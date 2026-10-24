//! Plugins through the Parquet writer: each hook, batches, errors, the sandbox. The plugin
//! is `tests/plugin/test.wasm`, built from `tests/plugin/src/lib.rs`.

#![cfg(feature = "plugins")]
#![expect(clippy::unwrap_used, reason = "test helpers")]

use hpt_parser::config::Config;
use hpt_parser::parquet::{METADATA_KEY, Output, Summary, write};
use hpt_parser::{Error, Format, Parser};
use parquet::file::reader::{FileReader, SerializedFileReader};
use std::fmt::Write;
use std::fs::{self, File};
use std::iter;
use std::path::{Path, PathBuf};

const WASM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/plugin/test.wasm");

/// `n` items, odd ones inpatient, every third without a payer.
fn csv(n: usize) -> String {
    let mut csv = "\
hospital_name,last_updated_on,version,hospital_address,license_number | NY
General Hospital,2026-01-01,2.2.0,1 Main St,12345
description,code | 1,code | 1 | type,setting,payer_name,plan_name,standard_charge | negotiated_dollar
"
    .to_string();
    for i in 1..=n {
        let setting = if i % 2 == 1 { "inpatient" } else { "outpatient" };
        let payer = if i % 3 == 0 { ",," } else { "Acme,PPO,10" };
        writeln!(csv, "Item {i},,,{setting},{payer}").unwrap();
    }
    csv
}

/// The test plugin as `t`, with `region = "us"`, then `rest`.
fn config(rest: &str) -> String {
    format!("[[plugin]]\nname = \"t\"\npath = \"{WASM}\"\nconfig = {{ region = \"us\" }}\n{rest}")
}

fn out(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir.join("out.parquet")
}

fn run(input: &str, toml: &str, path: &Path) -> Result<Summary, Error> {
    let config = Config::parse(toml).unwrap();
    write(Parser::new(input.as_bytes(), Format::Csv), &config, &Output::new(path), |_| {})
}

fn rows(path: &Path) -> Vec<String> {
    let reader = SerializedFileReader::new(File::open(path).unwrap()).unwrap();
    reader.get_row_iter(None).unwrap().map(|r| r.unwrap().to_string()).collect()
}

fn footer(path: &Path) -> serde_json::Value {
    let reader = SerializedFileReader::new(File::open(path).unwrap()).unwrap();
    let kv = reader.metadata().file_metadata().key_value_metadata().unwrap();
    let value = kv.iter().find(|kv| kv.key == METADATA_KEY).unwrap().value.as_ref();
    serde_json::from_str(value.unwrap()).unwrap()
}

/// The plugin's error message.
fn error(rest: &str) -> String {
    match run(&csv(3), &config(rest), &out("plugin-error")) {
        Err(Error::Plugin { plugin, message }) => format!("{plugin}: {message}"),
        other => format!("not a plugin error: {other:?}"),
    }
}

const DESCRIPTION: &str =
    "[[column]]\nname = \"s\"\nfrom = \"charge.description\"\ntype = \"string\"";

#[test]
fn hooks() {
    let path = out("plugin-hooks");
    let toml = config(&format!(
        "{DESCRIPTION}\ntransform = [{{ plugin = \"t.upper\" }}]
[[column]]
name = \"region\"
plugin = \"t.region\"
type = \"string\"
[[filter]]
plugin = \"t.keep_inpatient\"
[[metadata]]
plugin = \"t.add_fields\""
    ));
    let summary = run(&csv(5), &toml, &path).unwrap();
    assert_eq!(summary.records, 3);
    assert_eq!(
        rows(&path),
        [
            r#"{s: "ITEM 1", region: "us:Acme"}"#,
            r#"{s: "ITEM 3", region: null}"#,
            r#"{s: "ITEM 5", region: "us:Acme"}"#,
        ]
    );
    let extra = &footer(&path)["extra"];
    assert_eq!(
        *extra,
        serde_json::json!({"answer": "42", "hospital": "GENERAL HOSPITAL", "region": "us"})
    );
}

/// A hook gets only its `fields`: without `payer.name`, `region` sees no payer.
#[test]
fn fields() {
    let path = out("plugin-fields");
    let toml = config(
        "[[column]]\nname = \"region\"\nplugin = \"t.region\"\nfields = [\"charge.setting\"]
type = \"string\"\n[[filter]]\nplugin = \"t.keep_inpatient\"\nfields = \"charge.setting\"",
    );
    assert_eq!(run(&csv(5), &toml, &path).unwrap().records, 3);
    assert_eq!(rows(&path), ["{region: null}"; 3]);
}

#[test]
fn shapes() {
    // `upper` gives a list and an object back as they came.
    let path = out("plugin-shapes");
    let toml = config(
        "[[column]]\nname = \"m\"\nfrom = \"charge.modifiers\"\ntype = \"list\"
transform = [{ plugin = \"t.upper\" }]\n[[column]]\nname = \"c\"\nfrom = \"charge.codes\"
type = \"json\"\ntransform = [\"codes_by_type\", { plugin = \"t.upper\" }]",
    );
    let input = "a\nb\ndescription,code | 1,code | 1 | type,modifiers,payer_name,plan_name
Visit,99213,CPT,50|lt,,\n";
    run(input, &toml, &path).unwrap();
    assert_eq!(rows(&path), [r#"{m: ["50", "lt"], c: "{"CPT":["99213"]}"}"#]);
}

#[test]
fn batches_keep_order() {
    let path = out("plugin-batches");
    let toml = config(&format!(
        "{DESCRIPTION}\ntransform = [\"lower\", {{ plugin = \"t.upper\" }}, {{ map = {{ \"ITEM 1\" = \"first\" }} }}]
[[filter]]
plugin = \"t.keep_inpatient\""
    ));
    let summary = run(&csv(20_000), &toml, &path).unwrap();
    assert_eq!(summary.records, 10_000);
    let rows = rows(&path);
    assert_eq!(rows[0], r#"{s: "first"}"#);
    for (k, row) in rows.iter().enumerate().skip(1) {
        assert_eq!(*row, format!("{{s: \"ITEM {}\"}}", 2 * k + 1));
    }
}

/// A call gets up to 512 values. A JSON charge item's records stay in one: a call ends before
/// an item that does not fit, and an item with more records is a call of its own.
#[test]
fn call_sizes() {
    let path = out("plugin-sizes");
    let toml = config(&format!("{DESCRIPTION}\ntransform = [{{ plugin = \"t.count\" }}]"));
    let calls = |sizes: &[usize]| -> Vec<String> {
        sizes.iter().flat_map(|&n| vec![format!(r#"{{s: "{n}"}}"#); n]).collect()
    };
    run(&csv(1100), &toml, &path).unwrap();
    assert_eq!(rows(&path), calls(&[512, 512, 76]));

    let item = |charges: usize, payers: usize| {
        let payers = vec![r#"{"payer_name": "Acme"}"#; payers].join(",");
        let charge = format!(r#"{{"payers_information": [{payers}]}}"#);
        let charges = vec![charge; charges].join(",");
        format!(r#"{{"description": "d", "standard_charges": [{charges}]}}"#)
    };
    let items = (iter::repeat_n(item(2, 2), 128).chain([item(1, 600)]))
        .chain(iter::repeat_n(item(2, 2), 3))
        .collect::<Vec<_>>();
    let input = format!(r#"{{"standard_charge_information": [{}]}}"#, items.join(","));
    let config = Config::parse(&toml).unwrap();
    let parser = Parser::new(input.as_bytes(), Format::Json);
    write(parser, &config, &Output::new(&path), |_| {}).unwrap();
    assert_eq!(rows(&path), calls(&[512, 600, 12]));
}

#[test]
fn errors() {
    let column = |f: &str| format!("{DESCRIPTION}\ntransform = [{{ plugin = \"t.{f}\" }}]");
    assert_eq!(error(&column("nope")), "t.nope: the plugin has no such function");
    let fail = error(&column("fail"));
    assert!(fail.contains("this function always fails"), "{fail}");
    assert_eq!(error(&column("short")), "t.short: 2 items in `values` for 3 inputs");
    let missing = run(&csv(1), &config(&column("upper")).replace(WASM, "missing.wasm"), &out("p"));
    assert!(matches!(missing, Err(Error::Plugin { plugin, .. }) if plugin == "t"));
    let undeclared =
        Config::parse(&format!("{DESCRIPTION}\ntransform = [{{ plugin = \"x.upper\" }}]"));
    assert!(matches!(undeclared, Err(Error::Config { .. })));
}

#[test]
fn no_network() {
    let message = error(&format!("{DESCRIPTION}\ntransform = [{{ plugin = \"t.fetch\" }}]"));
    assert!(message.contains("https://example.org/ is not allowed"), "{message}");
}

#[test]
fn path_from_the_config_file() {
    let dir = out("plugin-path").parent().unwrap().to_path_buf();
    fs::write(dir.join("c.toml"), "[[plugin]]\nname = \"t\"\npath = \"t.wasm\"\n").unwrap();
    let config = Config::load(&dir.join("c.toml")).unwrap();
    assert_eq!(config.plugins[0].path, dir.join("t.wasm"));
}
