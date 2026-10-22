//! JSON files: one object whose `standard_charge_information` array holds the charge
//! items, the rest of its members are the header. Members may come in any order, so the
//! header can follow the array. Only the header members and one item at a time are built.

mod parse;

use crate::{
    Aliases, Charge, Code, Error, Layout, Metadata, ModifierInfo, ModifierPayer, Payer, Record,
};
use parse::{Key, Value};
use std::collections::VecDeque;
use std::io::Read;

/// The header members, then each charge item.
const PATHS: &[&[&str]] = &[
    &["hospital_name"],
    &["last_updated_on"],
    &["license_information"],
    &["version"],
    &["hospital_address"],
    &["hospital_location"],
    &["location_name"],
    &["affirmation"],
    &["attestation"],
    &["modifier_information"],
    &["type_2_npi"],
    &["standard_charge_information", "*"],
];

/// Every member name read, which an alias may stand for.
#[cfg(feature = "config")]
pub(crate) const NAMES: &[&str] = &[
    "hospital_name",
    "last_updated_on",
    "license_information",
    "license_number",
    "state",
    "version",
    "hospital_address",
    "hospital_location",
    "location_name",
    "affirmation",
    "confirm_affirmation",
    "attestation",
    "confirm_attestation",
    "attester_name",
    "modifier_information",
    "modifier_payer_information",
    "type_2_npi",
    "standard_charge_information",
    "description",
    "code_information",
    "code",
    "type",
    "drug_information",
    "unit",
    "standard_charges",
    "setting",
    "billing_class",
    "modifier_code",
    "gross_charge",
    "discounted_cash",
    "minimum",
    "maximum",
    "additional_generic_notes",
    "payers_information",
    "payer_name",
    "plan_name",
    "standard_charge_dollar",
    "standard_charge_percentage",
    "standard_charge_algorithm",
    "methodology",
    "estimated_amount",
    "median_amount",
    "10th_percentile",
    "90th_percentile",
    "count",
    "additional_payer_notes",
];

pub struct Parser<R> {
    values: parse::Parser<R>,
    pub metadata: Metadata,
    /// Counts the changes to `metadata`.
    pub changes: u64,
}

impl<R: Read> Parser<R> {
    pub fn new(input: R) -> Self {
        let metadata = Metadata { layout: Some(Layout::Json), ..Metadata::default() };
        Parser { values: parse::Parser::new(input, PATHS), metadata, changes: 0 }
    }

    pub fn set_aliases(&mut self, aliases: Aliases) {
        self.values.set_aliases(aliases);
    }

    /// Reads up to the next charge item and queues its records; false at the end.
    // ponytail: a top-level member read through an alias does not give way to one under its
    // own name (the later one wins, as with a repeated name); the members stream, so that
    // takes remembering which names came
    pub fn fill(&mut self, pending: &mut VecDeque<Record>) -> Result<bool, Error> {
        while let Some((key, value)) = self.values.next()? {
            let aliases = self.metadata.aliases_used.len();
            self.metadata.aliases_used.append(self.values.aliases_used());
            match &key {
                Key::Index(i) => item(&value, *i as u64, pending),
                Key::Name(name) => header_field(&mut self.metadata, name, &value),
            }
            if matches!(key, Key::Name(_)) || self.metadata.aliases_used.len() > aliases {
                self.changes += 1;
            }
            self.values.recycle(value);
            if matches!(key, Key::Index(_)) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn header_field(m: &mut Metadata, name: &str, v: &Value) {
    let get = |name| text(v.get(name));
    match name {
        "hospital_name" => m.hospital_name = text(Some(v)),
        "last_updated_on" => m.last_updated_on = text(Some(v)),
        "version" => m.version = text(Some(v)),
        "hospital_address" => m.hospital_address = list(Some(v)),
        "hospital_location" | "location_name" => m.location_name = list(Some(v)),
        "type_2_npi" => m.type_2_npi = list(Some(v)),
        "license_information" => {
            m.license_number = get("license_number");
            m.license_state = get("state");
        }
        "affirmation" => {
            m.attestation = get("affirmation");
            m.attestation_confirmed = get("confirm_affirmation");
        }
        "attestation" => {
            m.attestation = get("attestation");
            m.attestation_confirmed = get("confirm_attestation");
            m.attester_name = get("attester_name");
        }
        "modifier_information" => {
            m.modifiers = (array(Some(v)))
                .map(|x| ModifierInfo {
                    code: text(x.get("code")),
                    description: text(x.get("description")),
                    setting: text(x.get("setting")),
                    payers: (array(x.get("modifier_payer_information")))
                        .map(|p| ModifierPayer {
                            name: text(p.get("payer_name")),
                            plan: text(p.get("plan_name")),
                            description: text(p.get("description")),
                        })
                        .collect(),
                })
                .collect();
        }
        _ => {}
    }
}

/// A charge item's records: one per `payers_information` entry of each `standard_charges`
/// entry, those of one entry sharing its charge.
fn item(v: &Value, position: u64, pending: &mut VecDeque<Record>) {
    let drug = v.get("drug_information");
    let item = Charge {
        description: text(v.get("description")),
        codes: (array(v.get("code_information")))
            .filter_map(|c| {
                Some(Code { code: text(c.get("code"))?, code_type: text(c.get("type")) })
            })
            .collect(),
        drug_unit: text(drug.and_then(|d| d.get("unit"))),
        drug_unit_type: text(drug.and_then(|d| d.get("type"))),
        ..Charge::default()
    };
    let mut charges = array(v.get("standard_charges")).peekable();
    if charges.peek().is_none() {
        return crate::fan_out(pending, item, [], position);
    }
    for c in charges {
        let get = |name| text(c.get(name));
        let charge = Charge {
            setting: get("setting"),
            billing_class: get("billing_class"),
            modifiers: list(c.get("modifier_code")),
            gross_charge: get("gross_charge"),
            discounted_cash: get("discounted_cash"),
            minimum: get("minimum"),
            maximum: get("maximum"),
            notes: get("additional_generic_notes"),
            ..item.clone()
        };
        crate::fan_out(pending, charge, array(c.get("payers_information")).map(payer), position);
    }
}

// Without the hint an unrelated change in the config module left it out of line: nyp 3%
// slower.
#[inline]
fn payer(p: &Value) -> Payer {
    let get = |name| text(p.get(name));
    Payer {
        name: get("payer_name"),
        plan: get("plan_name"),
        dollar: get("standard_charge_dollar"),
        percentage: get("standard_charge_percentage"),
        algorithm: get("standard_charge_algorithm"),
        methodology: get("methodology"),
        estimated_amount: get("estimated_amount"),
        median_amount: get("median_amount"),
        percentile_10th: get("10th_percentile"),
        percentile_90th: get("90th_percentile"),
        count: get("count"),
        notes: get("additional_payer_notes"),
    }
}

/// A string as written, a number's text, `true` or `false`; `None` for an empty string and
/// anything else.
fn text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) | Value::Number(s) if !s.is_empty() => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// An array's texts; a lone value is a list of one.
fn list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(items)) => items.iter().filter_map(|v| text(Some(v))).collect(),
        v => text(v).into_iter().collect(),
    }
}

fn array(v: Option<&Value>) -> std::slice::Iter<'_, Value> {
    match v {
        Some(Value::Array(items)) => items.iter(),
        _ => [].iter(),
    }
}

#[cfg(all(test, feature = "config"))]
mod tests {
    /// Every name the code reads is in `NAMES`, so it can be an alias target.
    #[test]
    fn names() {
        let code = include_str!("mod.rs").split("#[cfg(all(test").next().unwrap();
        let names = code.split("const NAMES").nth(1).unwrap().split("];").nth(1).unwrap();
        for literal in names.split('"').skip(1).step_by(2) {
            if literal.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                assert!(super::NAMES.contains(&literal), "{literal}");
            }
        }
    }
}
