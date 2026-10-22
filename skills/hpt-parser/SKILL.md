---
name: hpt-parser
description: Reading CMS Hospital Price Transparency files (CSV tall or wide, JSON) into records or Parquet with the hpt-parser crate or its command line. Use when writing code that calls hpt_parser::Parser or hpt_parser::parquet::write, or running the hpt-parser command. For writing its TOML config or a plugin, use hpt-parser-config.
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

## Config and plugins

The output columns, their types and cleaning, aliases and WASM plugins: the
`hpt-parser-config` skill.

## Pitfalls

- A JSON file may put its header after the charge items: records read before it see
  empty metadata, so `metadata.*` columns are null in those rows and
  `Summary::late_metadata` is true. The last footer is complete.
- `record.position` is the CSV line a row starts on, or the JSON item's index; the
  records of one wide row or JSON item share it.
