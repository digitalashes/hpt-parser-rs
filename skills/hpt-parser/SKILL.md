---
name: hpt-parser
description: Reading CMS Hospital Price Transparency files (CSV tall or wide, JSON) into records or Parquet with the hpt-parser crate or its command line. Use when writing code that calls hpt_parser::Parser or hpt_parser::parquet::write, writing an output config (TOML) or a plugin for it, or running the hpt-parser command.
---

# Using hpt-parser

`hpt-parser` streams a CMS Hospital Price Transparency machine-readable file (data
dictionary 2.0.0 to 3.0.0) as flat records: one per charge item and payer/plan, the same
shape for tall CSV, wide CSV and JSON. The command writes them to Parquet through a TOML
config. It does not validate; for that, use `hpt-validator`.

## Command line

```sh
hpt-parser file.csv                                   # file.parquet, every field a string column
hpt-parser file.json -c columns.toml -o out/c.parquet --chunk-size 5000000
hpt-parser file.json --metadata json                  # only the header, nothing written
hpt-parser --print-config > columns.toml              # the built-in config, to edit
```

- Exit 0 done, 1 input, config or output failed (`error: ...` on stderr), 2 usage.
- `--format csv|json` is required for `-` (stdin); otherwise the extension decides.
- `--notify-socket <path>`: one JSON line per event on a unix socket someone listens on:
  `chunk_ready` (`path`, `index`, `records`), then `done` (`chunks`, `records`) or
  `error` (`message`). A `chunk_ready` file is complete and can be loaded at once.
- The file's metadata is JSON in each Parquet footer under the key `hpt.metadata`.

## Library

```rust
use hpt_parser::{Format, Parser};

let mut parser = Parser::new(reader, Format::Csv);   // any std::io::Read
for record in parser.by_ref() {
    let record = record?;                            // an Err ends the iteration
    // record.charge (shared by a row's records), record.payer (Option), record.position
}
let metadata = parser.metadata();                    // complete only now
```

- Values are `Option<String>` as in the file, trimmed; an empty value is `None`. Nothing
  is typed or cleaned by the parser: that is the config's job.
- `Err` only when the file cannot be read: `Error::Io`, `Error::Csv` (no header, no known
  layout), `Error::Json` (syntax, or the wrong top-level shape). A bad value is never an
  error.
- `Parser::with_aliases(config.aliases(format))` reads non-standard column names as the
  CMS ones.
- `hpt_parser::parquet::write(parser, &config, &output, on_chunk)` is the command's
  writer; `Summary` has `chunks`, `records`, `late_metadata`.
- Features, all default: `config` (TOML), `parquet` (writer, binary), `plugins` (WASM).
  `default-features = false` leaves the parser with no dependencies.

## Config recipes

Fields are `charge.*`, `payer.*`, `metadata.*` (`metadata.extra.<key>` too) and
`record.position`; the README lists them all. Transforms run in order, then the cast.

```toml
[[column]]                       # money as a number
name = "negotiated"
from = "payer.dollar"
type = "f64"
transform = ["money", { round = 2 }]

[[column]]                       # CMS spelling of a value domain
name = "setting"
from = "charge.setting"
type = "string"
transform = [{ match = "setting", fuzzy = 0.8 }]

[[column]]                       # a stable charge key
name = "id"
from = ["metadata.hospital_name", "charge.description", "payer.name", "payer.plan"]
type = "string"
transform = ["hash"]
```

## Plugins

`[[plugin]]` (`name`, `path` relative to the config, `config` table) names an Extism
module; its functions are `<name>.<function>`. Sandboxed: no files, network or WASI,
60 s per call, 256 MiB.

| Hook | Config | Gets | Answers |
|---|---|---|---|
| value transform | `transform = [{ plugin = "p.f" }]` | `{"values":[..]}` | `{"values":[..]}` |
| computed column | `plugin = "p.f"` (and `fields`) in place of `from` | `{"records":[..]}` | `{"values":[..]}` |
| filter | `[[filter]] plugin = "p.f"` (and `fields`) | `{"records":[..]}` | `{"keep":[bool]}` |
| metadata | `[[metadata]] plugin = "p.f"` | the metadata object | `{"fields":{..}}` |

- Up to 8192 items a call; the answer has exactly one item per input, in order.
- A record is flat, keyed by field name (`"charge.setting"`), only the fields with a
  value. Codes are `[{"code":..,"type":..}]`.
- Always set `fields = ["charge.setting", ...]` to what the function reads (`metadata.*`
  allowed): without it every `charge.*`/`payer.*` field goes, long notes included, and
  the JSON makes a run about 4x slower.
- Deserialize into typed structs with only the fields you read: a generic JSON value
  per record costs several times more.
- `[[metadata]]` runs once at the end; its fields go to `extra` in the last footer.

## Pitfalls

- A JSON file may put its header after the charge items: records read before it see
  empty metadata, so `metadata.*` columns are null in those rows and
  `Summary::late_metadata` is true. The last footer is complete.
- A value that does not cast to the column's type is null, silently: check null counts
  after a new config.
- `record.position` is the CSV line a row starts on, or the JSON item's index; the
  records of one wide row or JSON item share it.
