//! The plugin `tests/plugin.rs` runs: one function per hook, and some that misbehave.

use extism_pdk::json::{Value, json};
use extism_pdk::{Error, FnResult, HttpRequest, Json, WithReturnCode, config, http, plugin_fn};
use serde::Deserialize;

/// `{"records":[..]}` with the two fields these functions read; serde skips the others.
#[derive(Deserialize)]
struct Records {
    records: Vec<Record>,
}

#[derive(Deserialize)]
struct Record {
    #[serde(rename = "charge.setting")]
    setting: Option<String>,
    #[serde(rename = "payer.name")]
    payer: Option<String>,
}

fn items(input: &Value, key: &str) -> Vec<Value> {
    input[key].as_array().cloned().unwrap_or_default()
}

/// Value transform: strings upper-cased, the rest as it is.
#[plugin_fn]
pub fn upper(Json(input): Json<Value>) -> FnResult<Json<Value>> {
    let values: Vec<Value> = (items(&input, "values").into_iter())
        .map(|v| v.as_str().map_or(v.clone(), |s| Value::from(s.to_uppercase())))
        .collect();
    Ok(Json(json!({ "values": values })))
}

/// Computed column: the config's `region` and the payer, null without a payer.
#[plugin_fn]
pub fn region(Json(input): Json<Records>) -> FnResult<Json<Value>> {
    let region = config::get("region")?.unwrap_or_default();
    let values: Vec<Option<String>> =
        input.records.iter().map(|r| r.payer.as_ref().map(|p| format!("{region}:{p}"))).collect();
    Ok(Json(json!({ "values": values })))
}

/// Filter: the records whose setting is inpatient.
#[plugin_fn]
pub fn keep_inpatient(Json(input): Json<Records>) -> FnResult<Json<Value>> {
    let keep: Vec<bool> =
        input.records.iter().map(|r| r.setting.as_deref() == Some("inpatient")).collect();
    Ok(Json(json!({ "keep": keep })))
}

/// Metadata: the config's `region`, a number, and the hospital's name upper-cased.
#[plugin_fn]
pub fn add_fields(Json(metadata): Json<Value>) -> FnResult<Json<Value>> {
    let name = metadata["hospital_name"].as_str().unwrap_or_default().to_uppercase();
    let region = config::get("region")?;
    Ok(Json(json!({ "fields": { "region": region, "hospital": name, "answer": 42 } })))
}

/// Always an error.
#[plugin_fn]
pub fn fail(_: String) -> FnResult<String> {
    Err(WithReturnCode::new(Error::msg("this function always fails"), 1))
}

/// One value fewer than it was given.
#[plugin_fn]
pub fn short(Json(input): Json<Value>) -> FnResult<Json<Value>> {
    let mut values = items(&input, "values");
    values.pop();
    Ok(Json(json!({ "values": values })))
}

/// Tries the network, which the host does not allow.
#[plugin_fn]
pub fn fetch(Json(input): Json<Value>) -> FnResult<Json<Value>> {
    http::request::<()>(&HttpRequest::new("https://example.org/"), None)?;
    Ok(Json(input))
}
