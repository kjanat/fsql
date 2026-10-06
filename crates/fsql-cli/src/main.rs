use std::io::{self, BufWriter, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use fsql::error::Error;
use fsql::eval::format_size;
use fsql::exec;
use fsql::journal::{self, Journal};
use fsql::mutate::{self, Outcome, Resolved};
use fsql::output::{self, Format};
use fsql::plan::{Plan, Planner};
use fsql::walk::{self, WalkOptions};

#[derive(Parser)]
#[command(name = "fsql", version, about = "SQL over the filesystem")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// SQL to run; repeatable
    #[arg(short = 'e', long = "execute", value_name = "SQL")]
    execute: Vec<String>,

    /// Read SQL from a file, or `-` for stdin
    #[arg(value_name = "FILE")]
    file: Option<PathBuf>,

    /// Directory the `files` table starts from
    #[arg(short = 'C', long, value_name = "DIR")]
    root: Option<PathBuf>,

    /// Output format: table, csv, tsv, json, lines
    #[arg(short, long, default_value = "table")]
    format: String,

    /// Do not descend deeper than this many levels
    #[arg(short = 'd', long, value_name = "N")]
    max_depth: Option<u32>,

    /// Stay on the root's filesystem
    #[arg(short = 'x', long)]
    one_file_system: bool,

    /// Commit DELETE, UPDATE and INSERT statements instead of previewing them
    #[arg(long)]
    apply: bool,

    /// Refuse to apply a statement touching more than this many entries; 0 disables
    #[arg(long, default_value_t = 10_000, value_name = "N")]
    cap: usize,

    /// Apply without tombstones or before-images; undo becomes impossible
    #[arg(long)]
    no_journal: bool,

    /// Where journals live; defaults to $XDG_DATA_HOME/fsql/journal
    #[arg(long, value_name = "DIR")]
    journal_dir: Option<PathBuf>,

    /// Entries shown in a preview
    #[arg(long, default_value_t = 20, value_name = "N")]
    preview: usize,

    /// Return partial SELECT results when filesystem entries cannot be read
    #[arg(long)]
    best_effort: bool,

    /// Cumulative row allocation budget for query execution
    #[arg(long, default_value_t = 1_000_000)]
    max_rows: usize,

    /// Estimated value allocation budget in bytes
    #[arg(long, default_value_t = 268_435_456)]
    max_bytes: usize,

    /// Maximum input rows and candidate join pairs examined
    #[arg(long, default_value_t = 10_000_000)]
    max_work: usize,

    /// Stop execution after this many seconds (between filesystem calls)
    #[arg(long)]
    timeout: Option<u64>,

    /// Print counts only
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Reverse a previously applied statement
    Undo {
        /// Journal id printed when the statement was applied
        id: String,
    },
    /// List applied statements that can be undone
    Journal,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Err(error) = walk::install_governor() {
        eprintln!("fsql: {error}");
        return ExitCode::from(2);
    }
    match run(cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("fsql: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode, Error> {
    let base = cli
        .journal_dir
        .clone()
        .unwrap_or_else(journal::default_base);
    match &cli.command {
        Some(Command::Undo { id }) => {
            let outcome = mutate::undo(&base, id)?;
            report_failures(&outcome);
            println!(
                "UNDO {id}: reverted {} of {} recorded operations",
                outcome.applied,
                outcome.applied + outcome.failures.len()
            );
            return Ok(exit_for(&outcome));
        }
        Some(Command::Journal) => {
            for summary in journal::list(&base)? {
                let first = summary.statement.lines().next().unwrap_or("").trim();
                println!(
                    "{}  {:>6}  {}{}",
                    summary.id,
                    summary.records,
                    first,
                    if summary.recovery_required {
                        " [RECOVERY REQUIRED]"
                    } else {
                        ""
                    }
                );
            }
            return Ok(ExitCode::SUCCESS);
        }
        None => {}
    }
    let format = Format::parse(&cli.format)
        .ok_or_else(|| Error::Plan(format!("unknown format `{}`", cli.format)))?;
    let root = match &cli.root {
        Some(root) => root.clone(),
        None => std::env::current_dir().map_err(|source| Error::Io {
            path: PathBuf::from("."),
            source,
        })?,
    };
    let planner = Planner::new(
        root,
        WalkOptions {
            max_depth: cli.max_depth,
            one_filesystem: cli.one_file_system,
            initial_depth: 0,
        },
    );
    let mut scripts: Vec<String> = cli.execute.clone();
    match &cli.file {
        Some(path) if path.as_os_str() == "-" => scripts.push(read_stdin()?),
        Some(path) => scripts.push(std::fs::read_to_string(path).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?),
        None if scripts.is_empty() => scripts.push(read_stdin()?),
        None => {}
    }
    let mut worst = ExitCode::SUCCESS;
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    for script in scripts {
        for plan in planner.plan(&script)? {
            let code = match plan {
                Plan::Select(select) => {
                    let io_error = |source| Error::Io {
                        path: PathBuf::from("<stdout>"),
                        source,
                    };
                    let completion = if matches!(format, Format::Json | Format::Lines) {
                        exec::stream(
                            &select,
                            &planner,
                            execution_options(&cli),
                            &mut |headers, row| {
                                output::write_row(&mut out, headers, row, format)
                                    .map_err(io_error)?;
                                out.flush().map_err(io_error)?;
                                Ok(std::ops::ControlFlow::Continue(()))
                            },
                        )?
                    } else {
                        let (set, completion) =
                            exec::run_with_options(&select, &planner, execution_options(&cli))?;
                        output::write(&mut out, &set, format).map_err(io_error)?;
                        out.flush().map_err(io_error)?;
                        completion
                    };
                    let code = if completion.is_complete() {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::from(1)
                    };
                    for error in completion.diagnostics {
                        report_error(error);
                    }
                    code
                }
                mutation => mutate_plan(&cli, &base, &planner, &mutation, &script, &mut out)?,
            };
            if code != ExitCode::SUCCESS {
                worst = code;
            }
        }
    }
    out.flush().ok();
    Ok(worst)
}

fn execution_options(cli: &Cli) -> fsql::ExecutionOptions {
    fsql::ExecutionOptions {
        error_policy: if cli.best_effort {
            fsql::ErrorPolicy::BestEffort
        } else {
            fsql::ErrorPolicy::Strict
        },
        max_rows: cli.max_rows,
        max_bytes: cli.max_bytes,
        max_work: cli.max_work,
        timeout: cli.timeout.map(std::time::Duration::from_secs),
        ..fsql::ExecutionOptions::default()
    }
}

fn read_stdin() -> Result<String, Error> {
    let mut text = String::new();
    io::stdin()
        .read_to_string(&mut text)
        .map_err(|source| Error::Io {
            path: PathBuf::from("<stdin>"),
            source,
        })?;
    Ok(text)
}

fn report_error(error: Error) {
    eprintln!("fsql: {error}");
}

fn report_failures(outcome: &Outcome) {
    for path in &outcome.partial {
        eprintln!("fsql: {} was partially changed", path.display());
    }
    for path in &outcome.recovery_required {
        eprintln!(
            "fsql: {} requires recovery reconciliation; pending intents were retained",
            path.display()
        );
    }
    for (path, error) in &outcome.failures {
        eprintln!("fsql: {}: {error}", path.display());
    }
}

fn exit_for(outcome: &Outcome) -> ExitCode {
    if outcome.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn mutate_plan(
    cli: &Cli,
    base: &std::path::Path,
    planner: &Planner,
    plan: &Plan,
    script: &str,
    out: &mut dyn Write,
) -> Result<ExitCode, Error> {
    let resolved =
        mutate::resolve_with_options(plan, planner, execution_options(cli), &mut report_error)?;
    let verb = resolved.verb();
    let count = resolved.len();
    let bytes = resolved.bytes();
    if !cli.apply {
        preview(cli, &resolved, out)?;
        writeln!(
            out,
            "DRY RUN, nothing changed. Re-run with --apply to commit."
        )
        .ok();
        return Ok(ExitCode::SUCCESS);
    }
    if count == 0 {
        writeln!(out, "{verb} matched nothing.").ok();
        return Ok(ExitCode::SUCCESS);
    }
    if cli.cap != 0 && count > cli.cap {
        return Err(Error::Plan(format!(
            "{verb} would touch {count} entries, above the cap of {}; raise --cap or narrow the WHERE clause",
            cli.cap
        )));
    }
    let mut journal = if cli.no_journal {
        None
    } else {
        Some(Journal::open(base, script)?)
    };
    let outcome = mutate::apply(&resolved, journal.as_mut())?;
    report_failures(&outcome);
    let size = if bytes > 0 {
        format!(
            " ({})",
            format_size(i64::try_from(bytes).unwrap_or(i64::MAX))
        )
    } else {
        String::new()
    };
    match (&journal, outcome.failures.is_empty()) {
        (Some(journal), true) => writeln!(
            out,
            "{verb} affected {} entries{size}. Journal {}; undo with: fsql undo {}",
            outcome.applied,
            journal.id(),
            journal.id()
        ),
        (Some(journal), false) => writeln!(
            out,
            "{verb} affected {} of {count} entries{size}, {} failed. Journal {}; undo with: fsql undo {}",
            outcome.applied,
            outcome.failures.len(),
            journal.id(),
            journal.id()
        ),
        (None, true) => writeln!(out, "{verb} affected {} entries{size}. No journal, no undo.", outcome.applied),
        (None, false) => writeln!(
            out,
            "{verb} affected {} of {count} entries{size}, {} failed. No journal, no undo.",
            outcome.applied,
            outcome.failures.len()
        ),
    }
    .ok();
    Ok(exit_for(&outcome))
}

fn preview(cli: &Cli, resolved: &Resolved, out: &mut dyn Write) -> Result<(), Error> {
    let verb = resolved.verb();
    let count = resolved.len();
    let bytes = resolved.bytes();
    let size = if bytes > 0 {
        format!(
            " ({})",
            format_size(i64::try_from(bytes).unwrap_or(i64::MAX))
        )
    } else {
        String::new()
    };
    writeln!(out, "{verb} would affect {count} entries{size}:").ok();
    if cli.quiet {
        return Ok(());
    }
    let paths = resolved.paths();
    for (path, size) in paths.iter().take(cli.preview) {
        if *size > 0 {
            writeln!(
                out,
                "  {}  {}",
                path.display(),
                format_size(i64::try_from(*size).unwrap_or(i64::MAX))
            )
            .ok();
        } else {
            writeln!(out, "  {}", path.display()).ok();
        }
    }
    if paths.len() > cli.preview {
        writeln!(out, "  ... and {} more", paths.len() - cli.preview).ok();
    }
    Ok(())
}
