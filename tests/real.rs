//! A real file, given by path: `HPT_FILE=<path> [HPT_SHOW=0,1000] cargo test --release --test
//! real -- --ignored --nocapture` prints the record count, the time and the records at the
//! `HPT_SHOW` indexes, for a comparison with another reader.
//!
//! `HPT_FILE=<path> HPT_OUT=<out.parquet> [HPT_CODEC=zstd:3] ... -- --ignored --nocapture
//! real_parquet` writes it through the built-in config.

use hpt_parser::{Format, Parser};
use std::time::Instant;

#[test]
#[ignore = "needs HPT_FILE"]
fn real_file() {
    let path = std::env::var("HPT_FILE").unwrap();
    let show: Vec<usize> = std::env::var("HPT_SHOW")
        .unwrap_or_else(|_| "0".into())
        .split(',')
        .map(|i| i.trim().parse().unwrap())
        .collect();
    let file = std::io::BufReader::new(std::fs::File::open(&path).unwrap());
    let start = Instant::now();
    let json =
        std::path::Path::new(&path).extension().is_some_and(|e| e.eq_ignore_ascii_case("json"));
    let mut parser = Parser::new(file, if json { Format::Json } else { Format::Csv });
    let mut count = 0;
    for record in parser.by_ref() {
        let record = record.unwrap();
        if show.contains(&count) {
            println!("record {count}: {record:#?}");
        }
        count += 1;
    }
    println!("{:#?}", parser.metadata());
    println!("{count} records in {:.2?}", start.elapsed());
}

#[cfg(feature = "parquet")]
#[test]
#[ignore = "needs HPT_FILE and HPT_OUT"]
fn real_parquet() {
    use hpt_parser::config::Config;
    use hpt_parser::parquet::{Output, parse_compression, write};
    use parquet::file::reader::{FileReader, SerializedFileReader};

    let path = std::env::var("HPT_FILE").unwrap();
    let mut output = Output::new(std::env::var("HPT_OUT").unwrap());
    if let Ok(codec) = std::env::var("HPT_CODEC") {
        output.compression = parse_compression(&codec).unwrap();
    }
    let file = std::io::BufReader::new(std::fs::File::open(&path).unwrap());
    let json =
        std::path::Path::new(&path).extension().is_some_and(|e| e.eq_ignore_ascii_case("json"));
    let start = Instant::now();
    let parser = Parser::new(file, if json { Format::Json } else { Format::Csv });
    let summary = write(parser, &Config::builtin(), &output, |_| {}).unwrap();
    let size = std::fs::metadata(&output.path).unwrap().len();
    let reader = SerializedFileReader::new(std::fs::File::open(&output.path).unwrap()).unwrap();
    assert_eq!(reader.metadata().file_metadata().num_rows().cast_unsigned(), summary.records);
    println!("{summary:?} in {:.2?}, {size} bytes", start.elapsed());
}
