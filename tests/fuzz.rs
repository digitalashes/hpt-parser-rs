//! Reading never panics, nor do the config's transforms and casts or the Parquet writer on
//! what is read, on every fixture and reader case mutated at random. `fuzz/` runs the same
//! check under libFuzzer.

use hpt_parser::config::Config;
use hpt_parser::parquet::{Output, write};
use hpt_parser::{Error, Format, Parser};
use std::sync::LazyLock;
use std::{fs, process, thread};

/// Every transform and type on fields of each kind, and aliases of short names.
const CONFIG: &str = r#"
[aliases.csv]
description = ["d"]
payer_name = ["p"]
"standard_charge | negotiated_dollar" = ["standard_charge | d"]

[aliases.json]
description = ["d"]
code = ["c"]

[[column]]
name = "text"
from = ["charge.description", "payer.name", "payer.plan", "metadata.hospital_name"]
type = "string"
transform = ["trim", "upper", "lower", { map = { ip = "inpatient" } }, { default = "none" }]

[[column]]
name = "hash"
from = "payer.algorithm"
type = "string"
transform = ["hash"]

[[column]]
name = "money"
from = "charge.gross_charge"
type = "f64"
transform = ["money", { round = 2 }]

[[column]]
name = "round"
from = "payer.dollar"
type = "string"
transform = [{ round = 0 }]

[[column]]
name = "count"
from = "payer.count"
type = "i64"

[[column]]
name = "percentage"
from = "payer.percentage"
type = "i64"

[[column]]
name = "confirmed"
from = "metadata.attestation_confirmed"
type = "bool"

[[column]]
name = "updated"
from = "metadata.last_updated_on"
type = "date"

[[column]]
name = "notes"
from = "charge.notes"
type = "date"

[[column]]
name = "setting"
from = "charge.setting"
type = "string"
transform = [{ match = "setting", fuzzy = 0.8 }]

[[column]]
name = "methodology"
from = "payer.methodology"
type = "string"
transform = [{ match = "methodology", fuzzy = 0 }]

[[column]]
name = "billing_class"
from = "charge.billing_class"
type = "string"
transform = [{ match = "billing_class" }]

[[column]]
name = "drug_unit"
from = "charge.drug_unit_type"
type = "string"
transform = [{ match = "drug_unit", fuzzy = 1 }]

[[column]]
name = "code_type"
from = "charge.description"
type = "string"
transform = [{ match = "code_type", fuzzy = 0.5 }]

[[column]]
name = "codes"
from = "charge.codes"
type = "json"
transform = ["codes_by_type"]

[[column]]
name = "code_list"
from = "charge.codes"
type = "list"
transform = ["codes_list"]

[[column]]
name = "modifiers"
from = "charge.modifiers"
type = "list"

[[column]]
name = "split"
from = "payer.notes"
type = "list"
transform = [{ split = "|" }]

[[column]]
name = "json"
from = "charge.description"
type = "json"

[[column]]
name = "position"
from = "record.position"
type = "f64"

[[column]]
name = "npi"
from = "metadata.type_2_npi"
type = "json"

[[column]]
name = "address"
from = "metadata.hospital_address"
type = "list"
"#;

/// Reads `data`, takes each column's value of each record and writes the records in chunks
/// of 2 and row groups of 3, through the built-in config and through [`CONFIG`].
///
/// # Panics
///
/// When reading, a column's value or writing panics, or writing fails on more than the
/// parser's error.
pub fn check(format: Format, data: &[u8]) {
    static CONFIGS: LazyLock<[Config; 2]> =
        LazyLock::new(|| [Config::builtin(), Config::parse(CONFIG).expect("the config loads")]);
    let name = format!("hpt-parser-fuzz-{}-{:?}", process::id(), thread::current().id());
    let dir = std::env::temp_dir().join(name);
    fs::create_dir_all(&dir).expect("the temporary directory is writable");
    for config in &*CONFIGS {
        let parser = || Parser::new(data, format).with_aliases(config.aliases(format));
        let mut records = parser();
        while let Some(Ok(record)) = records.next() {
            for column in &config.columns {
                let _ = column.value(&record, records.metadata());
            }
        }
        let mut output = Output::new(dir.join("out.parquet"));
        output.chunk_size = 2;
        output.row_group_size = 3;
        let written =
            write(parser(), config, &output, |c| fs::remove_file(&c.path).expect("closed"));
        assert!(
            matches!(written, Ok(_) | Err(Error::Csv { .. } | Error::Json { .. })),
            "{written:?}"
        );
    }
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn mutated_cases() {
    use std::path::Path;

    // Bytes that delimit CSV and JSON, and one that is never UTF-8.
    const BYTES: &[u8] = b",\"\n\r|{}[]:0 \xff";
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut below = |n: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        usize::try_from(state % n as u64).expect("below a usize")
    };
    let mut checked = 0;
    for dir in ["tests/fixtures", "tests/reader"] {
        for entry in fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join(dir)).unwrap() {
            let path = entry.unwrap().path();
            let format = match path.extension().and_then(|e| e.to_str()) {
                Some("csv") => Format::Csv,
                _ => Format::Json,
            };
            let case = fs::read(&path).unwrap();
            for _ in 0..2 {
                let mut data = case.clone();
                for _ in 0..=below(4) {
                    let at = below(data.len() + 1);
                    let byte = BYTES[below(BYTES.len())];
                    match below(3) {
                        0 if at < data.len() => data[at] = byte,
                        1 => data.insert(at, byte),
                        _ => drop(data.drain(at..data.len().min(at + 1 + below(8)))),
                    }
                }
                check(format, &data);
                checked += 1;
            }
        }
    }
    assert!(checked > 0);
}
