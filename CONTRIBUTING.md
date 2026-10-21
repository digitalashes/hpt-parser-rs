# Contributing

Open an issue before a large change. A pull request passes:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```
