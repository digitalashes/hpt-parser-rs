#![no_main]

#[path = "../../tests/fuzz.rs"]
mod fuzz;

libfuzzer_sys::fuzz_target!(|data: &[u8]| fuzz::check(hpt_parser::Format::Csv, data));
