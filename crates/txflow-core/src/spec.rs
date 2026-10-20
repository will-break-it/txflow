//! Experiment spec: what a person writes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::cost::CostSpec;
use crate::error::Result;
use crate::ir::{Source, Tx};
use crate::metadata::Metadata;
use crate::schedule::Schedule;

/// Version of the spec format this crate reads. Adding a pattern or an
/// argument keeps it; a change that makes an existing spec invalid or mean
/// something else bumps it, and the schema for the old version stays
/// committed under `schema/v<old>/` (see `schema/README.md`).
pub const VERSION: u32 = 0;

/// An experiment file. `expect` is carried but not yet interpreted.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct ExperimentSpec {
    pub version: u32,
    pub experiment: String,
    pub seed: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, i64>,
    pub wallets: BTreeMap<String, WalletSpec>,
    /// Metadata on every tx. A pattern instance's or a tx's own metadata
    /// replaces it label by label.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: Metadata,
    /// Colour tag on every tx, unless a pattern instance or tx sets its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Script cost on every tx (or every tx with `select.label`), unless a
    /// pattern instance or tx sets its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<PatternInstance>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<String, Source>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub txs: BTreeMap<String, Tx>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<Schedule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<serde_yaml::Value>,
}

/// A symbolic wallet. In this MVP all wallets bind to the run key.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct WalletSpec {}

/// One use of a pattern.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct PatternInstance {
    pub id: String,
    #[serde(rename = "use")]
    pub pattern: String,
    /// How many independent instances to emit. Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<serde_yaml::Value>,
    #[serde(default)]
    pub with: BTreeMap<String, serde_yaml::Value>,
    /// Metadata on every tx this instance emits, over the spec's.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: Metadata,
    /// Colour tag on every tx this instance emits, over the spec's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Script cost on the txs this instance emits, replacing the spec's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostSpec>,
}

impl ExperimentSpec {
    pub fn from_yaml(text: &str) -> Result<ExperimentSpec> {
        from_yaml_strict(text)
    }
}

/// Parse YAML, rejecting a key repeated in any map. Deserializing straight
/// into a `BTreeMap` keeps the last value and drops the earlier ones
/// silently, so a copy-pasted tx or source would vanish from the graph.
pub fn from_yaml_strict<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    // A generic value rejects duplicate keys, with their location.
    serde_yaml::from_str::<serde_yaml::Value>(text)?;
    Ok(serde_yaml::from_str(text)?)
}
