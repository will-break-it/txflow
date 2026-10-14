//! The spec check, for the browser.
//!
//! Exposes [`txflow_core`]'s check, schema and catalogue to JavaScript. Every
//! function takes and returns strings, so the page needs no other glue.

use txflow_core::limits::Limits;
use txflow_core::network::Network;
use wasm_bindgen::prelude::wasm_bindgen;

/// Check spec text (YAML or JSON) within `limits` (a limits file's JSON, or
/// empty for none), resolving its schedule against `network` (a network
/// file's JSON, or empty for none). Returns a JSON report: `ok`, `errors`,
/// `warnings`, and on success `summary`, `ir` and `plan`.
#[wasm_bindgen]
pub fn check(text: &str, limits: &str, network: &str) -> String {
    let fail = |e: String| serde_json::json!({ "ok": false, "errors": [e] }).to_string();
    let limits = if limits.trim().is_empty() {
        Limits::default()
    } else {
        match Limits::from_text(limits) {
            Ok(limits) => limits,
            Err(e) => return fail(format!("limits: {e}")),
        }
    };
    let network = if network.trim().is_empty() {
        None
    } else {
        match Network::from_text(network) {
            Ok(network) => Some(network),
            Err(e) => return fail(e.to_string()),
        }
    };
    let report = txflow_core::check::check_within(text, &limits, network.as_ref());
    serde_json::to_string(&report).unwrap_or_else(|e| {
        serde_json::json!({ "ok": false, "errors": [format!("report: {e}")] }).to_string()
    })
}

/// A spec (YAML or JSON) as YAML, fields in their usual order. Text that is
/// not a spec comes back as YAML of whatever it holds.
#[wasm_bindgen]
pub fn to_yaml(text: &str) -> String {
    let yaml = match txflow_core::spec::ExperimentSpec::from_yaml(text) {
        Ok(spec) => serde_yaml::to_string(&spec),
        Err(_) => serde_yaml::from_str::<serde_yaml::Value>(text)
            .and_then(|value| serde_yaml::to_string(&value)),
    };
    yaml.unwrap_or_else(|e| format!("# {e}\n"))
}

/// YAML (or JSON) text as JSON, for checking against the schema:
/// `{"ok": true, "value": ...}` or `{"ok": false, "error": "..."}`.
#[wasm_bindgen]
pub fn to_json(text: &str) -> String {
    let result = match serde_yaml::from_str::<serde_json::Value>(text) {
        Ok(value) => serde_json::json!({ "ok": true, "value": value }),
        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
    };
    result.to_string()
}

/// The JSON Schema for specs of the version this build reads.
#[wasm_bindgen]
pub fn schema() -> String {
    txflow_core::schema::schema().to_string()
}

/// The pattern catalogue as Markdown.
#[wasm_bindgen]
pub fn catalogue() -> String {
    txflow_core::catalogue::markdown()
}
