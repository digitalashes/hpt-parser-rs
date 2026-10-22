//! Listens on a unix socket for the events of one `hpt-parser --notify-socket` run and
//! prints them: `cargo run --example listen -- hpt.sock`, then the run with
//! `--notify-socket hpt.sock`. A `chunk_ready` file is complete when its event comes, the
//! place to load, move or queue it. Exits 0 on `done`, 1 on `error` or when the run ends
//! without either.

#[cfg(unix)]
fn main() -> std::io::Result<()> {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;

    let path = std::env::args().nth(1).unwrap_or_else(|| "hpt.sock".into());
    let _ = std::fs::remove_file(&path); // left by an earlier run
    let listener = UnixListener::bind(&path)?;
    let (stream, _) = listener.accept()?;
    std::fs::remove_file(&path)?;
    for line in BufReader::new(stream).lines() {
        let event: serde_json::Value = serde_json::from_str(&line?)?;
        let text = |key: &str| event[key].as_str().unwrap_or_default().to_string();
        match event["event"].as_str() {
            Some("chunk_ready") => {
                println!(
                    "chunk {}: {} ({} records)",
                    event["index"],
                    text("path"),
                    event["records"]
                );
            }
            Some("done") => {
                println!("done: {} chunks, {} records", event["chunks"], event["records"]);
                return Ok(());
            }
            Some("error") => {
                eprintln!("error: {}", text("message"));
                std::process::exit(1);
            }
            _ => {} // an event added later
        }
    }
    eprintln!("the run ended without done or error");
    std::process::exit(1);
}

#[cfg(not(unix))]
fn main() {
    eprintln!("--notify-socket is unix only");
}
