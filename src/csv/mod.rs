//! CSV files: row 1 names the header fields, row 2 holds their values, row 3 names the
//! data columns, and every row after it is a charge item. Column names compare per
//! `|`-separated part, trimmed and case-insensitive, so `Code|1` is `code | 1`.
//!
//! A tall file has one row per item and payer/plan, named in `payer_name` and `plan_name`.
//! A wide file has one row per item and a column block per payer/plan, the payer and plan
//! written into the column names: `standard_charge | <payer> | <plan> | negotiated_dollar`,
//! or `standard_charge | <payer> | negotiated_dollar` in files that leave the plan out.

mod reader;

use crate::{Aliases, Charge, Code, Error, Layout, Metadata, Payer, Record, text};
use reader::Reader;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::Read;
use std::sync::Arc;

/// Bytes of cells in a [`Rows`].
#[cfg(feature = "parquet")]
const ROWS: usize = 64 * 1024;

pub struct Parser<R> {
    reader: Reader<R>,
    pub metadata: Metadata,
    pub aliases: Aliases,
    /// Set once rows 1 to 3 are read.
    columns: Option<Arc<Columns>>,
    /// One entry per payer/plan block, `None` between rows.
    payers: Vec<Option<Payer>>,
    /// An error met after the rows `next_rows` returned last. Its `dyn Error` would make the
    /// parser not unwind safe, but it is only kept to be returned: no panic can leave it half
    /// changed.
    #[cfg(feature = "parquet")]
    error: std::panic::AssertUnwindSafe<Option<Error>>,
}

/// Data rows copied out of the reader, so that another thread makes their records.
#[cfg(feature = "parquet")]
pub(crate) struct Rows {
    columns: Arc<Columns>,
    /// The rows' non-empty cells back to back.
    text: String,
    /// Each cell's index and byte range in `text`.
    cells: Vec<(usize, usize, usize)>,
    /// Each row's line and the end of its cells in `cells`.
    lines: Vec<(u64, usize)>,
}

#[cfg(feature = "parquet")]
impl Rows {
    /// Queues the rows' records, as `Parser::fill` does.
    pub(crate) fn records(&self, pending: &mut VecDeque<Record>) {
        let mut payers = vec![None; self.columns.blocks.len()];
        let mut from = 0;
        for &(line, to) in &self.lines {
            let cells =
                self.cells[from..to].iter().map(|&(i, start, end)| (i, &self.text[start..end]));
            from = to;
            if let Some(charge) = self.columns.record(cells, &mut payers) {
                crate::fan_out(pending, charge, payers.iter_mut().filter_map(Option::take), line);
            }
        }
    }
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
            aliases: Aliases::default(),
            columns: None,
            payers: Vec::new(),
            #[cfg(feature = "parquet")]
            error: std::panic::AssertUnwindSafe(None),
        }
    }

    /// The next data rows, about 64 KiB of cells; `None` at the end. An IO error comes after
    /// the rows read before it.
    #[cfg(feature = "parquet")]
    pub fn next_rows(&mut self) -> Result<Option<Rows>, Error> {
        if let Some(e) = self.error.take() {
            return Err(e);
        }
        if self.columns.is_none() {
            self.columns = Some(Arc::new(self.header()?));
        }
        let columns = Arc::clone(self.columns.as_ref().expect("set above"));
        let mut rows = Rows {
            columns,
            text: String::with_capacity(ROWS),
            cells: Vec::new(),
            lines: Vec::new(),
        };
        while rows.text.len() < ROWS {
            match self.reader.next_row() {
                Ok(Some(row)) => {
                    let line = row.line;
                    for (i, cell) in row.non_empty() {
                        let start = rows.text.len();
                        rows.text.push_str(cell);
                        rows.cells.push((i, start, rows.text.len()));
                    }
                    rows.lines.push((line, rows.cells.len()));
                }
                Ok(None) => break,
                Err(e) if rows.lines.is_empty() => return Err(e.into()),
                Err(e) => {
                    *self.error = Some(e.into());
                    break;
                }
            }
        }
        Ok((!rows.lines.is_empty()).then_some(rows))
    }

    /// Reads up to the next data row that is not blank and queues its records; false at
    /// the end.
    pub fn fill(&mut self, pending: &mut VecDeque<Record>) -> Result<bool, Error> {
        if self.columns.is_none() {
            self.columns = Some(Arc::new(self.header()?));
        }
        let columns = self.columns.as_ref().expect("set above");
        while let Some(row) = self.reader.next_row()? {
            let position = row.line;
            if let Some(charge) = columns.record(row.non_empty(), &mut self.payers) {
                let payers = self.payers.iter_mut().filter_map(Option::take);
                crate::fan_out(pending, charge, payers, position);
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Rows 1 to 3: the header into `metadata`, the column names into `Columns`.
    fn header(&mut self) -> Result<Columns, Error> {
        let mut names = self.names()?;
        rename(&mut names, &mut self.aliases, &mut self.metadata.aliases_used);
        let values = self.names()?;
        for (i, value) in values.iter().enumerate() {
            if let Some(name) = names.get(i) {
                header_field(&mut self.metadata, name, value);
            }
        }
        let line = self.reader.line;
        let mut row3 = self.names()?;
        rename(&mut row3, &mut self.aliases, &mut self.metadata.aliases_used);
        if row3.iter().all(String::is_empty) {
            return Err(Error::Csv { line, message: "row 3 (the column names) is blank".into() });
        }
        let (columns, layout) =
            Columns::new(&row3).map_err(|message| Error::Csv { line, message: message.into() })?;
        self.metadata.layout = Some(layout);
        self.payers = vec![None; columns.blocks.len()];
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

/// Replaces the aliases among a header row's names by the names they stand for, unless the
/// row has that name too. In a wide file's payer-specific column the alias is the name
/// without its payer and plan.
fn rename(names: &mut [String], aliases: &mut Aliases, used: &mut BTreeMap<String, String>) {
    if aliases.0.is_empty() {
        return;
    }
    let keys: HashSet<String> = names.iter().map(|n| key(n)).collect();
    for name in names {
        let p: Vec<&str> = parts(name).collect();
        let whole = key(name);
        let field = match p[..] {
            [a, _, _] => Some(key(a)),
            [a, _, _, b] => Some(key(&format!("{a}|{b}"))),
            _ => None,
        };
        let (lookup, renamed) = if let Some(alias) = aliases.get(&whole) {
            (whole, alias.target.clone())
        } else if let Some(field) = field
            && let Some(alias) = aliases.get(&field)
        {
            // Only a target of the alias's shape takes the payer and plan back.
            let renamed = match (&p[..], &parts(&alias.target).collect::<Vec<_>>()[..]) {
                ([_, payer, plan], [a]) => format!("{a} | {payer} | {plan}"),
                ([_, payer, plan, _], [a, b]) => format!("{a} | {payer} | {plan} | {b}"),
                _ => continue,
            };
            (field, renamed)
        } else {
            continue;
        };
        aliases.get(&lookup).expect("found above").take(used);
        if !keys.contains(&key(&renamed)) {
            *name = renamed;
        }
    }
}

/// Whether row 1 or row 3 may name a column `name`.
#[cfg(feature = "config")]
pub(crate) fn known(name: &str) -> bool {
    let key = key(name);
    let mut header = Metadata::default();
    header_field(&mut header, name, "x");
    header != Metadata::default()
        || column(&key).is_some()
        || code_column(&key).is_some()
        || payer_column(name).is_some()
}

/// One header field from row 1's `name` and row 2's `value`. Unknown names are ignored.
fn header_field(m: &mut Metadata, name: &str, value: &str) {
    let text = non_empty(value);
    let key = key(name);
    match key.as_str() {
        "HOSPITAL_NAME" => m.hospital_name = text,
        "LAST_UPDATED_ON" => m.last_updated_on = text,
        "VERSION" => m.version = text,
        "HOSPITAL_ADDRESS" => m.hospital_address = text::split(value, "|"),
        "LOCATION_NAME" | "HOSPITAL_LOCATION" => m.location_name.extend(text::split(value, "|")),
        "TYPE_2_NPI" => m.type_2_npi = text::split(value, "|"),
        "ATTESTER_NAME" => m.attester_name = text,
        _ if key.starts_with("LICENSE_NUMBER|") => {
            m.license_number = text;
            m.license_state = parts(name).nth(1).and_then(non_empty);
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
        // A payer without a plan makes a file wide only when it is not tall.
        let wide = payer
            .iter()
            .any(|p| matches!(p, Some((_, Some((_, plan)))) if !tall || !plan.is_empty()));
        if tall == wide {
            return Err(if tall {
                "row 3 has both payer_name and plan_name (tall) and payer-specific columns (wide)"
            } else {
                "row 3 has neither payer_name and plan_name (tall) nor payer-specific columns \
                    (wide)"
            });
        }
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
                // A payer column of the other layout is not read.
                let slot = match payer {
                    Some((kind, None)) if tall => Slot::Payer(0, kind),
                    Some((kind, Some((name, plan)))) if !tall => {
                        let key = format!("{name} | {plan}").to_lowercase();
                        let block = *block_index.entry(key).or_insert_with(|| {
                            blocks.push((non_empty(name), non_empty(plan)));
                            blocks.len() - 1
                        });
                        Slot::Payer(block, kind)
                    }
                    Some(_) => return None,
                    None if let Some(slot) = column(k) => slot,
                    None => {
                        let (n, is_type) = code_column(k)?;
                        let i = numbers.iter().position(|m| *m == n).expect("collected");
                        if is_type { Slot::CodeType(i) } else { Slot::Code(i) }
                    }
                };
                seen.insert(slot).then_some(slot)
            })
            .collect();
        let layout = if tall { Layout::Tall } else { Layout::Wide };
        Ok((Columns { slots, codes: numbers.len(), blocks }, layout))
    }

    /// A data row's charge, or `None` for a blank row. The payers of blocks with a value go
    /// into `payers` by block.
    fn record<'a>(
        &self,
        cells: impl Iterator<Item = (usize, &'a str)>,
        payers: &mut [Option<Payer>],
    ) -> Option<Charge> {
        let mut charge = Charge::default();
        let mut codes: Vec<(Option<String>, Option<String>)> = vec![(None, None); self.codes];
        let mut blank = true;
        for (i, cell) in cells {
            let cell = text::trim(cell);
            if cell.is_empty() {
                continue;
            }
            blank = false;
            let Some(&Some(slot)) = self.slots.get(i) else { continue };
            let v = || Some(cell.to_owned());
            match slot {
                Slot::Description => charge.description = v(),
                Slot::Setting => charge.setting = v(),
                Slot::BillingClass => charge.billing_class = v(),
                Slot::DrugUnit => charge.drug_unit = v(),
                Slot::DrugUnitType => charge.drug_unit_type = v(),
                Slot::Modifiers => charge.modifiers = text::split(cell, "|"),
                Slot::Gross => charge.gross_charge = v(),
                Slot::DiscountedCash => charge.discounted_cash = v(),
                Slot::Minimum => charge.minimum = v(),
                Slot::Maximum => charge.maximum = v(),
                Slot::Notes => charge.notes = v(),
                Slot::Code(k) => codes[k].0 = v(),
                Slot::CodeType(k) => codes[k].1 = v(),
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
                    *field = v();
                }
            }
        }
        if blank {
            return None;
        }
        charge.codes = (codes.into_iter())
            .filter_map(|(code, code_type)| Some(Code { code: code?, code_type }))
            .collect();
        Some(charge)
    }
}

/// A charge item column's slot, from its key; codes and payer columns are matched apart.
fn column(key: &str) -> Option<Slot> {
    Some(match key {
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
        _ => return None,
    })
}

/// A payer-specific column's field, with the payer and plan in a wide file's names:
/// `standard_charge | negotiated_dollar` or `count` in a tall file,
/// `standard_charge | <payer> | <plan> | negotiated_dollar` or `count | <payer> | <plan>`
/// in a wide one. Some wide files name a payer without a plan,
/// `standard_charge | <payer> | negotiated_dollar` or `count | <payer>`: the plan is empty.
/// A template's `[payer_name]` or `[plan_name]` left in a name is no payer.
fn payer_column(name: &str) -> Option<(Kind, Option<(&str, &str)>)> {
    let up = |s: &str| s.to_uppercase();
    let charge = |s: &str| match up(s).as_str() {
        "NEGOTIATED_DOLLAR" => Some(Kind::Dollar),
        "NEGOTIATED_PERCENTAGE" => Some(Kind::Percentage),
        "NEGOTIATED_ALGORITHM" => Some(Kind::Algorithm),
        "METHODOLOGY" => Some(Kind::Methodology),
        _ => None,
    };
    let amount = |s: &str| match up(s).as_str() {
        "ADDITIONAL_PAYER_NOTES" => Some(Kind::Notes),
        "ESTIMATED_AMOUNT" => Some(Kind::EstimatedAmount),
        "MEDIAN_AMOUNT" => Some(Kind::MedianAmount),
        "10TH_PERCENTILE" => Some(Kind::Percentile10th),
        "90TH_PERCENTILE" => Some(Kind::Percentile90th),
        "COUNT" => Some(Kind::Count),
        _ => None,
    };
    let placeholder = |s: &str| ["[PAYER_NAME]", "[PLAN_NAME]"].contains(&up(s).as_str());
    let block = |payer, plan| (!placeholder(payer) && !placeholder(plan)).then_some((payer, plan));
    match parts(name).collect::<Vec<_>>()[..] {
        [a] => match up(a).as_str() {
            "PAYER_NAME" => Some((Kind::Name, None)),
            "PLAN_NAME" => Some((Kind::Plan, None)),
            _ => Some((amount(a)?, None)),
        },
        [sc, c] if up(sc) == "STANDARD_CHARGE" => Some((charge(c)?, None)),
        [a, payer] => Some((amount(a)?, Some(block(payer, "")?))),
        [sc, payer, c] if up(sc) == "STANDARD_CHARGE" => {
            Some((charge(c)?, Some(block(payer, "")?)))
        }
        [sc, payer, plan, c] if up(sc) == "STANDARD_CHARGE" => {
            Some((charge(c)?, Some(block(payer, plan)?)))
        }
        [a, payer, plan] => Some((amount(a)?, Some(block(payer, plan)?))),
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
pub(crate) fn key(name: &str) -> String {
    parts(name).map(str::to_uppercase).collect::<Vec<_>>().join("|")
}

/// `None` for an empty text.
fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_owned())
}
