The plugin `tests/plugin.rs` runs. `test.wasm` is its build, committed; after changing
`src/lib.rs`, rebuild it from this directory (the prefix keeps local paths out of the
panic messages in the file):

```sh
RUSTFLAGS="--remap-path-prefix=$HOME=~" cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/hpt_parser_test_plugin.wasm test.wasm
```
