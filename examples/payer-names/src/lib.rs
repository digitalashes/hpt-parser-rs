//! Payer names to one spelling each, from a table in the plugin's config.
//!
//! A value transform for a `payer.name` column:
//! `transform = ["trim", { plugin = "payers.canonical" }]`. The table is the config's
//! `names`, an `alias = name` a line; an alias matches the whole value in any case, and a
//! value with no alias stays as it is.

use std::collections::HashMap;
use std::sync::OnceLock;

use extism_pdk::{Error, FnResult, Json, config, plugin_fn};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
struct Values {
    values: Vec<Option<String>>,
}

/// Read on the first call: one plugin instance serves the whole run.
static NAMES: OnceLock<HashMap<String, String>> = OnceLock::new();

#[plugin_fn]
pub fn canonical(Json(input): Json<Values>) -> FnResult<Json<Values>> {
    let names = match NAMES.get() {
        Some(names) => names,
        None => {
            let text = config::get("names")?.ok_or_else(|| Error::msg("no `names` in config"))?;
            NAMES.get_or_init(|| table(&text))
        }
    };
    let values = input.values.into_iter().map(|v| v.map(|s| lookup(names, s)));
    Ok(Json(Values { values: values.collect() }))
}

/// `alias = name` lines to a map keyed by the lower-cased alias; other lines are skipped.
fn table(text: &str) -> HashMap<String, String> {
    (text.lines().filter_map(|line| line.split_once('=')))
        .map(|(alias, name)| (alias.trim().to_lowercase(), name.trim().to_string()))
        .collect()
}

/// The table's name for `value`, or `value` itself.
fn lookup(names: &HashMap<String, String>, value: String) -> String {
    names.get(&value.to_lowercase()).cloned().unwrap_or(value)
}

#[cfg(test)]
mod tests {
    #[test]
    fn lookup() {
        let names = super::table("acme = Acme Health\n UHC=UnitedHealthcare \nnot a line\n");
        assert_eq!(super::lookup(&names, "ACME".into()), "Acme Health");
        assert_eq!(super::lookup(&names, "uhc".into()), "UnitedHealthcare");
        assert_eq!(super::lookup(&names, "Region Health".into()), "Region Health");
    }
}
