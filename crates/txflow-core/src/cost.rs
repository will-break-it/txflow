//! Script cost: how much Plutus execution a transaction carries.
//!
//! A spec says what a script run should cost, not which script runs. `cpu`
//! and `mem` are each either a share of the network's per-transaction limit
//! (`25%`, `12.5%`) or absolute execution units (`2500000000`). Shares keep a
//! spec network-free: the same spec means the same load on any network.
//! Materialise turns a cost into a run of txflow's burner script that uses
//! that much and no more.
//!
//! In the spec and in pattern instances a value may also be written with a
//! param, `"{{ dose }}%"` or `"{{ steps }}"`; [`CostSpec::resolve`] evaluates
//! it. Explicit txs and the IR hold resolved values only.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};

use crate::error::{Error, Result};
use crate::expr;

/// A share is held in hundredths of a percent, so `12.5%` is 1250.
pub const BASIS_POINTS: u32 = 10_000;

/// One dimension of a cost: a share of the per-tx limit or absolute units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CostValue {
    /// Hundredths of a percent of the per-tx limit, 1..=10000.
    Share(u32),
    /// Execution units (CPU steps or memory units).
    Units(u64),
}

impl CostValue {
    /// Parse `25%`, `12.5%`, `0.05%` or a whole number of units.
    pub fn parse(text: &str) -> Result<CostValue> {
        let text = text.trim();
        match text.strip_suffix('%') {
            Some(number) => parse_share(number.trim()).map(CostValue::Share),
            None => text
                .parse::<u64>()
                .map(CostValue::Units)
                .map_err(|_| {
                    Error::Spec(format!(
                        "`{text}` is neither a share such as `25%` nor a whole number of units"
                    ))
                })
                .and_then(CostValue::checked),
        }
    }

    fn checked(self) -> Result<CostValue> {
        match self {
            CostValue::Share(0) | CostValue::Units(0) => Err(Error::Spec(
                "a cost must be above zero; leave the field out instead".to_string(),
            )),
            CostValue::Share(bp) if bp > BASIS_POINTS => Err(Error::Spec(format!(
                "a share of {} is above 100% of the per-tx limit",
                share_text(bp)
            ))),
            value => Ok(value),
        }
    }

    /// Units this value asks for against a per-tx `limit`, rounded down.
    pub fn units(self, limit: u64) -> u64 {
        match self {
            CostValue::Units(units) => units,
            CostValue::Share(bp) => {
                (u128::from(limit) * u128::from(bp) / u128::from(BASIS_POINTS)) as u64
            }
        }
    }
}

/// `12.5` → 1250. At most two decimals.
fn parse_share(number: &str) -> Result<u32> {
    let bad = || {
        Error::Spec(format!(
            "`{number}%` is not a share: write a number from 0.01 to 100, at most two decimals"
        ))
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty()
        || fraction.len() > 2
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || (number.contains('.') && fraction.is_empty())
    {
        return Err(bad());
    }
    let whole: u32 = whole.parse().map_err(|_| bad())?;
    let fraction: u32 = format!("{fraction:0<2}").parse().map_err(|_| bad())?;
    let bp = whole
        .checked_mul(100)
        .and_then(|w| w.checked_add(fraction))
        .ok_or_else(bad)?;
    CostValue::Share(bp).checked().map(|_| bp)
}

/// 1250 → `12.5%`: the shortest exact spelling, so the IR hash is stable.
fn share_text(bp: u32) -> String {
    let (whole, fraction) = (bp / 100, bp % 100);
    match fraction {
        0 => format!("{whole}%"),
        f if f % 10 == 0 => format!("{whole}.{}%", f / 10),
        f => format!("{whole}.{f:02}%"),
    }
}

impl fmt::Display for CostValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CostValue::Share(bp) => f.write_str(&share_text(*bp)),
            CostValue::Units(units) => write!(f, "{units}"),
        }
    }
}

impl Serialize for CostValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            CostValue::Share(bp) => serializer.serialize_str(&share_text(*bp)),
            CostValue::Units(units) => serializer.serialize_u64(*units),
        }
    }
}

impl<'de> Deserialize<'de> for CostValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct ValueVisitor;
        impl Visitor<'_> for ValueVisitor {
            type Value = CostValue;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a share such as \"25%\" or a whole number of execution units")
            }

            fn visit_u64<E: de::Error>(self, units: u64) -> std::result::Result<CostValue, E> {
                CostValue::Units(units).checked().map_err(E::custom)
            }

            fn visit_i64<E: de::Error>(self, units: i64) -> std::result::Result<CostValue, E> {
                let units =
                    u64::try_from(units).map_err(|_| E::custom("a cost cannot be negative"))?;
                self.visit_u64(units)
            }

            fn visit_str<E: de::Error>(self, text: &str) -> std::result::Result<CostValue, E> {
                CostValue::parse(text).map_err(E::custom)
            }
        }
        deserializer.deserialize_any(ValueVisitor)
    }
}

/// The script cost of one transaction, as the IR holds it. A dimension left
/// out is as low as the script allows.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Cost {
    /// CPU steps, against the per-tx step limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<CostValue>,
    /// Memory units, against the per-tx memory limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem: Option<CostValue>,
}

impl Cost {
    /// A cost names at least one dimension.
    pub fn validate(&self) -> Result<()> {
        if self.cpu.is_none() && self.mem.is_none() {
            return Err(Error::Spec("`cost` needs `cpu`, `mem` or both".to_string()));
        }
        Ok(())
    }
}

/// `cost:` on the spec or a pattern instance: values may use params, and
/// `select` limits which of the txs it covers.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CostSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<serde_yaml::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem: Option<serde_yaml::Value>,
    /// Only txs with this label carry the cost; default: all of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select: Option<CostSelect>,
}

/// Which txs a spec- or pattern-level cost covers.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CostSelect {
    pub label: String,
}

/// A resolved spec- or pattern-level cost.
#[derive(Clone, Debug, PartialEq)]
pub struct Covering {
    pub cost: Cost,
    pub label: Option<String>,
}

impl Covering {
    /// The cost a tx with `labels` carries under this covering, if any.
    pub fn for_labels(&self, labels: &[String]) -> Option<Cost> {
        match &self.label {
            Some(label) if !labels.iter().any(|l| l == label) => None,
            _ => Some(self.cost),
        }
    }
}

impl CostSpec {
    /// Evaluate params and check the result.
    pub fn resolve(&self, params: &BTreeMap<String, i64>) -> Result<Covering> {
        let value = |field: &str, value: &Option<serde_yaml::Value>| -> Result<Option<CostValue>> {
            value
                .as_ref()
                .map(|v| resolve_value(v, params))
                .transpose()
                .map_err(|e| match e {
                    Error::Spec(message) => Error::Spec(format!("cost `{field}`: {message}")),
                    other => other,
                })
        };
        let cost = Cost {
            cpu: value("cpu", &self.cpu)?,
            mem: value("mem", &self.mem)?,
        };
        cost.validate()?;
        Ok(Covering {
            cost,
            label: self.select.as_ref().map(|s| s.label.clone()),
        })
    }
}

/// `25%`, `"{{ dose }}%"`, `2500000000` or `"{{ steps }}"`.
fn resolve_value(value: &serde_yaml::Value, params: &BTreeMap<String, i64>) -> Result<CostValue> {
    match value {
        serde_yaml::Value::String(text) if text.contains("{{") => {
            let trimmed = text.trim();
            match trimmed.strip_suffix('%') {
                Some(expression) => {
                    let percent =
                        expr::eval(&serde_yaml::Value::String(expression.to_string()), params)?;
                    let bp = u32::try_from(percent)
                        .ok()
                        .and_then(|p| p.checked_mul(100))
                        .ok_or_else(|| Error::Spec(format!("`{text}` is out of range")))?;
                    CostValue::Share(bp).checked()
                }
                None => CostValue::Units(expr::eval(value, params)?).checked(),
            }
        }
        serde_yaml::Value::String(text) => CostValue::parse(text),
        serde_yaml::Value::Number(_) => CostValue::Units(expr::eval(value, params)?).checked(),
        other => Err(Error::Spec(format!(
            "expected a share such as `25%` or a number of units, got {other:?}"
        ))),
    }
}
