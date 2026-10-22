//! Negotiated rates of the coded items: a filter, a computed column and a metadata hook.
//!
//! - `[[filter]] plugin = "rates.coded"`, `fields = ["charge.codes"]`: keeps the records
//!   with a code of a type in the config's `types` (comma-separated, any case).
//! - `[[column]] plugin = "rates.share"`, `fields = ["payer.dollar", "charge.gross_charge"]`:
//!   the negotiated dollar amount as a percentage of the gross charge, null without both.
//! - `[[metadata]] plugin = "rates.counts"`: how many records the filter kept and dropped
//!   over the whole run, which one plugin instance serves.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use extism_pdk::{Error, FnResult, Json, config, plugin_fn};
use serde::{Deserialize, Serialize};

static KEPT: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);

#[derive(Deserialize)]
struct Records {
    records: Vec<Record>,
}

/// The fields the functions read; a record has only those its hook's `fields` name.
#[derive(Deserialize)]
struct Record {
    #[serde(rename = "charge.codes", default)]
    codes: Vec<Code>,
    #[serde(rename = "payer.dollar")]
    dollar: Option<String>,
    #[serde(rename = "charge.gross_charge")]
    gross: Option<String>,
}

#[derive(Deserialize)]
struct Code {
    #[serde(rename = "type")]
    code_type: Option<String>,
}

#[derive(Serialize)]
struct Keep {
    keep: Vec<bool>,
}

#[derive(Serialize)]
struct Values {
    values: Vec<Option<f64>>,
}

#[derive(Serialize)]
struct Fields {
    fields: Counts,
}

#[derive(Serialize)]
struct Counts {
    kept: u64,
    dropped: u64,
}

#[plugin_fn]
pub fn coded(Json(input): Json<Records>) -> FnResult<Json<Keep>> {
    let types = config::get("types")?.ok_or_else(|| Error::msg("no `types` in config"))?;
    let types: Vec<&str> = types.split(',').map(str::trim).collect();
    let keep: Vec<bool> = input.records.iter().map(|r| has_type(&r.codes, &types)).collect();
    let kept = keep.iter().filter(|k| **k).count() as u64;
    KEPT.fetch_add(kept, Relaxed);
    DROPPED.fetch_add(keep.len() as u64 - kept, Relaxed);
    Ok(Json(Keep { keep }))
}

#[plugin_fn]
pub fn share(Json(input): Json<Records>) -> FnResult<Json<Values>> {
    let values = input.records.iter().map(|r| percent(r.dollar.as_deref(), r.gross.as_deref()));
    Ok(Json(Values { values: values.collect() }))
}

#[plugin_fn]
pub fn counts(_metadata: String) -> FnResult<Json<Fields>> {
    let counts = Counts { kept: KEPT.load(Relaxed), dropped: DROPPED.load(Relaxed) };
    Ok(Json(Fields { fields: counts }))
}

fn has_type(codes: &[Code], types: &[&str]) -> bool {
    let wanted = |t: &String| types.iter().any(|w| t.eq_ignore_ascii_case(w));
    codes.iter().any(|c| c.code_type.as_ref().is_some_and(wanted))
}

fn percent(dollar: Option<&str>, gross: Option<&str>) -> Option<f64> {
    let (dollar, gross) = (amount(dollar?)?, amount(gross?)?);
    (gross > 0.0).then(|| dollar / gross * 100.0)
}

/// `$1,200.00` to 1200.
fn amount(text: &str) -> Option<f64> {
    text.replace(['$', ','], "").trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{Code, has_type, percent};

    #[test]
    fn coded() {
        let codes = [Code { code_type: None }, Code { code_type: Some("cpt".into()) }];
        assert!(has_type(&codes, &["HCPCS", "CPT"]));
        assert!(!has_type(&codes, &["HCPCS"]));
        assert!(!has_type(&[], &["CPT"]));
    }

    #[test]
    fn share() {
        assert_eq!(percent(Some("$1,200.00"), Some("1500")), Some(80.0));
        assert_eq!(percent(None, Some("1800")), None);
        assert_eq!(percent(Some("1200"), Some("0")), None);
        assert_eq!(percent(Some("see notes"), Some("1800")), None);
    }
}
