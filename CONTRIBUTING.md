# Contributing

Open an issue before a large change. A pull request passes:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

CI also runs clippy and the tests without the default features (`--no-default-features`,
and with `--features cli` alone: the binary without plugins), and builds and runs the plugins in `examples/`.

`tests/plugin/test.wasm` is the build of the test plugin, committed. After changing
`tests/plugin/src/lib.rs`, rebuild it as [its README](tests/plugin/README.md) says (needs
`rustup target add wasm32-unknown-unknown`).

`tests/real.rs` runs a real file, which is never committed:

```sh
HPT_FILE=<path> cargo test --release --test real -- --ignored --nocapture
HPT_FILE=<path> HPT_OUT=out.parquet cargo test --release --test real -- --ignored --nocapture real_parquet
```

`tests/fuzz.rs` mutates every fixture and reader case at random and checks that reading,
the config's transforms and casts and the Parquet writer do not panic; `fuzz/` runs the
same check under [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) on a nightly
toolchain:

```sh
mkdir -p fuzz/corpus/csv fuzz/corpus/json
cargo +nightly fuzz run -s none csv fuzz/corpus/csv tests/fixtures tests/reader -- -max_len=4096
cargo +nightly fuzz run -s none json fuzz/corpus/json tests/fixtures -- -max_len=4096
```
