//! A real file, given by path: `HPT_FILE=<path> [HPT_SHOW=0,1000] cargo test --release --test
//! real -- --ignored --nocapture` prints the record count, the time and the records at the
//! `HPT_SHOW` indexes, for a comparison with another reader.

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
