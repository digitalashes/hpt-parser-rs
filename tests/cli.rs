#![cfg(feature = "parquet")]
#![expect(clippy::unwrap_used, reason = "test helpers")]

use hpt_parser::{Format, Parser};
use parquet::file::reader::{FileReader, SerializedFileReader};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TALL: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tall-3.0.0.csv");

fn hpt_parser(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hpt-parser")).args(args).output().unwrap()
}

/// A fresh directory for one test's files.
fn dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn rows(path: &Path) -> u64 {
    SerializedFileReader::new(File::open(path).unwrap())
        .unwrap()
        .metadata()
        .file_metadata()
        .num_rows()
        .try_into()
        .unwrap()
}

fn records(path: &str) -> u64 {
    Parser::new(File::open(path).unwrap(), Format::Csv).count() as u64
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap()
}

#[test]
fn usage() {
    for args in [&["--help"][..], &["-V"]] {
        assert_eq!(hpt_parser(args).status.code(), Some(0), "{args:?}");
    }
    for (args, message) in [
        (&["--bogus", TALL][..], "unknown option --bogus"),
        (&["-"], "standard input needs --format"),
        (&["--chunk-size", "x", TALL], "--chunk-size x: expected a whole number"),
        (&[TALL, "--metadata"], "--metadata needs a value"),
    ] {
        let out = hpt_parser(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(text(&out.stderr).starts_with(&format!("error: {message}")), "{args:?}");
    }
}

#[test]
fn parquet() {
    let out = dir("cli-parquet").join("tall.parquet");
    let run = hpt_parser(&[TALL, "-o", out.to_str().unwrap()]);
    assert_eq!(run.status.code(), Some(0));
    assert_eq!(rows(&out), records(TALL));
    assert_eq!(text(&run.stderr), format!("1 chunks, {} records\n", records(TALL)));
}

#[test]
fn chunks() {
    let dir = dir("cli-chunks");
    let run =
        hpt_parser(&[TALL, "--chunk-size=2", "--output", dir.join("t.parquet").to_str().unwrap()]);
    assert_eq!(run.status.code(), Some(0));
    let mut names: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    let n = records(TALL).div_ceil(2);
    let want: Vec<_> = (1..=n).map(|i| format!("t-{i:05}.parquet")).collect();
    assert_eq!(names, want);
    let total: u64 = want.iter().map(|name| rows(&dir.join(name))).sum();
    assert_eq!(total, records(TALL));
}

#[test]
fn metadata() {
    let json = hpt_parser(&["--metadata", "json", TALL]);
    assert_eq!(json.status.code(), Some(0));
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["hospital_name"], "North Valley Hospital");
    let table = hpt_parser(&["--metadata=table", TALL]);
    assert!(text(&table.stdout).contains("hospital_name          North Valley Hospital\n"));
    let config = hpt_parser(&["--print-config"]);
    assert_eq!(text(&config.stdout), include_str!("../src/default.toml"));
}

#[cfg(unix)]
mod notify {
    use super::{TALL, dir, records, text};
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::process::{Command, Stdio};

    /// The events the CLI sends for `input`, and its exit code.
    fn events(name: &str, input: &str) -> (Vec<serde_json::Value>, Option<i32>) {
        let dir = dir(name);
        let socket = dir.join("s.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let out = dir.join("t.parquet");
        let mut child = Command::new(env!("CARGO_BIN_EXE_hpt-parser"))
            .args([input, "--chunk-size", "2", "-o", out.to_str().unwrap(), "--notify-socket"])
            .arg(&socket)
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut lines = String::new();
        listener.accept().unwrap().0.read_to_string(&mut lines).unwrap();
        let code = child.wait().unwrap().code();
        (lines.lines().map(|l| serde_json::from_str(l).unwrap()).collect(), code)
    }

    #[test]
    fn chunks_then_done() {
        let (events, code) = events("cli-notify", TALL);
        assert_eq!(code, Some(0));
        let n = records(TALL).div_ceil(2);
        let (done, chunks) = events.split_last().unwrap();
        for (i, e) in (1..).zip(chunks) {
            assert_eq!(e["event"], "chunk_ready");
            assert_eq!(e["index"], i);
            assert!(e["path"].as_str().unwrap().ends_with(&format!("t-{i:05}.parquet")));
        }
        assert_eq!(chunks.len() as u64, n);
        assert_eq!(
            *done,
            serde_json::json!({"event": "done", "chunks": n, "records": records(TALL)})
        );
    }

    #[test]
    fn error() {
        let dir = dir("cli-notify-error-input");
        let broken = dir.join("broken.json");
        std::fs::write(&broken, r#"{"hospital_name": "#).unwrap();
        let (events, code) = events("cli-notify-error", broken.to_str().unwrap());
        assert_eq!(code, Some(1));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event"], "error");
        assert!(events[0]["message"].as_str().unwrap().starts_with("line 1"));
    }

    #[test]
    fn no_listener() {
        let socket = dir("cli-notify-none").join("s.sock");
        let out = super::hpt_parser(&[TALL, "--notify-socket", socket.to_str().unwrap()]);
        assert_eq!(out.status.code(), Some(1));
        assert!(text(&out.stderr).starts_with(&format!("error: {}: ", socket.display())));
    }
}
