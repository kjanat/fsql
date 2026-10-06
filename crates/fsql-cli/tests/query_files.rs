use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/queries/answer.fsql")
}

fn fsql() -> Command {
    Command::new(env!("CARGO_BIN_EXE_fsql"))
}

#[test]
fn explicit_and_positional_files_produce_the_same_output() {
    for option in [None, Some("--from-file"), Some("-F")] {
        let mut command = fsql();
        // Keep the existing short option for output format working.
        command.args(["-f", "json"]);
        if let Some(option) = option {
            command.arg(option);
        }
        let output = command.arg(fixture()).output().unwrap();
        assert!(output.status.success(), "{:?}", output);
        assert_eq!(output.stdout, b"{\"answer\":42}\n");
    }
}

#[test]
fn from_file_dash_reads_stdin() {
    let mut child = fsql()
        .args(["--from-file", "-", "--format", "json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(include_bytes!("queries/answer.fsql"))
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(output.stdout, b"{\"answer\":42}\n");
}

#[test]
fn explicit_and_positional_files_conflict() {
    let output = fsql()
        .arg("--from-file")
        .arg(fixture())
        .arg(fixture())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
    assert!(output.stdout.is_empty());
}

#[test]
fn missing_query_file_is_an_error() {
    let missing = fixture().with_file_name("missing.fsql");
    let output = fsql().arg("--from-file").arg(&missing).output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing.fsql"));
}
