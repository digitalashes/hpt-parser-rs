# Changelog

Notable changes to `hpt-parser`, the library and the command line. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Times are from a 2019 laptop
(Intel i7-9750H).

## [Unreleased]

### Changed

- Faster on CSV files: each batch's records are freed on a thread of their own once their
  rows are made. A 1 GB tall CSV 4.4 -> 3.6 seconds, the 2.5 GB wide CSV 4.6 -> 4.1, a
  1.2 GB CSV with 1000 columns 9.8 -> 8.8, a 229 MB JSON 1.4 -> 1.3. The Parquet bytes are
  the same.
- The release binaries are built with one codegen unit and thin LTO: wide CSV files 9-11%
  faster, tall ones 2-3%, JSON within the spread. The binary is 22 MB instead of 27 MB.
- Faster on wide CSV files and with plugins: two to six threads, each with a share of the
  columns, make and encode the values of each row group. The 2.5 GB wide CSV 3.7 -> 2.7
  seconds, the 1.2 GB CSV with 1000 columns 7.7 -> 3.8, a 1 GB tall CSV 3.6 -> 3.3, a
  229 MB JSON 1.3 -> 1.2; with the `examples/payer-names` config the 1000-column CSV
  13.6 -> 5.3 and the tall CSV 4.6 -> 3.3. The Parquet bytes are the same.
- A column that reads only the charge and the file's metadata is made once per charge and
  repeated over its payers (a wide row's payer/plan blocks, a JSON standard charge's
  payers). With two column threads, as a machine with four cores has, the 1.2 GB CSV with
  1000 columns 7.5 -> 6.2 seconds and the 2.5 GB wide CSV 3.7 -> 3.2; with six, where
  reading the file is the bound, 3% faster and 8% less CPU time on the 1000-column CSV.
  The Parquet bytes are the same.

## [0.1.3] - 2026-10-23

### Fixed

- Wide files that name a payer without a plan, `standard_charge | Aetna | negotiated_dollar`
  or `estimated_amount | Aetna`, lost those values without a word. They are read as a
  payer/plan block with no plan (`plan` is null). In one sampled file this was 14 of 25
  payers, in another 16 of 18. In a tall file such columns are still not read.
- A tall file with a template column left in, such as `estimated_amount | [payer_name] |
  [plan_name]`, looked both tall and wide and failed. A `[payer_name]` or `[plan_name]`
  placeholder is no payer now, so the file reads as tall.

Files without these shapes give the same records and the same Parquet bytes as 0.1.2.

## [0.1.2] - 2026-10-23

### Changed

- About 30% faster with plugins: the filters, the plugin calls and the rows of a batch run
  on a thread of their own while the next batch is read, as without plugins, with the same
  Parquet bytes as before. A 1 GB tall CSV with the `examples/rates` config 7.7 -> 5.2
  seconds, the 2.5 GB wide CSV 5.8 -> 4.0, a 229 MB JSON 2.1 -> 1.5.

### Fixed

- With plugins, a batch now ends where the file's metadata changes, so each record has the
  metadata it was read with, and a parse error still has the records before it written.

## [0.1.1] - 2026-10-23

### Added

- The command reads a `.gz`, or a `.zip` with one `.csv` or `.json` in it, unpacked on the
  fly (a Mac's `__MACOSX/` entries are left out). The default output name drops `.gz` or
  `.zip`.
- `cargo binstall hpt-parser` fetches the release binary.

### Changed

- About twice as fast without plugins: records are read, made into rows and encoded on
  three threads, with the same Parquet bytes as before. A 1 GB tall CSV 8.2 -> 4.3
  seconds, the 2.5 GB wide CSV 7.4 -> 4.4, a 1.2 GB CSV with 1000 columns 16.8 -> 9.7, a
  229 MB JSON 2.8 -> 1.3.
- The binary has a default feature of its own, `cli` (with `flate2` and `zip`), so
  `parquet` alone no longer pulls them in or builds it.
- An input that is neither `.csv` nor `.json` now exits with 1 (the input failed), not 2.

## [0.1.0] - 2026-10-22

The first release.

### Added

- `Parser` reads CSV files (tall and wide) and JSON files of data dictionary versions
  2.0.0 to 3.0.0 as a stream of flat records, one per charge item and payer/plan, with the
  file's header apart as `Metadata`. A value that does not parse stays as its text; only a
  file that cannot be read at all is an error.
- Aliases read non-standard CSV column and JSON key names.
- `parquet::write` writes the records to Parquet through a TOML config: the columns, their
  names, order and types (`string`, `json`, `f64`, `i64`, `bool`, `date`, `list`), and
  transforms (money, case, CMS value domains, code grouping, ids). Codecs, row groups,
  chunk files, and the metadata as JSON in every footer under `hpt.metadata`.
- WASM plugins (Extism) named in the config: value transforms, computed columns, record
  filters and metadata fields, without network or file access.
- The `hpt-parser` command: Parquet next to the input or at `-o`, `--config`, `--format`,
  `--compression`, `--chunk-size`, `--row-group-size`, `--metadata json|table`,
  `--print-config`, and `--notify-socket` events (`chunk_ready`, `done`, `error`) for
  pipelines.
- Release binaries for macOS, Linux and Windows.

[Unreleased]: https://github.com/digitalashes/hpt-parser-rs/compare/v0.1.3...HEAD
[0.1.3]: https://github.com/digitalashes/hpt-parser-rs/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/digitalashes/hpt-parser-rs/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/digitalashes/hpt-parser-rs/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/digitalashes/hpt-parser-rs/releases/tag/v0.1.0
