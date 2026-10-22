//! CSV files: row 1 names the header fields, row 2 holds their values, row 3 names the
//! data columns, and every row after it is a charge item. Column names compare per
//! `|`-separated part, trimmed and case-insensitive, so `Code|1` is `code | 1`.
//!
//! A tall file has one row per item and payer/plan, named in `payer_name` and `plan_name`.
//! A wide file has one row per item and a column block per payer/plan, the payer and plan
//! written into the column names: `standard_charge | <payer> | <plan> | negotiated_dollar`.

mod reader;

use crate::{Charge, Code, Error, Layout, Metadata, Payer, Record, text};
use reader::Reader;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::sync::Arc;

pub struct Parser<R> {
    reader: Reader<R>,
    pub metadata: Metadata,
    /// Set once rows 1 to 3 are read.
    columns: Option<Columns>,
    /// The rest of a wide row's records.
    pending: VecDeque<Record>,
}

/// Where row 3 puts each field.
struct Columns {
    /// By column index; `None` for a column the parser does not read.
    slots: Vec<Option<Slot>>,
    /// The `code | i` pairs, in the order of `i`.
    codes: usize,
    /// Each payer/plan block's payer and plan as row 3 first writes them; a tall file has
    /// one block, named in its cells.
    blocks: Vec<(Option<String>, Option<String>)>,
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
    /// A field of the payer/plan block with this index.
    Payer(usize, Kind),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Name,
    Plan,
    Dollar,
    Percentage,
    Algorithm,
    Methodology,
    Notes,
    EstimatedAmount,
    MedianAmount,
    Percentile10th,
    Percentile90th,
    Count,
}

impl<R: Read> Parser<R> {
    pub fn new(input: R) -> Self {
        Parser {
            reader: Reader::new(input),
            metadata: Metadata::default(),
            columns: None,
            pending: VecDeque::new(),
        }
    }

    pub fn next(&mut self) -> Option<Result<Record, Error>> {
        if let Some(record) = self.pending.pop_front() {
            return Some(Ok(record));
        }
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
            let Some((charge, payers)) = columns.record(row.non_empty()) else { continue };
            let charge = Arc::new(charge);
            if payers.is_empty() {
                return Some(Ok(Record { charge, payer: None, position }));
            }
            self.pending.extend(payers.into_iter().map(|payer| Record {
                charge: Arc::clone(&charge),
                payer: Some(payer),
                position,
            }));
            return self.pending.pop_front().map(Ok);
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
        let (columns, layout) =
            Columns::new(&row3).map_err(|message| Error::Csv { line, message: message.into() })?;
        self.metadata.layout = Some(layout);
        Ok(columns)
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
    /// From row 3's names; when a name repeats, its first column is read.
    fn new(names: &[String]) -> Result<(Self, Layout), &'static str> {
        let keys: Vec<String> = names.iter().map(|n| key(n)).collect();
        let payer: Vec<_> = names.iter().map(|n| payer_column(n)).collect();
        let tall = [Kind::Name, Kind::Plan].iter().all(|k| payer.contains(&Some((*k, None))));
        let wide = payer.iter().any(|p| matches!(p, Some((_, Some(_)))));
        let layout = match (tall, wide) {
            (true, false) => Layout::Tall,
            (false, true) => Layout::Wide,
            (true, true) => {
                return Err("row 3 has both payer_name and plan_name (tall) and \
                    payer-specific columns (wide)");
            }
            (false, false) => {
                return Err("row 3 has neither payer_name and plan_name (tall) nor \
                    payer-specific columns (wide)");
            }
        };
        // Code numbers sort as numbers when they are written without leading zeros.
        let mut numbers: Vec<&str> =
            keys.iter().filter_map(|k| code_column(k)).map(|c| c.0).collect();
        numbers.sort_by_key(|n| (n.len(), *n));
        numbers.dedup();
        let mut blocks = Vec::new();
        if tall {
            blocks.push((None, None));
        }
        // Blocks are told apart by payer and plan lower-cased.
        let mut block_index = HashMap::new();
        let mut seen = HashSet::new();
        let slots = (keys.iter().zip(payer))
            .map(|(k, payer)| {
                let slot = match (k.as_str(), payer) {
                    (_, Some((kind, None))) if tall => Slot::Payer(0, kind),
                    (_, Some((kind, Some((name, plan))))) if wide => {
                        let key = format!("{name} | {plan}").to_lowercase();
                        let block = *block_index.entry(key).or_insert_with(|| {
                            let text = |s: &str| (!s.is_empty()).then(|| s.to_owned());
                            blocks.push((text(name), text(plan)));
                            blocks.len() - 1
                        });
                        Slot::Payer(block, kind)
                    }
                    (_, Some(_)) => return None,
                    ("DESCRIPTION", None) => Slot::Description,
                    ("SETTING", None) => Slot::Setting,
                    ("BILLING_CLASS", None) => Slot::BillingClass,
                    ("DRUG_UNIT_OF_MEASUREMENT", None) => Slot::DrugUnit,
                    ("DRUG_TYPE_OF_MEASUREMENT", None) => Slot::DrugUnitType,
                    ("MODIFIERS", None) => Slot::Modifiers,
                    ("STANDARD_CHARGE|GROSS", None) => Slot::Gross,
                    ("STANDARD_CHARGE|DISCOUNTED_CASH", None) => Slot::DiscountedCash,
                    ("STANDARD_CHARGE|MIN", None) => Slot::Minimum,
                    ("STANDARD_CHARGE|MAX", None) => Slot::Maximum,
                    ("ADDITIONAL_GENERIC_NOTES", None) => Slot::Notes,
                    (k, None) => {
                        let (n, is_type) = code_column(k)?;
                        let i = numbers.iter().position(|m| *m == n).expect("collected above");
                        if is_type { Slot::CodeType(i) } else { Slot::Code(i) }
                    }
                };
                seen.insert(slot).then_some(slot)
            })
            .collect();
        Ok((Columns { slots, codes: numbers.len(), blocks }, layout))
    }

    /// A data row's charge and the payers of its blocks with a value, in block order, or
    /// `None` for a blank row.
    fn record<'a>(
        &self,
        cells: impl Iterator<Item = (usize, &'a str)>,
    ) -> Option<(Charge, Vec<Payer>)> {
        let mut charge = Charge::default();
        let mut payers: Vec<Option<Payer>> = vec![None; self.blocks.len()];
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
                Slot::Payer(b, kind) => {
                    let payer = payers[b].get_or_insert_with(|| {
                        let (name, plan) = self.blocks[b].clone();
                        Payer { name, plan, ..Payer::default() }
                    });
                    let field = match kind {
                        Kind::Name => &mut payer.name,
                        Kind::Plan => &mut payer.plan,
                        Kind::Dollar => &mut payer.dollar,
                        Kind::Percentage => &mut payer.percentage,
                        Kind::Algorithm => &mut payer.algorithm,
                        Kind::Methodology => &mut payer.methodology,
                        Kind::Notes => &mut payer.notes,
                        Kind::EstimatedAmount => &mut payer.estimated_amount,
                        Kind::MedianAmount => &mut payer.median_amount,
                        Kind::Percentile10th => &mut payer.percentile_10th,
                        Kind::Percentile90th => &mut payer.percentile_90th,
                        Kind::Count => &mut payer.count,
                    };
                    *field = v;
                }
            }
        }
        if blank {
            return None;
        }
        charge.codes = (codes.into_iter())
            .filter_map(|(code, code_type)| Some(Code { code: code?, code_type }))
            .collect();
        Some((charge, payers.into_iter().flatten().collect()))
    }
}

/// A payer-specific column's field, with the payer and plan in a wide file's names:
/// `standard_charge | negotiated_dollar` or `count` in a tall file,
/// `standard_charge | <payer> | <plan> | negotiated_dollar` or `count | <payer> | <plan>`
/// in a wide one.
fn payer_column(name: &str) -> Option<(Kind, Option<(&str, &str)>)> {
    let parts: Vec<&str> = parts(name).collect();
    let upper: Vec<String> = parts.iter().map(|p| p.to_uppercase()).collect();
    let charge = |s: &str| match s {
        "NEGOTIATED_DOLLAR" => Some(Kind::Dollar),
        "NEGOTIATED_PERCENTAGE" => Some(Kind::Percentage),
        "NEGOTIATED_ALGORITHM" => Some(Kind::Algorithm),
        "METHODOLOGY" => Some(Kind::Methodology),
        _ => None,
    };
    let amount = |s: &str| match s {
        "ADDITIONAL_PAYER_NOTES" => Some(Kind::Notes),
        "ESTIMATED_AMOUNT" => Some(Kind::EstimatedAmount),
        "MEDIAN_AMOUNT" => Some(Kind::MedianAmount),
        "10TH_PERCENTILE" => Some(Kind::Percentile10th),
        "90TH_PERCENTILE" => Some(Kind::Percentile90th),
        "COUNT" => Some(Kind::Count),
        _ => None,
    };
    let upper: Vec<&str> = upper.iter().map(String::as_str).collect();
    match (&upper[..], &parts[..]) {
        (["PAYER_NAME"], _) => Some((Kind::Name, None)),
        (["PLAN_NAME"], _) => Some((Kind::Plan, None)),
        (["STANDARD_CHARGE", c], _) => Some((charge(c)?, None)),
        ([a], _) => Some((amount(a)?, None)),
        (["STANDARD_CHARGE", _, _, c], [_, payer, plan, _]) => {
            Some((charge(c)?, Some((*payer, *plan))))
        }
        ([a, _, _], [_, payer, plan]) => Some((amount(a)?, Some((*payer, *plan)))),
        _ => None,
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
