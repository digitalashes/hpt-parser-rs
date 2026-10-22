#![expect(clippy::unwrap_used, reason = "a broken fixture should fail the test")]

use hpt_parser::{
    Charge, Code, Error, Format, Layout, Metadata, ModifierInfo, ModifierPayer, Parser, Payer,
    Record,
};
use std::sync::Arc;

const AFFIRMATION: &str = "To the best of its knowledge and belief, the hospital has included all applicable standard charge information in accordance with the requirements of 45 CFR 180.50, and the information encoded is true, accurate, and complete as of the date indicated.";

fn read(name: &str) -> (Vec<Record>, Metadata) {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let format = if name.ends_with("json") { Format::Json } else { Format::Csv };
    let mut parser = Parser::new(std::fs::File::open(path).unwrap(), format);
    let records = parser.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    (records, parser.metadata().clone())
}

#[expect(clippy::unnecessary_wraps, reason = "builds an expected optional field")]
fn s(v: &str) -> Option<String> {
    Some(v.into())
}

fn list(v: &[&str]) -> Vec<String> {
    v.iter().map(|&v| v.into()).collect()
}

fn code(code: &str, code_type: Option<&str>) -> Code {
    Code { code: code.into(), code_type: code_type.map(Into::into) }
}

fn error_csv(input: &str) -> Error {
    error(input, Format::Csv)
}

fn error(input: &str, format: Format) -> Error {
    let mut parser = Parser::new(input.as_bytes(), format);
    let e = parser.next().unwrap().unwrap_err();
    assert!(parser.next().is_none(), "fused after Err");
    e
}

#[test]
fn tall_2_0_0() {
    let (records, metadata) = read("tall-2.0.0.csv");
    assert_eq!(
        metadata,
        Metadata {
            layout: Some(Layout::Tall),
            version: s("2.0.0"),
            hospital_name: s("West Mercy Hospital"),
            last_updated_on: s("2024-07-01"),
            location_name: list(&["West Mercy Hospital", "West Mercy Surgical Center"]),
            hospital_address: list(&[
                "12 Main Street, Fullerton, CA 92832",
                "23 Ocean Ave, San Jose, CA 94088"
            ]),
            license_number: s("50056"),
            license_state: s("CA"),
            attestation: s(AFFIRMATION),
            attestation_confirmed: s("true"),
            ..Metadata::default()
        }
    );
    // The blank row on line 6 is skipped.
    assert_eq!(records.iter().map(|r| r.position).collect::<Vec<_>>(), [4, 5, 7, 8]);
    let hip = Charge {
        description: s("Major hip and knee joint replacement"),
        codes: vec![code("470", Some("MS-DRG")), code("175869", Some("LOCAL"))],
        setting: s("inpatient"),
        billing_class: s("facility"),
        gross_charge: s("25000"),
        discounted_cash: s("20000"),
        minimum: s("18000"),
        maximum: s("22000"),
        ..Charge::default()
    };
    assert_eq!(*records[0].charge, hip);
    assert_eq!(
        records[1].payer,
        Some(Payer {
            name: s("Region Health"),
            plan: s("HMO"),
            percentage: s("60"),
            methodology: s("percent of total billed charges"),
            ..Payer::default()
        })
    );
    assert_eq!(records[1].charge.notes, s("Only for members"));
    assert_eq!(records[2].charge.codes, [code("99213", None)]);
    assert_eq!(records[3].payer, None);
    assert_eq!(records[3].charge.notes, s("Cash price only"));
}

#[test]
fn tall_2_2_0() {
    let (records, metadata) = read("tall-2.2.0.csv");
    assert_eq!(metadata.hospital_address, ["100 Lake Rd, Albany, NY 12207"]);
    assert_eq!((metadata.license_number, metadata.license_state), (s("H-0042"), s("NY")));
    assert_eq!(metadata.attestation_confirmed, s("TRUE"));
    assert_eq!(records.iter().map(|r| r.position).collect::<Vec<_>>(), [4, 5, 6]);
    let drug = &records[0].charge;
    assert_eq!((&drug.drug_unit, &drug.drug_unit_type), (&s("2"), &s("ME")));
    assert_eq!(records[1].charge.modifiers, ["50", "LT"]);
    assert_eq!(records[1].payer.as_ref().unwrap().estimated_amount, s("3150.00"));
    // A modifier row: no code, still a record.
    let modifier = &records[2];
    assert_eq!(modifier.charge.codes, []);
    assert_eq!(modifier.charge.modifiers, ["50"]);
    assert_eq!(modifier.charge.notes, s("Modifier 50 pays 150 percent of the fee schedule"));
    assert_eq!(modifier.payer.as_ref().unwrap().percentage, s("150"));
}

#[test]
fn tall_3_0_0() {
    let (records, metadata) = read("tall-3.0.0.csv");
    assert_eq!(metadata.hospital_name, s("North Valley Hospital"));
    assert_eq!(metadata.location_name, ["North Valley Hospital", "North Valley Clinic"]);
    assert_eq!(metadata.type_2_npi, ["1982621447", "1154343150"]);
    assert!(metadata.attestation.unwrap().starts_with("To the best of its knowledge"));
    assert_eq!(metadata.attestation_confirmed, s("true"));
    assert_eq!(metadata.attester_name, s("Jane Roe"));
    assert_eq!(records.iter().map(|r| r.position).collect::<Vec<_>>(), [4, 5, 6]);
    assert_eq!(
        records[1].payer,
        Some(Payer {
            name: s("Region Health"),
            plan: s("Silver HMO"),
            percentage: s("70"),
            methodology: s("percent of total billed charges"),
            median_amount: s("1250.00"),
            percentile_10th: s("980.00"),
            percentile_90th: s("1600.00"),
            count: s("25"),
            ..Payer::default()
        })
    );
    assert_eq!(
        records[2].charge.codes,
        [code("J1815", Some("HCPCS")), code("0002-7715-01", Some("NDC"))]
    );
}

#[test]
fn header_errors() {
    let message = |e: Error| match e {
        Error::Csv { line, message } => (line, message),
        e => panic!("{e}"),
    };
    assert_eq!(message(error_csv("")).0, 1);
    assert_eq!(message(error_csv("hospital_name\nWest\n")).0, 3);
    assert_eq!(message(error_csv("a\nb\n\nx\n")), (3, "row 3 (the column names) is blank".into()));
    let both = "a\nb\npayer_name,plan_name,count | Acme | PPO\n";
    assert!(message(error_csv(both)).1.contains("has both payer_name"));
    let neither = "a\nb\ndescription,payer_name,standard_charge | Acme | negotiated_dollar\n";
    assert!(message(error_csv(neither)).1.contains("has neither payer_name"));
}

#[test]
fn wide_2_2_0() {
    let (records, metadata) = read("wide-2.2.0.csv");
    assert_eq!(metadata.layout, Some(Layout::Wide));
    // Both blocks, the first only, none.
    assert_eq!(records.iter().map(|r| r.position).collect::<Vec<_>>(), [4, 4, 5, 6]);
    assert!(Arc::ptr_eq(&records[0].charge, &records[1].charge));
    assert_eq!(records[0].charge.codes, [code("470", Some("MS-DRG"))]);
    // `Aetna | PPO` and `aetna | ppo` columns are one block, named as first written.
    assert_eq!(
        records[0].payer,
        Some(Payer {
            name: s("Aetna"),
            plan: s("PPO"),
            dollar: s("20000"),
            methodology: s("case rate"),
            ..Payer::default()
        })
    );
    assert_eq!(
        records[1].payer,
        Some(Payer {
            name: s("Region Health"),
            plan: s("HMO"),
            percentage: s("60"),
            estimated_amount: s("23145.98"),
            methodology: s("percent of total billed charges"),
            notes: s("Excludes implants"),
            ..Payer::default()
        })
    );
    assert_eq!(records[2].payer.as_ref().unwrap().name, s("Aetna"));
    assert_eq!(records[3].payer, None);
    assert_eq!(records[3].charge.notes, s("Cash price only"));
}

#[test]
fn wide_3_0_0() {
    let (records, metadata) = read("wide-3.0.0.csv");
    assert_eq!(metadata.layout, Some(Layout::Wide));
    assert_eq!(records.iter().map(|r| r.position).collect::<Vec<_>>(), [4, 4, 5]);
    assert_eq!(
        records[1].payer,
        Some(Payer {
            name: s("Region Health"),
            plan: s("Silver HMO"),
            percentage: s("70"),
            median_amount: s("1250.00"),
            percentile_10th: s("980.00"),
            percentile_90th: s("1600.00"),
            count: s("25"),
            methodology: s("percent of total billed charges"),
            notes: s("Paid at 70 percent"),
            ..Payer::default()
        })
    );
    let modifier = &records[2];
    assert_eq!(modifier.charge.modifiers, ["50"]);
    let payer = modifier.payer.as_ref().unwrap();
    assert_eq!((&payer.name, &payer.count), (&s("Acme"), &s("1 through 10")));
}

#[test]
fn blank_header_rows() {
    let input = ",\n,\ndescription,payer_name,plan_name\nVisit,,\n";
    let mut parser = Parser::new(input.as_bytes(), Format::Csv);
    let record = parser.next().unwrap().unwrap();
    assert_eq!(record.charge.description, s("Visit"));
    assert_eq!(record.payer, None);
    assert!(parser.next().is_none());
    assert_eq!(parser.metadata().hospital_name, None);
}

#[test]
fn json_2_2_0() {
    let (records, metadata) = read("sample-2.2.0.json");
    assert_eq!(
        metadata,
        Metadata {
            layout: Some(Layout::Json),
            version: s("2.2.0"),
            hospital_name: s("West Mercy Hospital"),
            last_updated_on: s("2024-07-01"),
            location_name: list(&["West Mercy Hospital", "West Mercy Surgical Center"]),
            hospital_address: list(&["12 Main Street, Fullerton, CA 92832"]),
            license_number: s("50056"),
            license_state: s("CA"),
            attestation: s(AFFIRMATION),
            attestation_confirmed: s("true"),
            modifiers: vec![ModifierInfo {
                code: s("50"),
                description: s("Bilateral procedure"),
                setting: None,
                payers: vec![ModifierPayer {
                    name: s("Region Health"),
                    plan: s("HMO"),
                    description: s("150 percent of the fee schedule"),
                }],
            }],
            ..Metadata::default()
        }
    );
    // Two payers, then a standard charge without any, then the second item.
    assert_eq!(records.iter().map(|r| r.position).collect::<Vec<_>>(), [0, 0, 0, 1]);
    assert!(Arc::ptr_eq(&records[0].charge, &records[1].charge));
    assert!(!Arc::ptr_eq(&records[1].charge, &records[2].charge));
    let hip = Charge {
        description: s("Major hip and knee joint replacement"),
        codes: vec![code("470", Some("MS-DRG")), code("175869", Some("LOCAL"))],
        setting: s("inpatient"),
        gross_charge: s("25000"),
        discounted_cash: s("20000"),
        minimum: s("18000"),
        maximum: s("22000"),
        ..Charge::default()
    };
    assert_eq!(*records[1].charge, hip);
    assert_eq!(
        records[1].payer,
        Some(Payer {
            name: s("Region Health"),
            plan: s("HMO"),
            percentage: s("60"),
            estimated_amount: s("23145.98"),
            methodology: s("percent of total billed charges"),
            notes: s("Excludes implants"),
            ..Payer::default()
        })
    );
    assert_eq!(records[2].payer, None);
    assert_eq!(records[2].charge.notes, s("Cash price only"));
    assert_eq!(records[2].charge.setting, s("outpatient"));
    assert_eq!(
        *records[3].charge,
        Charge {
            description: s("Infliximab 10 mg"),
            codes: vec![code("J1745", Some("HCPCS"))],
            setting: s("outpatient"),
            drug_unit: s("2"),
            drug_unit_type: s("ME"),
            gross_charge: s("150"),
            discounted_cash: s("120"),
            minimum: s("90"),
            maximum: s("130"),
            ..Charge::default()
        }
    );
}

#[test]
fn json_3_0_0() {
    let (records, metadata) = read("sample-3.0.0.json");
    assert_eq!(metadata.location_name, ["North Valley Hospital", "North Valley Clinic"]);
    assert_eq!(metadata.type_2_npi, ["1982621447", "1154343150"]);
    assert_eq!((&metadata.license_number, &metadata.license_state), (&None, &s("NY")));
    assert!(metadata.attestation.unwrap().starts_with("To the best of its knowledge"));
    assert_eq!(metadata.attester_name, s("Jane Roe"));
    assert_eq!(metadata.modifiers[0].setting, s("outpatient"));
    assert_eq!(records.len(), 2);
    let charge = &records[0].charge;
    assert_eq!(charge.gross_charge, s("1.50"));
    assert_eq!(charge.discounted_cash, s("1.2"));
    assert_eq!(charge.modifiers, ["50", "LT"]);
    assert_eq!(records[0].payer.as_ref().unwrap().dollar, s("1.25"));
    assert_eq!(
        records[1].payer,
        Some(Payer {
            name: s("Region Health"),
            plan: s("Silver HMO"),
            percentage: s("70"),
            methodology: s("percent of total billed charges"),
            median_amount: s("1250"),
            percentile_10th: s("980"),
            percentile_90th: s("1600"),
            count: s("25"),
            ..Payer::default()
        })
    );
}

#[test]
fn json_metadata_after_items() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/metadata-after.json");
    let mut parser = Parser::new(std::fs::File::open(path).unwrap(), Format::Json);
    let record = parser.next().unwrap().unwrap();
    assert_eq!((record.position, record.payer), (0, None));
    assert_eq!(parser.metadata().hospital_name, None);
    assert!(parser.next().is_none());
    assert_eq!(parser.metadata().hospital_name, s("Lake Hospital"));
    assert_eq!(parser.metadata().license_number, s("H-0042"));
}

#[test]
fn json_errors() {
    let position = |e: Error| match e {
        Error::Json { line, column, .. } => (line, column),
        e => panic!("{e}"),
    };
    assert_eq!(position(error("{\n\"a\": 1,\n\"b\": tru}", Format::Json)), (3, 9));
    let truncated = "{\"hospital_name\": \"X\", \"standard_charge_information\": [";
    assert_eq!(position(error(truncated, Format::Json)), (1, 56));
    assert_eq!(position(error("", Format::Json)), (1, 1));
    // An object's members are not taken for header fields.
    let object = "{\"standard_charge_information\": {\"version\": \"2.2.0\"}}";
    let mut parser = Parser::new(object.as_bytes(), Format::Json);
    let Some(Err(Error::Json { message, .. })) = parser.next() else { panic!() };
    assert_eq!(message, "standard_charge_information is not an array");
    assert_eq!(parser.metadata().version, None);
}

#[test]
fn aliases() {
    type Pairs = &'static [(&'static str, &'static str)];
    let cases: &[(&str, Pairs, Pairs)] = &[
        (
            "tall-3.0.0.csv",
            &[("standard_charge | gross,", "Gross_Charges ,"), ("median_amount", "median")],
            &[("gross_charges", "standard_charge | gross"), ("median", "median_amount")],
        ),
        (
            "wide-3.0.0.csv",
            &[("| negotiated_dollar", "| negotiated_dollars"), ("median_amount |", "median |")],
            &[
                ("standard_charge | negotiated_dollars", "standard_charge | negotiated_dollar"),
                ("median", "median_amount"),
                ("unused", "setting"),
            ],
        ),
        (
            "sample-3.0.0.json",
            &[("\"payers_information\"", "\"payers\""), ("\"modifier_code\"", "\"modifiers\"")],
            &[("payers", "payers_information"), ("modifiers", "modifier_code")],
        ),
    ];
    for &(name, replace, aliases) in cases {
        let (records, metadata) = read(name);
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        let mut text = std::fs::read_to_string(path).unwrap();
        for (from, to) in replace {
            assert!(text.contains(from), "{name}: {from}");
            text = text.replace(from, to);
        }
        let format = if name.ends_with("json") { Format::Json } else { Format::Csv };
        let pairs = aliases.iter().map(|&(a, t)| (a.to_owned(), t.to_owned()));
        let mut parser = Parser::new(text.as_bytes(), format).with_aliases(pairs);
        let got = parser.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(got, records, "{name}");
        let mut got = parser.metadata().clone();
        let used = std::mem::take(&mut got.aliases_used);
        assert_eq!(got, metadata, "{name}");
        let expected = aliases.iter().filter(|(a, _)| *a != "unused");
        let expected = expected.map(|&(a, t)| (a.to_owned(), t.to_owned())).collect();
        assert_eq!(used, expected, "{name}");
    }
}

/// A file with both a name and an alias of it reads the name, in either order.
#[test]
fn alias_gives_way() {
    let aliases = || [("gross_charges".to_owned(), "standard_charge | gross".to_owned())];
    for row3 in ["gross_charges,standard_charge | gross", "standard_charge | gross,gross_charges"] {
        let file = format!("version\n3.0.0\ndescription,payer_name,plan_name,{row3}\nX,,,1,2\n");
        let mut parser = Parser::new(file.as_bytes(), Format::Csv).with_aliases(aliases());
        let record = parser.next().unwrap().unwrap();
        let expected = if row3.starts_with("gross") { "2" } else { "1" };
        assert_eq!(record.charge.gross_charge.as_deref(), Some(expected), "{row3}");
        assert_eq!(parser.metadata().aliases_used.len(), 1);
    }
    let aliases = [("modifiers".to_owned(), "modifier_code".to_owned())];
    let file = r#"{"standard_charge_information": [{"standard_charges": [
        {"modifiers": ["A"], "modifier_code": ["B"]}, {"modifier_code": ["B"], "modifiers": ["A"]},
        {"modifiers": ["A"]}]}]}"#;
    let parser = Parser::new(file.as_bytes(), Format::Json).with_aliases(aliases);
    let modifiers: Vec<_> = parser.map(|r| r.unwrap().charge.modifiers.clone()).collect();
    assert_eq!(modifiers, [list(&["B"]), list(&["B"]), list(&["A"])]);
}
