# hpt-parser

[![CI](https://github.com/digitalashes/hpt-parser-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/digitalashes/hpt-parser-rs/actions/workflows/ci.yml)

A streaming parser for CMS Hospital Price Transparency machine-readable files, and a
command line that turns them into Parquet. It reads CSV files (tall and wide) and JSON
files of data dictionary versions 2.0.0 to 3.0.0, and gives every one of them the same
flat shape: one record per charge item and payer/plan.

> **Unofficial.** This project is not affiliated with, endorsed by, or maintained by the
> Centers for Medicare & Medicaid Services.

- **One shape for every file.** A tall row is one record, a wide row is one record per
  payer/plan block, a JSON standard charge is one record per payer. The file's header
  (hospital, version, attestation, ...) is kept apart, as `Metadata`.
- **Streaming.** Records come out as the file is read, so memory stays flat: a 2.5 GB
  wide CSV file (2.2 million records) becomes Parquet in 9 seconds with 68 MB of resident
  memory on a 2019 laptop (Intel i7-9750H).
- **Lenient.** Hospital files are messy. A value is kept as the text in the file, and a
  value that does not parse never fails the run. Only a file that cannot be read at all
  is an error: IO, no CSV header, broken JSON syntax.
- **Your columns.** A TOML config picks the output columns, their names, order and types,
  and cleans the values on the way: money, case, CMS value domains, code grouping, ids.
- **Made for pipelines.** Chunked output, stable exit codes, metadata as JSON, and events
  on a unix socket when each chunk file is ready.

It does not validate. To check that a file follows the data dictionary, use
[`hpt-validator`](https://github.com/digitalashes/hpt-validator-rs) or its command line,
[`hpt-validator-cli`](https://github.com/digitalashes/hpt-validator-cli-rs).

## Install

```sh
cargo install --git https://github.com/digitalashes/hpt-parser-rs
```

The binary is `hpt-parser`. It needs Rust 1.96 or later.

## Command line

```sh
# 123456789_hospital_standardcharges.parquet next to the input, every field a column
hpt-parser 123456789_hospital_standardcharges.csv

# your own columns, a file per 5 million records
hpt-parser standardcharges.json -c columns.toml -o out/charges.parquet --chunk-size 5000000

# straight from the web
curl -s https://example.org/standardcharges.csv | hpt-parser - --format csv -o charges.parquet

# only the header, for scripts or for people
hpt-parser standardcharges.json --metadata json
hpt-parser standardcharges.csv --metadata table
```

| Option                     | Meaning                                                                                      |
|----------------------------|----------------------------------------------------------------------------------------------|
| `<INPUT>`                  | The file to read, or `-` for standard input.                                                 |
| `-o, --output <PATH>`      | The Parquet file. Default: the input's name with `.parquet`, `out.parquet` for standard input. |
| `-c, --config <PATH>`      | The output config. Default: the built-in one, every field as a string column.                |
| `--format csv\|json`       | The input format. Default: by the extension; required for standard input.                    |
| `--compression <CODEC>`    | `none`, `snappy`, `gzip[:level]`, `lz4`, `zstd[:level]` or `brotli[:level]`. Default `zstd:3`. |
| `--chunk-size <N>`         | Records per file, `0` for one file (the default). Files are `<stem>-00001.parquet`, ...      |
| `--row-group-size <N>`     | Rows per row group, `0` for one per file. Default 256000.                                    |
| `--metadata json\|table`   | Print the file's metadata and write nothing.                                                 |
| `--notify-socket <PATH>`   | Send events to a unix socket (below).                                                        |
| `--print-config`           | Print the built-in config, a start for your own.                                             |
| `-h, --help`, `-V, --version` | Help and version.                                                                         |

Options override the config's `[output]` table, which overrides the defaults. The run
prints `<chunks> chunks, <records> records` to standard error.

| Exit code | Meaning                                                   |
|-----------|-----------------------------------------------------------|
| 0         | Done.                                                     |
| 1         | The input, the config or the output failed: `error: ...`. |
| 2         | Usage error.                                              |

`--metadata table` lines up the header for a person:

```text
layout                 tall
version                3.0.0
hospital_name          North Valley Hospital
last_updated_on        2026-01-01
location_name          North Valley Hospital, North Valley Clinic
hospital_address       5 Valley Way, Troy, NY 12180
license_number         PFI-1234
license_state          NY
type_2_npi             1982621447, 1154343150
attestation_confirmed  true
attester_name          Jane Roe
```

For a CSV file only the first rows are read. A JSON file may put its header after the
charge items, so it is read to the end.

### Events

With `--notify-socket <PATH>` the command connects to a unix socket that another program
listens on and writes one JSON line per event:

```json
{"event":"chunk_ready","path":"out/charges-00001.parquet","index":1,"records":5000000}
{"event":"chunk_ready","path":"out/charges-00002.parquet","index":2,"records":1240117}
{"event":"done","chunks":2,"records":6240117}
```

A parse that fails sends `{"event":"error","message":"..."}` instead of `done`. A
`chunk_ready` file is closed and complete, so a consumer can load it while the rest is
still being parsed. No listener at the start is an error (exit 1); a write that fails
later prints one warning and the run goes on.

## Output config

`hpt-parser --print-config` prints the built-in config. A config is a list of columns:

```toml
[output]
compression = "zstd:9"
chunk_size = 5000000

[[column]]
name = "hospital"
from = "metadata.hospital_name"
type = "string"

[[column]]
name = "codes"
from = "charge.codes"
type = "json"
transform = ["codes_by_type"]          # {"CPT":["99213"],"MS-DRG":["470"]}

[[column]]
name = "setting"
from = "charge.setting"
type = "string"
transform = [{ match = "setting", fuzzy = 0.8 }]   # "Out-patient" reads as "outpatient"

[[column]]
name = "negotiated"
from = "payer.dollar"
type = "f64"
transform = ["money", { round = 2 }]   # "$1,204.50 " reads as 1204.5

[[column]]
name = "updated"
from = "metadata.last_updated_on"
type = "date"

[[column]]
name = "id"
from = ["metadata.hospital_name", "charge.description", "payer.name", "payer.plan"]
type = "string"
transform = ["hash"]
```

Each column reads one field, or several whose texts are joined by `|`. The transforms
run in order, then the value is cast to the column's type; a value that does not cast is
null.

**Fields.**

| Group          | Fields |
|----------------|--------|
| `charge.*`     | `description`, `codes`, `setting`, `billing_class`, `drug_unit`, `drug_unit_type`, `modifiers`, `gross_charge`, `discounted_cash`, `minimum`, `maximum`, `notes` |
| `payer.*`      | `name`, `plan`, `dollar`, `percentage`, `algorithm`, `methodology`, `estimated_amount`, `median_amount`, `percentile_10th`, `percentile_90th`, `count`, `notes` |
| `metadata.*`   | `layout`, `version`, `hospital_name`, `last_updated_on`, `location_name`, `hospital_address`, `license_number`, `license_state`, `type_2_npi`, `attestation`, `attestation_confirmed`, `attester_name`, `extra.<key>` |
| `record.*`     | `position`: the CSV line the row starts on, or the JSON item's index |

**Types.**

| Type     | Parquet                 | Reads                                      |
|----------|-------------------------|--------------------------------------------|
| `string` | `BYTE_ARRAY (STRING)`   | The text.                                  |
| `f64`    | `DOUBLE`                | A number.                                  |
| `i64`    | `INT64`                 | A whole number.                            |
| `bool`   | `BOOLEAN`               | `true`, `yes`, `y`, `1` or `false`, `no`, `n`, `0`, any case. |
| `date`   | `INT32 (DATE)`          | `YYYY-MM-DD` or `M/D/YYYY`.                |
| `json`   | `BYTE_ARRAY (JSON)`     | Codes and lists as JSON arrays, other text as a JSON string. |
| `list`   | `LIST` of strings       | A list field, or text after `split`.       |

Every column is optional (nullable).

**Transforms.**

| Transform                           | Does |
|-------------------------------------|------|
| `"trim"`, `"upper"`, `"lower"`      | Whitespace and case. |
| `"money"`                           | Drops `$`, `,` and spaces. |
| `{ round = N }`                     | Rounds a number to `N` digits. |
| `{ map = { ip = "inpatient" } }`    | Replaces a value: exact match first, then ignoring case. |
| `{ match = "<domain>", fuzzy = 0.8 }` | Snaps a value to the CMS spelling of `setting`, `methodology`, `code_type`, `drug_unit` or `billing_class`; `fuzzy` is the least similarity for a near miss. |
| `{ split = "\|" }`                  | Text to a list. |
| `{ default = "x" }`                 | For a null or empty value. |
| `"codes_by_type"`                   | Codes as `{"TYPE":["code",...]}`, sorted. |
| `"codes_list"`                      | Codes as a list of `TYPE:code`, sorted. |
| `"hash"`                            | A 128-bit FNV-1a hash of the text, as hex: a stable id. |
| `{ plugin = "name.function" }`      | A plugin function (below). |

**Aliases.** Some files use their own names for CMS columns. `[aliases.csv]` and
`[aliases.json]` map a CMS name to the names read as it; the ones a file used end up in
the metadata as `aliases_used`.

```toml
[aliases.csv]
"standard_charge | gross" = ["gross_charges", "gross charge"]

[aliases.json]
modifier_code = ["modifiers"]
```

### Plugins

What the built-in transforms cannot do, a plugin can: a WASM module made with
[Extism](https://extism.org), in any language with an Extism PDK. It runs sandboxed, with
no files, no network and no WASI, 60 seconds per call and 256 MiB of memory.

```toml
[[plugin]]
name = "acme"
path = "acme.wasm"            # relative to the config file
config = { region = "us" }    # read by the plugin as Extism config

[[column]]
name = "payer"
from = "payer.name"
type = "string"
transform = ["trim", { plugin = "acme.normalize" }]   # a value transform

[[column]]
name = "region"
plugin = "acme.region"        # a computed column
type = "string"

[[filter]]
plugin = "acme.keep"          # keeps or drops records

[[metadata]]
plugin = "acme.source"        # adds fields to the metadata
```

| Hook            | Gets                  | Answers                       |
|-----------------|-----------------------|-------------------------------|
| value transform | `{"values":[...]}`    | `{"values":[...]}`            |
| computed column | `{"records":[...]}`   | `{"values":[...]}`            |
| filter          | `{"records":[...]}`   | `{"keep":[true,false,...]}`   |
| metadata        | the metadata object   | `{"fields":{"key":"value"}}`  |

- Calls are batched, up to 8192 records or values each, and an answer has one item per
  input, in order.
- A record is flat, keyed by field name, with the fields that have a value:
  `{"charge.description":"MRI brain","charge.codes":[{"code":"70551","type":"CPT"}],"payer.name":"Acme",...}`.
  `metadata.*` fields are not in it.
- A value is a string, a list of strings, the codes as above, or null. An answer's
  strings, lists and numbers go on through the rest of the transforms and the cast.
- The `[[metadata]]` hooks run once after the last record; their fields go to `extra`
  in the last file's footer.
- A plugin that fails, or answers in the wrong shape or with the wrong number of items,
  stops the run (exit 1).
- The JSON costs time: on the 2.5 GB file above, a filter over every record and a
  computed column took the run from 7.6 to 18 seconds. Read only the fields you use
  (typed structs, not a generic JSON value: that took 43 seconds).

A filter in Rust, with `extism-pdk` and `serde` (derive) as dependencies and
`crate-type = ["cdylib"]`:

```rust,ignore
use extism_pdk::{FnResult, Json, plugin_fn};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct Records {
    records: Vec<Record>,
}

#[derive(Deserialize)]
struct Record {
    #[serde(rename = "charge.setting")]
    setting: Option<String>,
}

#[derive(Serialize)]
struct Keep {
    keep: Vec<bool>,
}

#[plugin_fn]
pub fn keep(Json(input): Json<Records>) -> FnResult<Json<Keep>> {
    let keep = input.records.iter().map(|r| r.setting.as_deref() == Some("inpatient"));
    Ok(Json(Keep { keep: keep.collect() }))
}
```

```sh
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown   # target/wasm32-unknown-unknown/release/<name>.wasm
```

[`tests/plugin/src/lib.rs`](tests/plugin/src/lib.rs) has a function for each hook.

### What is in a Parquet file

One row per record, the config's columns in its order, and the file's metadata as JSON in
the footer under the key `hpt.metadata`, in every chunk:

```sql
-- DuckDB
SELECT value FROM parquet_kv_metadata('charges.parquet') WHERE key = 'hpt.metadata';
```

```python
# pyarrow
pq.read_metadata("charges.parquet").metadata[b"hpt.metadata"]
```

A JSON file whose header comes after its charge items gives the rows read before it nulls
in the `metadata.*` columns; the command warns when that happens, and the last file's
footer is complete.

## Library

```toml
[dependencies]
hpt-parser = { git = "https://github.com/digitalashes/hpt-parser-rs" }
```

`Parser` reads any `std::io::Read` (a file, stdin, a decompressor) and is an iterator of
records:

```rust,no_run
use hpt_parser::{Format, Parser};
use std::fs::File;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open("123456789_hospital_standardcharges.json")?;
    let mut parser = Parser::new(file, Format::Json);
    for record in parser.by_ref() {
        let record = record?;
        let payer = record.payer.unwrap_or_default();
        println!("{:?} {:?} {:?}", record.charge.description, payer.name, payer.dollar);
    }
    // Complete once the parser is done: a JSON header may come after the items.
    println!("{:?}", parser.metadata().hospital_name);
    Ok(())
}
```

- A `Record` is `charge` (shared by every record of one row or item), `payer` (`None`
  for a row without payer values) and `position`. Fields added in later versions of the
  data dictionary are `Option`; an empty value is `None`.
- `Parser::with_aliases` reads non-standard names as CMS names.
- An `Err` ends the iteration.

With the `parquet` feature, `hpt_parser::parquet::write` does what the command does:

```rust,no_run
use hpt_parser::config::Config;
use hpt_parser::parquet::{Output, write};
use hpt_parser::{Format, Parser};
use std::fs::File;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(Path::new("columns.toml"))?;
    let parser = Parser::new(File::open("standardcharges.csv")?, Format::Csv)
        .with_aliases(config.aliases(Format::Csv));
    let mut output = Output::new("charges.parquet");
    output.apply(&config.output)?;
    let summary = write(parser, &config, &output, |chunk| println!("{}", chunk.path.display()))?;
    println!("{} records", summary.records);
    Ok(())
}
```

| Feature   | Default | Adds |
|-----------|---------|------|
| `config`  | yes     | The TOML config, its transforms and casts (`toml`, `serde`, `serde_json`). |
| `parquet` | yes     | Parquet output and the `hpt-parser` binary (`parquet`, `bytes`). |
| `plugins` | yes     | WASM plugins in the config (`extism`, on wasmtime). |

Without default features the parser has no dependencies:
`hpt-parser = { git = "...", default-features = false }`.

## Roadmap

- A release on crates.io, and binaries.

## License

[CC0 1.0](LICENSE).
