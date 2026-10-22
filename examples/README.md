# Examples

| Example | What it shows |
|---|---|
| [`payer-names/`](payer-names) | A value transform: payer names to one spelling each, from a lookup table in the plugin's config, read once per run. |
| [`rates/`](rates) | A filter by code type, a computed column (the negotiated dollar amount as a percentage of the gross charge) and a metadata hook with the filter's counts. |
| [`listen.rs`](listen.rs), [`listen.py`](listen.py) | A listener for `--notify-socket`: prints each event, exits on `done` or `error`. |

## Plugins

Each plugin is a crate of its own, built to WASM, with a `columns.toml` that runs it on a
fixture of this repository. Its file comments say what each function takes and gives.

```sh
rustup target add wasm32-unknown-unknown
cd examples/rates
cargo build --release --target wasm32-unknown-unknown
cargo test                          # the plain functions, on the host
cd ../..
cargo run -- tests/fixtures/tall-3.0.0.csv -c examples/rates/columns.toml -o rates.parquet
```

`rates.parquet` has the two CPT rows of the three, `percent_of_gross` 66.7 and null (the
second payer has no dollar amount), and `"extra":{"dropped":"1","kept":"2"}` in the
footer's `hpt.metadata`. `payer-names` on `tests/fixtures/wide-3.0.0.csv` writes the payer
"Acme" as "Acme Health", the name the tall fixture uses.

## Listeners

Start the listener first: with no one listening, the run stops at once.

```sh
cargo run --example listen -- hpt.sock &     # or: python3 examples/listen.py hpt.sock &
until [ -S hpt.sock ]; do sleep 0.1; done
cargo run -- tests/fixtures/tall-3.0.0.csv -o charges.parquet --chunk-size 2 --notify-socket hpt.sock
```

```text
chunk 1: charges-00001.parquet (2 records)
chunk 2: charges-00002.parquet (1 records)
done: 2 chunks, 3 records
```
