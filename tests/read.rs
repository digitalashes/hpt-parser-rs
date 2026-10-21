#![expect(clippy::unwrap_used, reason = "a broken fixture should fail the test")]

use hpt_parser::{Charge, Code, Error, Format, Layout, Metadata, Parser, Payer, Record};

const AFFIRMATION: &str = "To the best of its knowledge and belief, the hospital has included all applicable standard charge information in accordance with the requirements of 45 CFR 180.50, and the information encoded is true, accurate, and complete as of the date indicated.";

fn read(name: &str) -> (Vec<Record>, Metadata) {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let mut parser = Parser::new(std::fs::File::open(path).unwrap(), Format::Csv);
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

fn error(input: &str) -> Error {
    let mut parser = Parser::new(input.as_bytes(), Format::Csv);
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
    assert_eq!(message(error("")).0, 1);
    assert_eq!(message(error("hospital_name\nWest\n")).0, 3);
    assert_eq!(message(error("a\nb\n\nx\n")), (3, "row 3 (the column names) is blank".into()));
    let wide = "a\nb\ndescription,standard_charge | Acme | PPO | negotiated_dollar\n";
    assert!(message(error(wide)).1.contains("neither payer_name"));
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
