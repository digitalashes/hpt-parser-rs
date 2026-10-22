---
name: hpt-parser-config
description: Writing the TOML output config of hpt-parser (CMS Hospital Price Transparency files to Parquet) and Extism WASM plugins for it. Covers picking fields, types, transforms and aliases, checking the output, and writing, building and testing a plugin for its hooks (value transform, computed column, filter, metadata). Use when creating or changing a columns.toml, choosing how to clean or type a field, or writing a plugin for the config.
---

# Writing an hpt-parser config and plugins

The config turns each record (one per charge item and payer/plan) into a Parquet row: which
columns, from which fields, cleaned how, typed how. The `hpt-parser` skill covers the
command and the library; this one covers the config and its plugins.

## Workflow

1. Look at the file: `hpt-parser f.csv --metadata table` gives the layout (tall, wide,
   json) and the version. Fields of 3.0.0 (`payer.median_amount`, `percentile_*`,
   `count`, `metadata.type_2_npi`, `metadata.attester_name`) are null in 2.x files,
   `payer.estimated_amount` in files before 2.2.0.
2. Start from the built-in config, every field as an untouched string column:
   `hpt-parser --print-config > columns.toml`. Delete, rename and reorder columns, then
   type and clean them.
3. Run it on a sample. A CSV file cuts by lines with its header rows intact
   (`head -n 20000 f.csv > sample.csv`, wide files too); a JSON file does not, so use a
   small file or the whole one.
4. Check the nulls. A value that does not cast is null, with no error. A cleaning that
   misses shows as a typed column with more nulls than its text: keep the text as a
   second column while trying (`dollar` and `dollar_text` below), then compare.

   ```sql
   -- DuckDB
   SUMMARIZE SELECT * FROM 'out.parquet';                       -- null_percentage per column
   SELECT dollar_text, count(*) FROM 'out.parquet'
   WHERE dollar IS NULL AND dollar_text IS NOT NULL GROUP BY 1 ORDER BY 2 DESC LIMIT 20;
   ```

5. A config that does not load stops the run before reading:
   ``error: columns.toml: column `setting`: unknown domain `settings` ``. Unknown keys
   are errors too.

## The file

```toml
[output]                          # optional; command-line options override it
compression = "zstd:3"            # none, snappy, gzip[:level], lz4, zstd[:level], brotli[:level]
row_group_size = 256000           # 0: one per file
chunk_size = 0                    # records per file, 0: one file

[[column]]                        # one per output column, in order; names are unique
name = "negotiated"
from = "payer.dollar"             # one field, or a list: their texts joined by `|`
type = "f64"
transform = ["money", { round = 2 }]   # run in order, then the cast
```

Also `[[plugin]]`, `[[filter]]`, `[[metadata]]` (plugins, below) and `[aliases.csv]`,
`[aliases.json]`.

## Fields

- `charge.*`: `description`, `codes`, `setting`, `billing_class`, `drug_unit`,
  `drug_unit_type`, `modifiers`, `gross_charge`, `discounted_cash`, `minimum`,
  `maximum`, `notes`. Shared by every payer/plan of the item.
- `payer.*`: `name`, `plan`, `dollar`, `percentage`, `algorithm`, `methodology`,
  `estimated_amount`, `median_amount`, `percentile_10th`, `percentile_90th`, `count`,
  `notes`. All null in a record without payer values (an item with only gross or cash
  prices).
- `metadata.*`: `layout`, `version`, `hospital_name`, `last_updated_on`,
  `location_name`, `hospital_address`, `license_number`, `license_state`, `type_2_npi`,
  `attestation`, `attestation_confirmed`, `attester_name`; the same in every row, but
  null in the rows read before a JSON header that comes after the items.
- `record.position`: the CSV line the row starts on, or the JSON item's index. The
  records of one wide row or JSON item share it, so it is not a row id.

Every value is the file's text, trimmed; an empty value is null. Amounts keep the file's
spelling (`$1,204.50`, `1204.5`), so a number column needs `money`. Lists:
`charge.modifiers`, `metadata.location_name`, `metadata.hospital_address`,
`metadata.type_2_npi`. `charge.codes` holds code and type pairs, with only the codes
that have a value.

A list of fields joins their texts with `|`, a missing one as empty, and is null only
when all are missing. It makes keys: `from = [..., "payer.plan"]` with `hash`.

## Types

| Type | Reads | Null when |
|---|---|---|
| `string` | the text; lists joined by `\|`; codes as `TYPE:code` joined by `\|` | null |
| `f64` | a finite number after trimming (`1e3` too) | `$1,204` without `money`, `N/A` |
| `i64` | a whole number | `12.0`, `1,000` |
| `bool` | `true yes y 1` / `false no n 0`, any case | anything else |
| `date` | `YYYY-MM-DD` or `M/D/YYYY`, a real day | `2024-02-30`, `20240101`, `2024/01/02` |
| `json` | codes as `[{"code":..,"type":..}]`, lists as arrays, text as a JSON string; `codes_by_type` output as it is | null |
| `list` | a list field, codes as `TYPE:code`, text split on `\|` | null |

## Transforms

On text, and on each item of a list. Codes pass through every transform but
`codes_by_type`, `codes_list`, `hash` and plugins.

| Transform | Does | Watch |
|---|---|---|
| `"trim"` | whitespace off both ends | values come trimmed; this is for plugin answers |
| `"upper"`, `"lower"` | case | |
| `"money"` | drops `$`, `,` and whitespace | `(12.50)` and `1.234,50` stay wrong |
| `{ round = N }` | a number to N digits after the point, 0 to 17 | text that is not a number stays |
| `{ map = { ip = "inpatient" } }` | the trimmed value equal to a key, else equal ignoring case, becomes the value | no match: unchanged; quote keys with spaces |
| `{ match = "<domain>", fuzzy = 0.8 }` | snaps to the CMS spelling, ignoring case; with `fuzzy`, the most similar value at least that similar (1 minus the edit distance over the longer length) | no match: unchanged, not null |
| `{ split = ";" }` | text to a list; items trimmed, empty ones dropped | with `type = "list"` |
| `{ default = "x" }` | for a null or empty value | put it last |
| `"codes_by_type"` | codes as `{"CPT":["99213"],"MS-DRG":["470"]}`, sorted, types upper-cased, untyped under `""` | with `type = "json"` |
| `"codes_list"` | codes as `["CPT:99213",...]`, sorted | with `type = "list"` |
| `"hash"` | a 128-bit FNV-1a of the text as 32 hex digits | null stays null |
| `{ plugin = "p.f" }` | a plugin function, below | |

Domains for `match`: `setting` (inpatient, outpatient, both), `methodology` (case rate,
fee schedule, percent of total billed charges, per diem, other), `code_type` (CPT, HCPCS,
ICD, DRG, MS-DRG, R-DRG, S-DRG, APS-DRG, AP-DRG, APR-DRG, TRIS-DRG, APC, NDC, HIPPS,
LOCAL, EAPG, CDT, RC, CDM, CMG, MS-LTC-DRG), `drug_unit` (GR, ME, ML, UN, F2, EA, GM),
`billing_class` (professional, facility, both). Abbreviations are too far for `fuzzy`:
`map` them first, `transform = [{ map = { IP = "inpatient", OP = "outpatient" } }, { match = "setting" }]`.

## Recipes

```toml
[[column]]                       # a price as a number
name = "negotiated"
from = "payer.dollar"
type = "f64"
transform = ["money", { round = 2 }]

[[column]]                       # CMS spelling, misspellings forgiven
name = "methodology"
from = "payer.methodology"
type = "string"
transform = [{ match = "methodology", fuzzy = 0.8 }]

[[column]]                       # codes grouped by type
name = "codes"
from = "charge.codes"
type = "json"
transform = ["codes_by_type"]

[[column]]                       # a stable key for an item and payer/plan
name = "id"
from = ["metadata.hospital_name", "charge.description", "payer.name", "payer.plan"]
type = "string"
transform = ["hash"]

[[column]]                       # a date column
name = "updated"
from = "metadata.last_updated_on"
type = "date"
```

## Aliases

A file that uses its own name for a CMS column or member reads as the CMS one with an
alias. Signs that one is needed: a header error, or a column that is null in every row
although the file has the data.

```toml
[aliases.csv]                    # row 1 and row 3 names; per part, trimmed, any case
"standard_charge | gross" = ["gross_charges", "gross charge"]
"standard_charge | negotiated_dollar" = ["standard_charge | negotiated_amount"]

[aliases.json]                   # member names, exact
modifier_code = ["modifiers"]
```

- The key is the CMS name (checked: a name the parser does not read is an error); the
  value lists the file's names.
- In a wide file, alias the payer-specific name without its payer and plan, as above:
  `standard_charge | Acme | PPO | negotiated_amount` then reads as
  `standard_charge | Acme | PPO | negotiated_dollar`.
- A file that has the CMS name as well keeps it. The aliases a file used are in its
  metadata as `aliases_used`.

## Plugins

For what the transforms cannot do: lookups, payer name cleanup with rules, derived
values, dropping records. A plugin is an [Extism](https://extism.org) module, from any
language with an Extism PDK.

```toml
[[plugin]]
name = "acme"
path = "acme.wasm"               # relative to the config file
config = { region = "us" }       # Extism config: strings as they are, other values as TOML text

[[column]]
name = "payer"
from = "payer.name"
type = "string"
transform = ["trim", { plugin = "acme.payer" }]   # value transform

[[column]]
name = "region"
plugin = "acme.region"           # computed column, in place of `from`
fields = ["payer.name"]
type = "string"

[[filter]]
plugin = "acme.keep"
fields = ["charge.setting"]

[[metadata]]
plugin = "acme.source"
```

| Hook | Gets | Answers |
|---|---|---|
| value transform | `{"values":[..]}`: the column's values so far | `{"values":[..]}` |
| computed column | `{"records":[..]}` | `{"values":[..]}` |
| filter | `{"records":[..]}` | `{"keep":[true,false,..]}` |
| metadata | the metadata object | `{"fields":{"key":"value"}}` |

- A call gets up to 512 items, and the answer has exactly one per input, in order. A
  wrong shape, a wrong count, a `keep` item that is not a boolean or an error from the
  function stops the run: `error: plugin acme.keep: ...`.
- A record is a flat object keyed by field name with the hook's `fields` that have a
  value: `{"charge.setting":"inpatient","charge.codes":[{"code":"70551","type":"CPT"}]}`.
  Absent fields are left out, so read them as optional. Without `fields` a record has
  every `charge.*` and `payer.*` field and `record.position`, long notes included, which
  makes the run several times slower: always name `fields`.
- An input value is a string, a list of strings, the codes as above, or null. An answer
  value may be a string, a list of strings, a number or a boolean (as its text), null,
  or other JSON (as its text); it goes on through the rest of the transforms and the
  cast, so a plugin column of `type = "f64"` may answer numbers.
- Filters run first, one after another, on the records of a batch; the rest see only
  the records kept.
- `[[metadata]]` runs once, after the last record. The metadata object has the
  `metadata.*` fields under their short names (`hospital_name`, `location_name` as a
  list, ...) and `modifiers`, `aliases_used`, `extra`. The answer's fields go to `extra`
  in the last file's footer (`hpt.metadata`), not into the rows, so no column or
  `fields` can name them.
- Sandboxed: no WASI, files or network; 60 seconds a call and 256 MiB of memory. For
  Rust build for `wasm32-unknown-unknown`, where there is no clock, random source or
  environment either: keep the functions pure.
- One instance serves the whole run, so a lookup table built on the first call (a
  `OnceLock` filled from config) stays, and counts toward the memory limit.

### A plugin in Rust

```toml
# Cargo.toml
[package]
name = "acme"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
extism-pdk = "1"
serde = { version = "1", features = ["derive"] }

[profile.release]
opt-level = "s"
lto = true
strip = true
```

```rust
// src/lib.rs
use extism_pdk::{FnResult, Json, config, plugin_fn};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct Records {
    records: Vec<Record>,
}

/// Only the fields this plugin reads; serde skips the rest.
#[derive(Deserialize)]
struct Record {
    #[serde(rename = "payer.name")]
    payer: Option<String>,
    #[serde(rename = "charge.setting")]
    setting: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct Values<T> {
    values: Vec<T>,
}

#[derive(Serialize)]
struct Keep {
    keep: Vec<bool>,
}

#[derive(Deserialize)]
struct Metadata {
    hospital_name: Option<String>,
}

#[derive(Serialize)]
struct Fields {
    fields: Source,
}

#[derive(Serialize)]
struct Source {
    source: String,
}

/// Computed column: `[[column]] plugin = "acme.region"`, `fields = ["payer.name"]`.
#[plugin_fn]
pub fn region(Json(input): Json<Records>) -> FnResult<Json<Values<Option<String>>>> {
    let region = config::get("region")?.unwrap_or_default();
    let values = input.records.iter().map(|r| r.payer.as_ref().map(|p| format!("{region}:{p}")));
    Ok(Json(Values { values: values.collect() }))
}

/// Filter: `[[filter]] plugin = "acme.keep"`, `fields = ["charge.setting"]`.
#[plugin_fn]
pub fn keep(Json(input): Json<Records>) -> FnResult<Json<Keep>> {
    let keep = input.records.iter().map(|r| r.setting.as_deref() == Some("inpatient"));
    Ok(Json(Keep { keep: keep.collect() }))
}

/// Value transform: `transform = [{ plugin = "acme.payer" }]` on a string column.
#[plugin_fn]
pub fn payer(Json(input): Json<Values<Option<String>>>) -> FnResult<Json<Values<Option<String>>>> {
    let values = input.values.into_iter().map(|v| v.map(|s| clean_payer(&s)));
    Ok(Json(Values { values: values.collect() }))
}

/// Metadata: `[[metadata]] plugin = "acme.source"`, a field for the footer.
#[plugin_fn]
pub fn source(Json(metadata): Json<Metadata>) -> FnResult<Json<Fields>> {
    let source = format!("acme:{}", metadata.hospital_name.unwrap_or_default());
    Ok(Json(Fields { fields: Source { source } }))
}

fn clean_payer(name: &str) -> String {
    name.trim_end_matches(" Inc").trim_end_matches(" LLC").to_uppercase()
}

#[cfg(test)]
mod tests {
    #[test]
    fn clean_payer() {
        assert_eq!(super::clean_payer("Acme Health Inc"), "ACME HEALTH");
    }
}
```

```sh
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/acme.wasm .
cargo test                     # the plain functions, on the host
```

- Read into typed structs with only the fields the plugin uses; a generic JSON value
  per record took more than twice as long.
- A value transform on a list column gets lists (`Vec<String>`), on a codes column
  `[{"code":..,"type":..}]`: type `values` to match the column.
- Keep the logic in plain functions and test them with `cargo test` on the host; the
  `#[plugin_fn]` wrappers only move JSON. Then run the config on a small file, the
  repository's `tests/fixtures/tall-3.0.0.csv` for example.
- Add `RUSTFLAGS="--remap-path-prefix=$HOME=~"` to the build to keep local paths out of
  the panic messages in the `.wasm`.
- Two more plugins with their configs are in the repository's
  [`examples/`](https://github.com/digitalashes/hpt-parser-rs/tree/main/examples):
  `payer-names` (a lookup table from config, built once in a `OnceLock`) and `rates` (a
  filter, a computed column, a metadata hook reporting counts kept in statics).
