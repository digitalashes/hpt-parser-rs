//! CSV files: row 1 names the header fields, row 2 holds their values, row 3 names the
//! data columns, and every row after it is a charge item. Column names compare per
//! `|`-separated part, trimmed and case-insensitive, so `Code|1` is `code | 1`.

mod reader;

use crate::{Charge, Code, Error, Layout, Metadata, Payer, Record, text};
use reader::Reader;
use std::collections::HashSet;
use std::io::Read;
use std::sync::Arc;

pub struct Parser<R> {
    reader: Reader<R>,
    pub metadata: Metadata,
    /// Set once rows 1 to 3 are read.
    columns: Option<Columns>,
}

/// Where row 3 puts each field.
struct Columns {
    /// By column index; `None` for a column the parser does not read.
    slots: Vec<Option<Slot>>,
    /// The `code | i` pairs, in the order of `i`.
    codes: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Slot {
    Description,
    Setting,
    BillingClass,
    DrugUnit,
    DrugUnitType,
    Modifiers,
    Gross,
    DiscountedCash,
    Minimum,
    Maximum,
    Notes,
    Code(usize),
    CodeType(usize),
    PayerName,
    PlanName,
    Dollar,
    Percentage,
    Algorithm,
    Methodology,
    EstimatedAmount,
    MedianAmount,
    Percentile10th,
    Percentile90th,
    Count,
}

impl<R: Read> Parser<R> {
    pub fn new(input: R) -> Self {
        Parser { reader: Reader::new(input), metadata: Metadata::default(), columns: None }
    }

    pub fn next(&mut self) -> Option<Result<Record, Error>> {
        if self.columns.is_none() {
            match self.header() {
                Ok(columns) => self.columns = Some(columns),
                Err(e) => return Some(Err(e)),
            }
        }
        let columns = self.columns.as_ref().expect("set above");
        loop {
            let row = match self.reader.next_row() {
                Ok(Some(row)) => row,
                Ok(None) => return None,
                Err(e) => return Some(Err(e.into())),
            };
            let position = row.line;
            if let Some((charge, payer)) = columns.record(row.non_empty()) {
                return Some(Ok(Record { charge: Arc::new(charge), payer, position }));
            }
        }
    }

    /// Rows 1 to 3: the header into `metadata`, the column names into `Columns`.
    fn header(&mut self) -> Result<Columns, Error> {
        let names = self.names()?;
        let values = self.names()?;
        for (i, value) in values.iter().enumerate() {
            if let Some(name) = names.get(i) {
                header_field(&mut self.metadata, name, value);
            }
        }
        let line = self.reader.line;
        let row3 = self.names()?;
        if row3.iter().all(String::is_empty) {
            return Err(Error::Csv { line, message: "row 3 (the column names) is blank".into() });
        }
        let keys: Vec<String> = row3.iter().map(|n| key(n)).collect();
        let tall = ["PAYER_NAME", "PLAN_NAME"].iter().all(|k| keys.iter().any(|c| c == k));
        if !tall {
            let message = "row 3 has neither payer_name and plan_name (tall) nor \
                payer-specific columns (wide)";
            return Err(Error::Csv { line, message: message.into() });
        }
        self.metadata.layout = Some(Layout::Tall);
        Ok(Columns::new(&keys))
    }

    /// The next row's cells, trimmed, empty ones included.
    fn names(&mut self) -> Result<Vec<String>, Error> {
        let line = self.reader.line;
        let Some(row) = self.reader.next_row()? else {
            let message = "the file ends before row 3 (the column names)";
            return Err(Error::Csv { line, message: message.into() });
        };
        let mut cells = Vec::new();
        for (i, cell) in row.non_empty() {
            cells.resize(i, String::new());
            cells.push(text::trim(cell).to_owned());
        }
        Ok(cells)
    }
}

/// One header field from row 1's `name` and row 2's `value`. Unknown names are ignored.
fn header_field(m: &mut Metadata, name: &str, value: &str) {
    let text = (!value.is_empty()).then(|| value.to_owned());
    let key = key(name);
    match key.as_str() {
        "HOSPITAL_NAME" => m.hospital_name = text,
        "LAST_UPDATED_ON" => m.last_updated_on = text,
        "VERSION" => m.version = text,
        "HOSPITAL_ADDRESS" => m.hospital_address = split_list(value),
        "LOCATION_NAME" | "HOSPITAL_LOCATION" => m.location_name.extend(split_list(value)),
        "TYPE_2_NPI" => m.type_2_npi = split_list(value),
        "ATTESTER_NAME" => m.attester_name = text,
        _ if key.starts_with("LICENSE_NUMBER|") => {
            m.license_number = text;
            m.license_state = parts(name).nth(1).filter(|s| !s.is_empty()).map(str::to_owned);
        }
        // The attestation (3.0.0) or affirmation (2.x) sentence is the column name itself.
        _ if key.starts_with("TO THE BEST OF ITS KNOWLEDGE") => {
            m.attestation = Some(name.to_owned());
            m.attestation_confirmed = text;
        }
        _ => {}
    }
}

impl Columns {
    /// From row 3's keys; when a name repeats, its first column is read.
    fn new(keys: &[String]) -> Self {
        // Code numbers sort as numbers when they are written without leading zeros.
        let mut numbers: Vec<&str> =
            keys.iter().filter_map(|k| code_column(k)).map(|c| c.0).collect();
        numbers.sort_by_key(|n| (n.len(), *n));
        numbers.dedup();
        let mut seen = HashSet::new();
        let slots = keys
            .iter()
            .map(|k| {
                let slot = match k.as_str() {
                    "DESCRIPTION" => Slot::Description,
                    "SETTING" => Slot::Setting,
                    "BILLING_CLASS" => Slot::BillingClass,
                    "DRUG_UNIT_OF_MEASUREMENT" => Slot::DrugUnit,
                    "DRUG_TYPE_OF_MEASUREMENT" => Slot::DrugUnitType,
                    "MODIFIERS" => Slot::Modifiers,
                    "STANDARD_CHARGE|GROSS" => Slot::Gross,
                    "STANDARD_CHARGE|DISCOUNTED_CASH" => Slot::DiscountedCash,
                    "STANDARD_CHARGE|MIN" => Slot::Minimum,
                    "STANDARD_CHARGE|MAX" => Slot::Maximum,
                    "ADDITIONAL_GENERIC_NOTES" => Slot::Notes,
                    "PAYER_NAME" => Slot::PayerName,
                    "PLAN_NAME" => Slot::PlanName,
                    "STANDARD_CHARGE|NEGOTIATED_DOLLAR" => Slot::Dollar,
                    "STANDARD_CHARGE|NEGOTIATED_PERCENTAGE" => Slot::Percentage,
                    "STANDARD_CHARGE|NEGOTIATED_ALGORITHM" => Slot::Algorithm,
                    "STANDARD_CHARGE|METHODOLOGY" => Slot::Methodology,
                    "ESTIMATED_AMOUNT" => Slot::EstimatedAmount,
                    "MEDIAN_AMOUNT" => Slot::MedianAmount,
                    "10TH_PERCENTILE" => Slot::Percentile10th,
                    "90TH_PERCENTILE" => Slot::Percentile90th,
                    "COUNT" => Slot::Count,
                    _ => {
                        let (n, is_type) = code_column(k)?;
                        let i = numbers.iter().position(|m| *m == n).expect("collected above");
                        if is_type { Slot::CodeType(i) } else { Slot::Code(i) }
                    }
                };
                seen.insert(slot).then_some(slot)
            })
            .collect();
        Columns { slots, codes: numbers.len() }
    }

    /// A data row's record, or `None` for a blank row.
    fn record<'a>(
        &self,
        cells: impl Iterator<Item = (usize, &'a str)>,
    ) -> Option<(Charge, Option<Payer>)> {
        let mut charge = Charge::default();
        let mut payer = Payer::default();
        let mut codes: Vec<(Option<String>, Option<String>)> = vec![(None, None); self.codes];
        let mut blank = true;
        for (i, cell) in cells {
            let cell = text::trim(cell);
            if cell.is_empty() {
                continue;
            }
            blank = false;
            let Some(&Some(slot)) = self.slots.get(i) else { continue };
            let v = Some(cell.to_owned());
            match slot {
                Slot::Description => charge.description = v,
                Slot::Setting => charge.setting = v,
                Slot::BillingClass => charge.billing_class = v,
                Slot::DrugUnit => charge.drug_unit = v,
                Slot::DrugUnitType => charge.drug_unit_type = v,
                Slot::Modifiers => charge.modifiers = split_list(cell),
                Slot::Gross => charge.gross_charge = v,
                Slot::DiscountedCash => charge.discounted_cash = v,
                Slot::Minimum => charge.minimum = v,
                Slot::Maximum => charge.maximum = v,
                Slot::Notes => charge.notes = v,
                Slot::Code(k) => codes[k].0 = v,
                Slot::CodeType(k) => codes[k].1 = v,
                Slot::PayerName => payer.name = v,
                Slot::PlanName => payer.plan = v,
                Slot::Dollar => payer.dollar = v,
                Slot::Percentage => payer.percentage = v,
                Slot::Algorithm => payer.algorithm = v,
                Slot::Methodology => payer.methodology = v,
                Slot::EstimatedAmount => payer.estimated_amount = v,
                Slot::MedianAmount => payer.median_amount = v,
                Slot::Percentile10th => payer.percentile_10th = v,
                Slot::Percentile90th => payer.percentile_90th = v,
                Slot::Count => payer.count = v,
            }
        }
        if blank {
            return None;
        }
        charge.codes = (codes.into_iter())
            .filter_map(|(code, code_type)| Some(Code { code: code?, code_type }))
            .collect();
        Some((charge, (payer != Payer::default()).then_some(payer)))
    }
}

/// `code | i` as `(i, false)`, `code | i | type` as `(i, true)`, from a column key.
fn code_column(key: &str) -> Option<(&str, bool)> {
    let parts: Vec<&str> = key.split('|').filter(|p| !p.is_empty()).collect();
    match parts[..] {
        ["CODE", n] => Some((n, false)),
        ["CODE", n, "TYPE"] => Some((n, true)),
        _ => None,
    }
}

/// A column name's `|`-separated parts, trimmed.
fn parts(name: &str) -> impl Iterator<Item = &str> {
    name.split('|').map(text::trim)
}

/// A column name compared per part, trimmed and case-insensitive.
fn key(name: &str) -> String {
    parts(name).map(str::to_uppercase).collect::<Vec<_>>().join("|")
}

/// A cell holding several values separated by `|`, trimmed, empty ones dropped.
fn split_list(value: &str) -> Vec<String> {
    parts(value).filter(|p| !p.is_empty()).map(str::to_owned).collect()
}
