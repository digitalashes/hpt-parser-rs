//! Parquet output through the config: types, codecs, row groups, chunks, the footer.

#![cfg(feature = "config")]

use hpt_parser::config::{Config, Value};
use hpt_parser::{Format, Parser};

#[test]
fn builtin_config() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tall-3.0.0.csv");
    let mut parser = Parser::new(std::fs::File::open(path).unwrap(), Format::Csv);
    let record = parser.nth(1).unwrap().unwrap();
    let config = Config::builtin();
    let row: Vec<(&str, Value)> = (config.columns.iter())
        .map(|c| (c.name.as_str(), c.value(&record, parser.metadata())))
        .filter(|(_, v)| *v != Value::Null)
        .collect();
    let t = |s: &'static str| Value::Text(s.into());
    assert_eq!(
        row,
        [
            ("charge_description", t("MRI brain without contrast")),
            ("charge_codes", t(r#"[{"code":"70551","type":"CPT"}]"#)),
            ("charge_setting", t("outpatient")),
            ("charge_modifiers", Value::List(vec![])),
            ("charge_gross_charge", t("1800")),
            ("charge_discounted_cash", t("1440")),
            ("charge_minimum", t("1100")),
            ("charge_maximum", t("1300")),
            ("payer_name", t("Region Health")),
            ("payer_plan", t("Silver HMO")),
            ("payer_percentage", t("70")),
            ("payer_methodology", t("percent of total billed charges")),
            ("payer_median_amount", t("1250.00")),
            ("payer_percentile_10th", t("980.00")),
            ("payer_percentile_90th", t("1600.00")),
            ("payer_count", t("25")),
        ]
    );
}

#[cfg(feature = "parquet")]
#[expect(clippy::unwrap_used, reason = "test helpers")]
mod files {
    use hpt_parser::config::Config;
    use hpt_parser::parquet::{Chunk, METADATA_KEY, Output, Summary, parse_compression, write};
    use hpt_parser::{Error, Format, Parser};
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use std::fmt::Write;
    use std::fs::{self, File};
    use std::path::{Path, PathBuf};

    const HEADER: &str = "\
hospital_name,last_updated_on,version,hospital_address,license_number | NY
General Hospital,2026-01-01,2.2.0,1 Main St,12345
description,code | 1,code | 1 | type,modifiers,payer_name,plan_name,standard_charge | negotiated_dollar,estimated_amount,additional_generic_notes,standard_charge | gross
";

    /// Files, records, late metadata.
    fn counts(s: Summary) -> (u32, u64, bool) {
        (s.chunks, s.records, s.late_metadata)
    }

    /// A fresh directory for one test's files.
    fn dir(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn columns(columns: &[(&str, &str, &str)]) -> Config {
        let mut toml = String::new();
        for (name, from, kind) in columns {
            writeln!(toml, "[[column]]\nname = \"{name}\"\nfrom = \"{from}\"\ntype = \"{kind}\"")
                .unwrap();
        }
        Config::parse(&toml).unwrap()
    }

    fn run(
        input: &[u8],
        format: Format,
        config: &Config,
        output: &Output,
    ) -> (Summary, Vec<Chunk>) {
        let mut chunks = Vec::new();
        let summary =
            write(Parser::new(input, format), config, output, |c| chunks.push(c.clone())).unwrap();
        (summary, chunks)
    }

    fn reader(path: &Path) -> SerializedFileReader<File> {
        SerializedFileReader::new(File::open(path).unwrap()).unwrap()
    }

    fn rows(path: &Path) -> Vec<String> {
        reader(path).get_row_iter(None).unwrap().map(|r| r.unwrap().to_string()).collect()
    }

    fn footer(path: &Path) -> serde_json::Value {
        let reader = reader(path);
        let kv = reader.metadata().file_metadata().key_value_metadata().unwrap();
        let value = kv.iter().find(|kv| kv.key == METADATA_KEY).unwrap().value.as_ref();
        serde_json::from_str(value.unwrap()).unwrap()
    }

    /// `n` records, the description numbered.
    fn csv(n: usize) -> String {
        let mut csv = HEADER.to_string();
        for i in 1..=n {
            writeln!(csv, "Item {i},,,,Acme,PPO,,,,").unwrap();
        }
        csv
    }

    #[test]
    fn types() {
        let input = format!(
            "{HEADER}\
A,99213,CPT,,Acme,PPO,1.5,7,yes,2026-01-02
,,,25,Acme,PPO,,,,
,,,25|59|76,Acme,PPO,x,1.5,maybe,soon
"
        );
        let config = columns(&[
            ("s", "charge.description", "string"),
            ("f", "payer.dollar", "f64"),
            ("i", "payer.estimated_amount", "i64"),
            ("b", "charge.notes", "bool"),
            ("d", "charge.gross_charge", "date"),
            ("j", "charge.codes", "json"),
            ("l", "charge.modifiers", "list"),
        ]);
        let path = dir("types").join("out.parquet");
        let (summary, chunks) = run(input.as_bytes(), Format::Csv, &config, &Output::new(&path));
        assert_eq!(counts(summary), (1, 3, false));
        assert_eq!(
            chunks.iter().map(|c| (&c.path, c.index, c.records)).collect::<Vec<_>>(),
            [(&path, 1, 3)]
        );
        assert_eq!(
            rows(&path),
            [
                r#"{s: "A", f: 1.5, i: 7, b: true, d: 2026-01-02, j: "[{"code":"99213","type":"CPT"}]", l: []}"#,
                r#"{s: null, f: null, i: null, b: null, d: null, j: "[]", l: ["25"]}"#,
                r#"{s: null, f: null, i: null, b: null, d: null, j: "[]", l: ["25", "59", "76"]}"#,
            ]
        );
    }

    #[test]
    fn codecs() {
        let config = columns(&[("s", "charge.description", "string")]);
        let input = csv(3);
        let dir = dir("codecs");
        let codecs = [
            ("none", "UNCOMPRESSED"),
            ("snappy", "SNAPPY"),
            ("gzip", "GZIP"),
            ("gzip:9", "GZIP"),
            ("lz4", "LZ4_RAW"),
            ("zstd", "ZSTD"),
            ("zstd:19", "ZSTD"),
            ("brotli", "BROTLI"),
            ("brotli:5", "BROTLI"),
        ];
        for (i, (codec, name)) in codecs.iter().enumerate() {
            let mut output = Output::new(dir.join(format!("{i}.parquet")));
            output.compression = parse_compression(codec).unwrap();
            run(input.as_bytes(), Format::Csv, &config, &output);
            // The file names the codec, not the level.
            let written = reader(&output.path).metadata().row_group(0).column(0).compression();
            assert!(format!("{written:?}").starts_with(name), "{codec}: {written:?}");
            assert_eq!(rows(&output.path).len(), 3, "{codec}");
        }
        for codec in ["lzo", "zstd:99", "snappy:1", "gzip:x", "gzip:-1", "brotli:12", ""] {
            let e = parse_compression(codec).unwrap_err();
            assert!(matches!(e, Error::Config { .. }), "{codec}: {e}");
        }
    }

    #[test]
    fn row_groups_and_chunks() {
        let config = columns(&[("s", "charge.description", "string")]);
        let input = csv(5);
        let dir = dir("chunks");
        let mut output = Output::new(dir.join("one.parquet"));
        output.row_group_size = 2;
        run(input.as_bytes(), Format::Csv, &config, &output);
        assert_eq!(reader(&output.path).metadata().num_row_groups(), 3);

        let mut output = Output::new(dir.join("out.parquet"));
        output.chunk_size = 2;
        let (summary, chunks) = run(input.as_bytes(), Format::Csv, &config, &output);
        assert_eq!(counts(summary), (3, 5, false));
        let names: Vec<_> = (chunks.iter())
            .map(|c| (c.path.file_name().unwrap().to_str().unwrap(), c.index, c.records))
            .collect();
        assert_eq!(
            names,
            [("out-00001.parquet", 1, 2), ("out-00002.parquet", 2, 2), ("out-00003.parquet", 3, 1)]
        );
        assert_eq!(rows(&chunks[2].path), [r#"{s: "Item 5"}"#]);
        assert!(!dir.join("out.parquet").exists());
    }

    #[test]
    fn footer_metadata() {
        let config = columns(&[("s", "charge.description", "string")]);
        let path = dir("footer").join("out.parquet");
        run(csv(1).as_bytes(), Format::Csv, &config, &Output::new(&path));
        let meta = footer(&path);
        assert_eq!(meta["hospital_name"], "General Hospital");
        assert_eq!(meta["license_state"], "NY");
        assert_eq!(meta["layout"], "tall");

        let input =
            fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/metadata-after.json"))
                .unwrap();
        let config = columns(&[
            ("hospital", "metadata.hospital_name", "string"),
            ("s", "charge.description", "string"),
        ]);
        let path = path.with_file_name("late.parquet");
        let (summary, _) = run(&input, Format::Json, &config, &Output::new(&path));
        assert!(summary.late_metadata);
        assert_eq!(rows(&path), [r#"{hospital: null, s: "Office visit"}"#]);
        assert_eq!(footer(&path)["hospital_name"], "Lake Hospital");
    }

    #[test]
    fn zero_records() {
        let config = columns(&[("s", "charge.description", "string")]);
        let path = dir("zero").join("out.parquet");
        let (summary, chunks) = run(HEADER.as_bytes(), Format::Csv, &config, &Output::new(&path));
        assert_eq!(counts(summary), (1, 0, false));
        assert_eq!(chunks.len(), 1);
        assert_eq!(reader(&path).metadata().file_metadata().num_rows(), 0);
        assert_eq!(footer(&path)["hospital_name"], "General Hospital");
    }
}
