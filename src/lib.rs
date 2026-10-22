//! Parser for CMS Hospital Price Transparency machine-readable files: CSV tall and wide
//! files and JSON files, data dictionary 2.0.0 to 3.0.0.
//!
//! [`Parser`] reads a file as a stream of flat [`Record`]s, one per charge item and
//! payer/plan: a tall row is one record, a wide row one per payer/plan block with a value,
//! a JSON `standard_charges` entry one per `payers_information` entry. A row or entry
//! without payer values is one record with `payer: None`. The file's header ([`Metadata`])
//! is kept apart from the records. A JSON file may put its header after the charge items:
//! [`Parser::metadata`] is complete once the parser has returned `None`.
//!
//! Values are kept as the text in the file, trimmed; an empty cell is `None`. Nothing is
//! checked or converted, so a sloppy value never fails the parse. An [`Error`] means the
//! file cannot be read at all: IO, no CSV header, JSON syntax, or a JSON file that is not
//! an object or whose `standard_charge_information` is not an array.
//!
//! ```
//! use hpt_parser::{Format, Parser};
//!
//! let file = "\
//! hospital_name,last_updated_on,version,hospital_address,license_number | NY
//! General Hospital,2026-01-01,2.2.0,1 Main St,12345
//! description,code | 1,code | 1 | type,setting,payer_name,plan_name,standard_charge | negotiated_dollar
//! Office visit,99213,CPT,outpatient,Acme,PPO,120.50
//! ";
//! let mut parser = Parser::new(file.as_bytes(), Format::Csv);
//! let record = parser.next().unwrap().unwrap();
//! assert_eq!(record.charge.codes[0].code, "99213");
//! assert_eq!(record.payer.unwrap().dollar.as_deref(), Some("120.50"));
//! assert_eq!(parser.metadata().license_state.as_deref(), Some("NY"));
//! ```

mod csv;
mod json;
mod text;

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::io::{self, Read};
use std::iter::FusedIterator;
use std::sync::Arc;

/// The input's format, given by the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Csv,
    Json,
}

/// The file's layout, known once the header is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// CSV with `payer_name` and `plan_name` columns: one row per item and payer/plan.
    Tall,
    /// CSV with a column block per payer/plan: one row per item.
    Wide,
    Json,
}

/// One charge item for one payer/plan.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    /// The item's fields, shared by every record of one CSV row or JSON `standard_charges`
    /// entry.
    pub charge: Arc<Charge>,
    /// `None` when the row has no payer-specific value.
    pub payer: Option<Payer>,
    /// The physical line the CSV row starts on, 1-based; the JSON item's index in
    /// `standard_charge_information`, 0-based.
    pub position: u64,
}

/// A charge item: what is the same for every payer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Charge {
    /// CSV `description`, JSON `description`.
    pub description: Option<String>,
    /// CSV `code | i` with `code | i | type`, JSON `code_information`. Only codes with a
    /// value, in the order of `i`.
    pub codes: Vec<Code>,
    /// CSV `setting`, JSON `setting`.
    pub setting: Option<String>,
    /// CSV `billing_class` (2.0.0, 2.1.0), JSON `billing_class`.
    pub billing_class: Option<String>,
    /// CSV `drug_unit_of_measurement`, JSON `drug_information.unit`.
    pub drug_unit: Option<String>,
    /// CSV `drug_type_of_measurement`, JSON `drug_information.type`.
    pub drug_unit_type: Option<String>,
    /// CSV `modifiers` split on `|`, JSON `modifier_code`.
    pub modifiers: Vec<String>,
    /// CSV `standard_charge | gross`, JSON `gross_charge`.
    pub gross_charge: Option<String>,
    /// CSV `standard_charge | discounted_cash`, JSON `discounted_cash`.
    pub discounted_cash: Option<String>,
    /// CSV `standard_charge | min`, JSON `minimum`.
    pub minimum: Option<String>,
    /// CSV `standard_charge | max`, JSON `maximum`.
    pub maximum: Option<String>,
    /// CSV `additional_generic_notes`, JSON `additional_generic_notes`.
    pub notes: Option<String>,
}

/// A billing code.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Code {
    /// CSV `code | i`, JSON `code`.
    pub code: String,
    /// CSV `code | i | type`, JSON `type`: `CPT`, `HCPCS`, `MS-DRG`, ...
    pub code_type: Option<String>,
}

/// The payer-specific part of a record.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Payer {
    /// CSV `payer_name`, JSON `payer_name`.
    pub name: Option<String>,
    /// CSV `plan_name`, JSON `plan_name`.
    pub plan: Option<String>,
    /// CSV `standard_charge | negotiated_dollar`, JSON `standard_charge_dollar`.
    pub dollar: Option<String>,
    /// CSV `standard_charge | negotiated_percentage`, JSON `standard_charge_percentage`.
    pub percentage: Option<String>,
    /// CSV `standard_charge | negotiated_algorithm`, JSON `standard_charge_algorithm`.
    pub algorithm: Option<String>,
    /// CSV `standard_charge | methodology`, JSON `methodology`.
    pub methodology: Option<String>,
    /// CSV and JSON `estimated_amount` (2.2.0).
    pub estimated_amount: Option<String>,
    /// CSV and JSON `median_amount` (3.0.0).
    pub median_amount: Option<String>,
    /// CSV and JSON `10th_percentile` (3.0.0).
    pub percentile_10th: Option<String>,
    /// CSV and JSON `90th_percentile` (3.0.0).
    pub percentile_90th: Option<String>,
    /// CSV and JSON `count` (3.0.0).
    pub count: Option<String>,
    /// JSON `additional_payer_notes`; tall CSV files have only `additional_generic_notes`,
    /// which is [`Charge::notes`].
    pub notes: Option<String>,
}

/// The file's header. Lists come from `|`-separated CSV cells or JSON arrays.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Metadata {
    pub layout: Option<Layout>,
    /// CSV and JSON `version`.
    pub version: Option<String>,
    /// CSV and JSON `hospital_name`.
    pub hospital_name: Option<String>,
    /// CSV and JSON `last_updated_on`.
    pub last_updated_on: Option<String>,
    /// `location_name` (3.0.0) or `hospital_location` (2.x).
    pub location_name: Vec<String>,
    /// CSV and JSON `hospital_address`.
    pub hospital_address: Vec<String>,
    /// CSV `license_number | <state>`'s value, JSON `license_information.license_number`.
    pub license_number: Option<String>,
    /// The state in the CSV column name `license_number | <state>`, JSON
    /// `license_information.state`.
    pub license_state: Option<String>,
    /// CSV and JSON `type_2_npi` (3.0.0).
    pub type_2_npi: Vec<String>,
    /// The attestation (3.0.0) or affirmation (2.x) text: in CSV the column name, in JSON
    /// `attestation.attestation` or `affirmation.affirmation`.
    pub attestation: Option<String>,
    /// Whether the hospital confirms it: the CSV value under the attestation column, JSON
    /// `confirm_attestation` or `confirm_affirmation`.
    pub attestation_confirmed: Option<String>,
    /// CSV `attester_name`, JSON `attestation.attester_name` (3.0.0).
    pub attester_name: Option<String>,
    /// JSON `modifier_information`; CSV files list modifiers as rows of their own, which
    /// come out as records.
    pub modifiers: Vec<ModifierInfo>,
    /// Fields the caller adds; the parser leaves it empty.
    pub extra: BTreeMap<String, String>,
}

/// One JSON `modifier_information` entry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModifierInfo {
    pub code: Option<String>,
    pub description: Option<String>,
    pub setting: Option<String>,
    /// `modifier_payer_information`.
    pub payers: Vec<ModifierPayer>,
}

/// One `modifier_payer_information` entry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModifierPayer {
    pub name: Option<String>,
    pub plan: Option<String>,
    pub description: Option<String>,
}

/// Why a file cannot be read.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    Io(io::Error),
    /// The CSV header is missing or names no known layout; `line` is physical, 1-based.
    Csv {
        line: u64,
        message: String,
    },
    /// JSON syntax, no JSON value, a top-level value that is not an object, or
    /// `standard_charge_information` not an array; `line` and `column` are 1-based, the
    /// column in chars.
    Json {
        line: u64,
        column: u64,
        message: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => e.fmt(f),
            Error::Csv { line, message } => write!(f, "line {line}: {message}"),
            Error::Json { line, column, message } => {
                write!(f, "line {line}, column {column}: {message}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// Records from a file, read as they are asked for. After an `Err` it yields `None`.
pub struct Parser<R> {
    inner: Inner<R>,
    /// The rest of a row's or item's records.
    pending: VecDeque<Record>,
    done: bool,
}

enum Inner<R> {
    Csv(csv::Parser<R>),
    Json(json::Parser<R>),
}

impl<R: Read> Parser<R> {
    /// Reads nothing until the first record is asked for.
    pub fn new(reader: R, format: Format) -> Self {
        let inner = match format {
            Format::Csv => Inner::Csv(csv::Parser::new(reader)),
            Format::Json => Inner::Json(json::Parser::new(reader)),
        };
        Parser { inner, pending: VecDeque::new(), done: false }
    }

    /// The header as far as it has been read: for CSV complete once the first record is
    /// out, for JSON once the parser has returned `None`.
    pub fn metadata(&self) -> &Metadata {
        match &self.inner {
            Inner::Csv(p) => &p.metadata,
            Inner::Json(p) => &p.metadata,
        }
    }
}

impl<R: Read> Iterator for Parser<R> {
    type Item = Result<Record, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(record) = self.pending.pop_front() {
                return Some(Ok(record));
            }
            if self.done {
                return None;
            }
            let filled = match &mut self.inner {
                Inner::Csv(p) => p.fill(&mut self.pending),
                Inner::Json(p) => p.fill(&mut self.pending),
            };
            match filled {
                Ok(more) => self.done = !more,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

impl<R: Read> FusedIterator for Parser<R> {}

/// Queues a record per payer, all sharing the charge, or one with `payer: None` when there
/// are none.
fn fan_out(
    pending: &mut VecDeque<Record>,
    charge: Charge,
    payers: impl IntoIterator<Item = Payer>,
    position: u64,
) {
    let charge = Arc::new(charge);
    let before = pending.len();
    pending.extend(payers.into_iter().map(|payer| Record {
        charge: Arc::clone(&charge),
        payer: Some(payer),
        position,
    }));
    if pending.len() == before {
        pending.push_back(Record { charge, payer: None, position });
    }
}
