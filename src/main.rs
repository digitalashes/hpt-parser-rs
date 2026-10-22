//! `hpt-parser`: a CMS Hospital Price Transparency file to Parquet, or its metadata to
//! standard output.

use hpt_parser::config::{self, BUILTIN, Config};
use hpt_parser::parquet::{Output, parse_compression, write};
use hpt_parser::{Format, Layout, Metadata, Parser};
use serde_json::json;
use std::error::Error;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;

const HELP: &str = "\
Usage: hpt-parser [OPTIONS] <INPUT>

Reads a CMS Hospital Price Transparency file (CSV tall or wide, JSON) and writes its
records to Parquet.

Arguments:
  <INPUT>                   the file to read, - for standard input

Options:
  -o, --output <PATH>       the Parquet file (default: <input stem>.parquet next to the
                            input, out.parquet for standard input)
  -c, --config <PATH>       the output config (default: the built-in one)
      --format <FORMAT>     csv or json (default: by the input's extension)
      --compression <CODEC> none, snappy, gzip[:level], lz4, zstd[:level] or
                            brotli[:level] (default: zstd:3)
      --chunk-size <N>      records per file, 0 for one file (default: 0); the files
                            are <stem>-00001.parquet, <stem>-00002.parquet, ...
      --row-group-size <N>  rows per row group, 0 for one per file (default: 256000)
      --metadata <FORMAT>   print the file's metadata as json or table, write nothing
      --notify-socket <PATH>
                            send one JSON line per event (chunk_ready, done, error) to
                            the unix socket someone listens on
      --print-config        print the built-in config, a start for your own
  -h, --help                print this help
  -V, --version             print the version

The flags override the config's [output], which overrides the defaults.

Exit codes: 0 done, 1 the input, config or output failed, 2 usage error.
";

/// What to read, where to write and how.
struct Args {
    input: OsString,
    format: Format,
    output: PathBuf,
    config: Option<PathBuf>,
    /// `[output]` as the flags set it.
    flags: config::Output,
    metadata: Option<Show>,
    notify: Option<PathBuf>,
}

#[derive(Clone, Copy)]
enum Show {
    Json,
    Table,
}

fn main() -> ExitCode {
    let args = match parse(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("error: {e}\n\nRun hpt-parser --help for usage.");
            return ExitCode::from(2);
        }
    };
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The arguments, or a usage error. `--help`, `--version` and `--print-config` print and
/// exit.
fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Args, String> {
    let (mut input, mut format, mut output, mut config) = (None, None, None, None);
    let (mut flags, mut metadata, mut notify) = (config::Output::default(), None, None);
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy().into_owned();
        if text == "-" || !text.starts_with('-') {
            if input.replace(arg).is_some() {
                return Err("more than one input".into());
            }
            continue;
        }
        let (name, mut inline) = match text.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(OsString::from(value))),
            _ => (text.as_str(), None),
        };
        let mut value =
            || inline.take().or_else(|| args.next()).ok_or(format!("{name} needs a value"));
        match name {
            "-h" | "--help" => exit(HELP),
            "-V" | "--version" => exit(concat!("hpt-parser ", env!("CARGO_PKG_VERSION"), "\n")),
            "--print-config" => exit(BUILTIN),
            "-o" | "--output" => output = Some(PathBuf::from(value()?)),
            "-c" | "--config" => config = Some(PathBuf::from(value()?)),
            "--format" => {
                format = Some(match value()?.to_string_lossy().as_ref() {
                    "csv" => Format::Csv,
                    "json" => Format::Json,
                    v => return Err(format!("--format {v}: expected csv or json")),
                });
            }
            "--compression" => {
                let v = value()?.to_string_lossy().into_owned();
                parse_compression(&v).map_err(|e| e.to_string())?;
                flags.compression = Some(v);
            }
            "--chunk-size" => flags.chunk_size = Some(number(name, &value()?)?),
            "--row-group-size" => flags.row_group_size = Some(number(name, &value()?)?),
            "--metadata" => {
                metadata = Some(match value()?.to_string_lossy().as_ref() {
                    "json" => Show::Json,
                    "table" => Show::Table,
                    v => return Err(format!("--metadata {v}: expected json or table")),
                });
            }
            "--notify-socket" if cfg!(unix) => notify = Some(PathBuf::from(value()?)),
            "--notify-socket" => return Err("--notify-socket works on unix only".into()),
            _ => return Err(format!("unknown option {text}")),
        }
    }
    let input = input.ok_or("missing <INPUT>")?;
    let stdin = input == "-";
    let format = match format {
        Some(format) => format,
        None if stdin => return Err("standard input needs --format".into()),
        None => match Path::new(&input).extension().map(std::ffi::OsStr::to_ascii_lowercase) {
            Some(e) if e == "csv" => Format::Csv,
            Some(e) if e == "json" => Format::Json,
            _ => return Err(format!("{}: neither .csv nor .json, give --format", input.display())),
        },
    };
    let output = output.unwrap_or_else(|| {
        if stdin { "out.parquet".into() } else { Path::new(&input).with_extension("parquet") }
    });
    if output == input {
        return Err("the output would overwrite the input, give another --output".into());
    }
    Ok(Args { input, format, output, config, flags, metadata, notify })
}

fn exit(text: &str) -> ! {
    print(text);
    std::process::exit(0)
}

fn number<T: FromStr>(name: &str, value: &OsString) -> Result<T, String> {
    let value = value.to_string_lossy();
    value.parse().map_err(|_| format!("{name} {value}: expected a whole number, 0 or more"))
}

fn run(args: &Args) -> Result<(), Box<dyn Error>> {
    let config = match &args.config {
        Some(path) => Config::load(path)?,
        None => Config::builtin(),
    };
    if let Some(show) = args.metadata {
        return metadata(args, &config, show);
    }
    let mut output = Output::new(&args.output);
    output.apply(&config.output)?;
    output.apply(&args.flags)?;
    let mut notify = Notify::connect(args.notify.as_deref())?;
    let result = open(args, &config).and_then(|parser| {
        let chunk_ready = |c: &hpt_parser::parquet::Chunk| {
            let path = c.path.display().to_string();
            notify.send(
                "chunk_ready",
                &json!({"path": path, "index": c.index, "records": c.records}),
            );
        };
        Ok(write(parser, &config, &output, chunk_ready)?)
    });
    let summary =
        result.inspect_err(|e| notify.send("error", &json!({"message": e.to_string()})))?;
    notify.send("done", &json!({"chunks": summary.chunks, "records": summary.records}));
    eprintln!("{} chunks, {} records", summary.chunks, summary.records);
    if summary.late_metadata {
        eprintln!(
            "warning: the file's metadata came after its first records, which have nulls or \
             stale values in the metadata.* columns; the last file's footer is complete"
        );
    }
    Ok(())
}

fn open(args: &Args, config: &Config) -> Result<Parser<Box<dyn Read>>, Box<dyn Error>> {
    let input: Box<dyn Read> = if args.input == "-" {
        Box::new(io::stdin().lock())
    } else {
        Box::new(File::open(&args.input).map_err(|e| format!("{}: {e}", args.input.display()))?)
    };
    Ok(Parser::new(input, args.format).with_aliases(config.aliases(args.format)))
}

fn metadata(args: &Args, config: &Config, show: Show) -> Result<(), Box<dyn Error>> {
    let mut parser = open(args, config)?;
    // A CSV file's metadata is complete after its header, a JSON file's may come after the
    // charge items.
    let records = if args.format == Format::Csv { 1 } else { usize::MAX };
    for record in parser.by_ref().take(records) {
        record?;
    }
    let m = parser.metadata();
    print(&match show {
        Show::Json => serde_json::to_string_pretty(m).expect("strings, lists and maps") + "\n",
        Show::Table => table(m),
    });
    Ok(())
}

/// `name  value` lines, lists joined by `, `, empty fields left out.
fn table(m: &Metadata) -> String {
    let one = |v: &Option<String>| v.clone().unwrap_or_default();
    let mut rows = vec![
        ("layout", m.layout.map(Layout::name).unwrap_or_default().to_string()),
        ("version", one(&m.version)),
        ("hospital_name", one(&m.hospital_name)),
        ("last_updated_on", one(&m.last_updated_on)),
        ("location_name", m.location_name.join(", ")),
        ("hospital_address", m.hospital_address.join(", ")),
        ("license_number", one(&m.license_number)),
        ("license_state", one(&m.license_state)),
        ("type_2_npi", m.type_2_npi.join(", ")),
        ("attestation", one(&m.attestation)),
        ("attestation_confirmed", one(&m.attestation_confirmed)),
        ("attester_name", one(&m.attester_name)),
    ];
    let modifier = |x: &hpt_parser::ModifierInfo| {
        ("modifier", format!("{}: {}", one(&x.code), one(&x.description)))
    };
    rows.extend(m.modifiers.iter().map(modifier));
    rows.extend(m.aliases_used.iter().map(|(alias, name)| ("alias", format!("{alias}: {name}"))));
    rows.extend(m.extra.iter().map(|(key, value)| ("extra", format!("{key}: {value}"))));
    rows.retain(|(_, value)| !value.is_empty());
    let width = rows.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    rows.iter().fold(String::new(), |mut text, (name, value)| {
        let _ = writeln!(text, "{name:width$}  {value}");
        text
    })
}

/// Writes to standard output; a closed pipe is not an error worth reporting.
fn print(text: &str) {
    let _ = io::stdout().lock().write_all(text.as_bytes());
}

/// Events for whoever listens on `--notify-socket`, one JSON line each.
struct Notify(Option<Box<dyn Write>>);

impl Notify {
    fn connect(path: Option<&Path>) -> Result<Self, String> {
        let Some(path) = path else { return Ok(Notify(None)) };
        let socket = connect(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Notify(Some(socket)))
    }

    /// `{"event":"<event>",<fields>}`, the event first for whoever reads the lines. A failed
    /// write drops the socket: one warning, and parsing goes on.
    fn send(&mut self, event: &str, fields: &serde_json::Value) {
        let line = format!("{{\"event\":\"{event}\",{}\n", &fields.to_string()[1..]);
        if let Some(socket) = &mut self.0
            && let Err(e) = socket.write_all(line.as_bytes())
        {
            eprintln!("warning: --notify-socket: {e}; no more events are sent");
            self.0 = None;
        }
    }
}

#[cfg(unix)]
fn connect(path: &Path) -> io::Result<Box<dyn Write>> {
    Ok(Box::new(std::os::unix::net::UnixStream::connect(path)?))
}

/// Refused as an argument before it gets here.
#[cfg(not(unix))]
fn connect(_: &Path) -> io::Result<Box<dyn Write>> {
    Err(io::ErrorKind::Unsupported.into())
}
