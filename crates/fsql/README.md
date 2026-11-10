# fsql

[![Crates.io](https://img.shields.io/crates/v/fsql?logo=rust&labelColor=B7410E&color=black)][crates] [![License: MIT](https://img.shields.io/crates/l/fsql?color=blue)](LICENSE)

`fsql` runs SQL against the filesystem, from the `fsql` command or from Rust. `SELECT` reads directory entries, mounts, extended attributes and POSIX ACLs. `DELETE`, `UPDATE` and `INSERT` change files, and a journal records each applied statement so it can be undone.

## Command

```sh
cargo install fsql
```

```sh
fsql -e "select path, human(size) from files where size > 1g order by size desc limit 20"
fsql -C ~/projects -e "select ext, count(*), sum(size) from files group by ext order by 3 desc"
fsql -f json -e "select path, mtime from files where ext = 'log'"
```

`-C DIR` sets the directory the `files` table starts from. The default is the current directory. `-f` selects `table`, `csv`, `tsv`, `json` or `lines`. `fsql FILE`, `fsql -F FILE` and `fsql -` read statements from a file or from stdin, and `-e` can be passed more than once.

```sh
fsql -e "delete from files where ext = 'tmp' and mtime < now() - interval '30' day"
fsql --apply -e "delete from files where ext = 'tmp' and mtime < now() - interval '30' day"
fsql journal    # list statements that can be undone
fsql undo ID    # reverse one
fsql recover ID # finish an interrupted apply or undo
```

Without `--apply`, `DELETE`, `UPDATE` and `INSERT` print the entries they would change and change nothing. With `--apply`, fsql refuses a statement that touches more than `--cap` entries (10000 by default) and journals the rest under `$XDG_DATA_HOME/fsql/journal`, or `%LOCALAPPDATA%\fsql\journal` on Windows. `--no-journal` applies without a journal, and fsql cannot undo the result.

`--max-rows`, `--max-bytes`, `--max-work` and `--timeout SECONDS` bound a query. `--best-effort` returns the readable rows of a `SELECT` when some entries cannot be read, and exits with status 1 when it skipped entries.

## Library

The command is behind the default `cli` feature. A library dependency can leave it out, along with `clap`:

```toml
[dependencies]
fsql = { version = "0.1", default-features = false }
```

```rust
use std::ops::ControlFlow;

use fsql::walk::WalkOptions;
use fsql::{Engine, ErrorPolicy};

fn main() -> fsql::Result<()> {
    let mut engine = Engine::new(".", WalkOptions::default());
    engine.execution.error_policy = ErrorPolicy::BestEffort;
    let query = engine.prepare_query("select path from files where ext = 'rs'")?;
    let completion = query.stream(&mut |columns, row| {
        println!("{columns:?}: {row:?}");
        Ok(ControlFlow::Continue(()))
    })?;
    assert!(completion.is_complete(), "{:?}", completion.diagnostics);
    Ok(())
}
```

`Engine::prepare_query` validates names, function arities and grouping before any scan. `PreparedQuery::stream` passes rows to a callback, and the callback can return `ControlFlow::Break(())` to stop. `PreparedQuery::collect` returns the whole result set. Both report a `Completion` that lists skipped entries under `ErrorPolicy::BestEffort`.

`ExecutionOptions` holds the row, byte and work budgets, the timeout and a clonable `CancellationToken`.

`Engine::resolve_mutation` walks the tree, evaluates the predicate and returns a `ResolvedMutation` that lists every matching entry with its device, inode and ctime. `ResolvedMutation::apply(journal_base)` checks each entry's identity again before changing it and writes a journal. `apply_without_journal` skips the journal, and its result cannot be undone. `fsql::mutate::undo` and `fsql::mutate::recover` reverse a journaled statement or finish an interrupted one.

The [fsql repository](https://github.com/kjanat/fsql#readme) documents the tables, columns, functions and mutation rules. The [examples](https://github.com/kjanat/fsql/tree/master/crates/fsql/examples) include complete programs that collect results, stream rows, and apply and undo a mutation.

## License

MIT. See [`LICENSE`](LICENSE).

[crates]: https://crates.io/crates/fsql
