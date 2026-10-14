//! Limits one deployment puts on experiments, on top of the format.
//!
//! The spec format has its own hard limits ([`crate::patterns::MAX_SIZE`] and
//! friends) that only stop runaway expansion. A deployment that lets people
//! write their own experiments and funds them with its own test ADA wants far
//! smaller ones; those live in a limits file (`limits.json` at the repository
//! root) and are checked on the expanded graph. They are policy, not grammar:
//! changing them never changes the spec version.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ir::{Ir, Ref};
use crate::metadata::encoded_size;

const LOVELACE_PER_ADA: u64 = 1_000_000;

/// Each limit is optional; a missing one is not checked.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Most transactions an experiment may have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_txs: Option<u64>,
    /// Most transactions on one path of dependent transactions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_chain_length: Option<u64>,
    /// Most outputs one transaction may have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_outputs_per_tx: Option<u64>,
    /// Most ADA the funding outputs may hold together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_funding_ada: Option<u64>,
    /// Most bytes of metadata and colour one transaction may ask for, encoded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_metadata_bytes: Option<u64>,
}

impl Limits {
    /// Parse a limits file (JSON, or YAML).
    pub fn from_text(text: &str) -> crate::error::Result<Limits> {
        crate::spec::from_yaml_strict(text)
    }

    /// The limits as sentences, for a person or a model writing a spec.
    pub fn describe(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(n) = self.max_txs {
            out.push(format!(
                "At most {n} transactions in total, counting every copy."
            ));
        }
        if let Some(n) = self.max_chain_length {
            out.push(format!(
                "At most {n} transactions on one path of dependent transactions."
            ));
        }
        if let Some(n) = self.max_outputs_per_tx {
            out.push(format!("At most {n} outputs per transaction."));
        }
        if let Some(n) = self.max_funding_ada {
            out.push(format!("At most {n} ADA of funding in total."));
        }
        if let Some(n) = self.max_metadata_bytes {
            out.push(format!(
                "At most {n} bytes of metadata per transaction, encoded, colour included."
            ));
        }
        out
    }

    /// Everything `ir` exceeds, as messages naming the number and the limit.
    pub fn check(&self, ir: &Ir) -> Vec<String> {
        let mut errors = Vec::new();
        let txs = ir.txs.len() as u64;
        if let Some(max) = self.max_txs.filter(|max| txs > *max) {
            errors.push(format!(
                "{txs} transactions; this site allows at most {max}"
            ));
        }
        if let Some(max) = self.max_chain_length {
            let longest = longest_chain(ir);
            if longest > max {
                errors.push(format!(
                    "a path of {longest} dependent transactions; this site allows at most {max}"
                ));
            }
        }
        if let Some(max) = self.max_outputs_per_tx {
            if let Some((name, tx)) = ir.txs.iter().find(|(_, tx)| tx.outputs.len() as u64 > max) {
                errors.push(format!(
                    "tx `{name}` has {} outputs; this site allows at most {max}",
                    tx.outputs.len()
                ));
            }
        }
        if let Some(max) = self.max_funding_ada {
            let lovelace = ir
                .sources
                .values()
                .fold(0u64, |sum, s| sum.saturating_add(s.lovelace));
            if lovelace > max.saturating_mul(LOVELACE_PER_ADA) {
                errors.push(format!(
                    "{} ADA of funding; this site allows at most {max} ADA",
                    lovelace.div_ceil(LOVELACE_PER_ADA)
                ));
            }
        }
        if let Some(max) = self.max_metadata_bytes {
            let largest = ir
                .txs
                .iter()
                .map(|(name, tx)| (name, encoded_size(&tx.metadata, tx.color.as_deref())))
                .find(|(_, size)| *size as u64 > max);
            if let Some((name, size)) = largest {
                errors.push(format!(
                    "tx `{name}` has {size} bytes of metadata; this site allows at most {max}"
                ));
            }
        }
        errors
    }
}

/// Number of transactions on the longest path of spends between transactions.
fn longest_chain(ir: &Ir) -> u64 {
    let order: Vec<&String> = match &ir.derived {
        Some(derived) => derived.topo_order.iter().collect(),
        None => return 0,
    };
    let mut length: BTreeMap<&str, u64> = BTreeMap::new();
    let mut longest = 0;
    for name in order {
        let Some(tx) = ir.txs.get(name) else { continue };
        let before = tx
            .inputs
            .iter()
            .chain(&tx.reference_inputs)
            .filter_map(|input| match Ref::parse(input) {
                Ok(Ref::Output { tx, .. }) => length.get(tx.as_str()).copied(),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        let here = before + 1;
        longest = longest.max(here);
        length.insert(name.as_str(), here);
    }
    longest
}
