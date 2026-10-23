//! The `hpt-parser` binary: usage errors, Parquet, .gz and .zip input, chunks, metadata,
//! notify events.

#![cfg(feature = "cli")]
#![expect(clippy::unwrap_used, reason = "test helpers")]

use hpt_parser::{Format, Parser};
use parquet::file::reader::{FileReader, SerializedFileReader};
use std::fs::{self, File};
use std::io::Write;
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
fn packed() {
    let dir = dir("cli-packed");
    let csv = fs::read(TALL).unwrap();
    let mut gz =
        flate2::write::GzEncoder::new(File::create(dir.join("t.csv.gz")).unwrap(), <_>::default());
    gz.write_all(&csv).unwrap();
    gz.finish().unwrap();
    let zip = |name: &str, files: &[&str]| {
        let mut zip = zip::ZipWriter::new(File::create(dir.join(name)).unwrap());
        for file in files {
            zip.start_file(*file, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(&csv).unwrap();
        }
        zip.finish().unwrap();
    };
    zip("t.zip", &["__MACOSX/._t.csv", "t.csv"]);
    zip("two.zip", &["t.csv", "u.json"]);
    for name in ["t.csv.gz", "t.zip"] {
        let _ = fs::remove_file(dir.join("t.parquet"));
        let run = hpt_parser(&[dir.join(name).to_str().unwrap()]);
        assert_eq!(text(&run.stderr), format!("1 chunks, {} records\n", records(TALL)), "{name}");
        assert_eq!(rows(&dir.join("t.parquet")), records(TALL), "{name}");
    }
    let two = dir.join("two.zip");
    let run = hpt_parser(&[two.to_str().unwrap()]);
    assert_eq!(run.status.code(), Some(1));
    let want = format!("error: {}: 2 .csv or .json files in it, not one\n", two.display());
    assert_eq!(text(&run.stderr), want);
}

#[test]
fn output_is_input() {
    let dir = dir("cli-output-is-input");
    let input = dir.join("t.csv");
    fs::copy(TALL, &input).unwrap();
    let again = dir.join("..").join("cli-output-is-input").join("t.csv");
    let out = hpt_parser(&[input.to_str().unwrap(), "-o", again.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(fs::read(&input).unwrap(), fs::read(TALL).unwrap());
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
    use std::io::{ErrorKind, Read};
    use std::os::unix::net::UnixListener;
    use std::process::{Command, Stdio};

    /// The events the CLI sends for `input`, and its exit code.
    fn events(name: &str, input: &str) -> (Vec<serde_json::Value>, Option<i32>) {
        let dir = dir(name);
        // A socket path has at most 104 bytes on macOS, which a deep checkout's target
        // directory passes.
        let socket = std::env::temp_dir().join(format!("hpt-{name}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let out = dir.join("t.parquet");
        let mut child = Command::new(env!("CARGO_BIN_EXE_hpt-parser"))
            .args([input, "--chunk-size", "2", "-o", out.to_str().unwrap(), "--notify-socket"])
            .arg(&socket)
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Polled, so that a CLI that exits without connecting fails the test, not hangs it.
        listener.set_nonblocking(true).unwrap();
        let mut stream = loop {
            let exited = child.try_wait().unwrap().is_some();
            match listener.accept() {
                Err(e) if e.kind() == ErrorKind::WouldBlock && !exited => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                accepted => break accepted.unwrap().0,
            }
        };
        stream.set_nonblocking(false).unwrap();
        let mut lines = String::new();
        stream.read_to_string(&mut lines).unwrap();
        let _ = std::fs::remove_file(&socket);
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
