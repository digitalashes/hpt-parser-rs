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
    let t = |s: &str| Value::Text(s.into());
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
