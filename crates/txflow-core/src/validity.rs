//! Validity windows: from which slot until which slot a transaction is valid.
//!
//! A window is written in slots after an anchor, never as absolute slots, so a
//! spec means the same on every run. `from` becomes the ledger's
//! `invalid_before` (the first slot the tx is valid in) and `until` its
//! `invalid_hereafter` (the first slot it is no longer valid in). The anchor is
//! the run's `start` (the slot the schedule's clock starts in) or its `tip`
//! (the slot of the tip block then). A node admits and revalidates its mempool
//! at the slot after its tip, which trails the wall clock by the time since
//! the last block, so `tip` is the anchor that window edges near zero mean.
//!
//! On the spec and on pattern instances a window may use params, `step`
//! shifts it by that many slots per generation (a tx that spends a tx in
//! the window's reach is one generation later), and `select` limits which
//! txs it covers. [`ValiditySpec::resolve`] evaluates it; explicit txs and
//! the IR hold resolved windows only.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::expr;
use crate::ir::{Ir, Ref};
use crate::schedule::{show_ms, Plan};

/// The largest offset a window may reach, after any `step`: about 31 years
/// of one-second slots, far beyond any run, and safe to add to any slot.
pub const MAX_OFFSET: u64 = 1_000_000_000;

/// What a window's offsets count from.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
#[serde(rename_all = "lowercase")]
pub enum Anchor {
    /// The slot the run's schedule starts in (`at: 0ms`).
    #[default]
    Start,
    /// The slot of the tip block when the run's schedule starts.
    Tip,
}

impl Anchor {
    pub fn as_str(self) -> &'static str {
        match self {
            Anchor::Start => "start",
            Anchor::Tip => "tip",
        }
    }

    fn is_start(&self) -> bool {
        *self == Anchor::Start
    }
}

/// The validity window of one transaction, as the IR holds it.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Validity {
    /// First slot the tx is valid in, after the anchor (`invalid_before`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<u64>,
    /// First slot the tx is no longer valid in, after the anchor
    /// (`invalid_hereafter`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
    /// What `from` and `until` count from. Left out of the canonical form when
    /// it is `start`, the default.
    #[serde(default, skip_serializing_if = "Anchor::is_start")]
    pub anchor: Anchor,
}

impl Validity {
    /// A window names at least one edge, and an empty window is a mistake.
    pub fn validate(&self) -> Result<()> {
        match (self.from, self.until) {
            (None, None) => Err(Error::Spec(
                "`validity` needs `from`, `until` or both".to_string(),
            )),
            (Some(from), Some(until)) if until <= from => Err(Error::Spec(format!(
                "`validity` is empty: `until` ({until}) must be after `from` ({from})"
            ))),
            _ => {
                for edge in [self.from, self.until].into_iter().flatten() {
                    if edge > MAX_OFFSET {
                        return Err(Error::Spec(format!(
                            "`validity` reaches {edge} slots; the most is {MAX_OFFSET}"
                        )));
                    }
                }
                Ok(())
            }
        }
    }

    /// This window moved `by` slots later.
    pub fn shifted(&self, by: u64) -> Result<Validity> {
        let shift = |edge: Option<u64>| edge.map(|e| e.saturating_add(by));
        let moved = Validity {
            from: shift(self.from),
            until: shift(self.until),
            anchor: self.anchor,
        };
        moved.validate()?;
        Ok(moved)
    }

    /// `slots 0..5 after the start`, as the summary and errors say it.
    pub fn describe(&self) -> String {
        let anchor = match self.anchor {
            Anchor::Start => "the start",
            Anchor::Tip => "the tip",
        };
        match (self.from, self.until) {
            (Some(from), Some(until)) => format!("slots {from} to {until} after {anchor}"),
            (Some(from), None) => format!("from slot {from} after {anchor}"),
            (None, Some(until)) => format!("until slot {until} after {anchor}"),
            (None, None) => "always".to_string(),
        }
    }

    /// The absolute window against the run's anchor slots: `(invalid_before,
    /// invalid_hereafter)`. `None` for an anchor the run does not know.
    pub fn absolute(
        &self,
        start: Option<u64>,
        tip: Option<u64>,
    ) -> Option<(Option<u64>, Option<u64>)> {
        let base = match self.anchor {
            Anchor::Start => start?,
            Anchor::Tip => tip?,
        };
        Some((
            self.from.map(|f| base.saturating_add(f)),
            self.until.map(|u| base.saturating_add(u)),
        ))
    }
}

/// `validity:` on the spec or a pattern instance: values may use params,
/// `step` staggers the window by generation, and `select` limits which txs it
/// covers.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ValiditySpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<serde_yaml::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<serde_yaml::Value>,
    /// Slots each generation's window starts later than its parent's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<serde_yaml::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<Anchor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select: Option<ValiditySelect>,
}

/// Which txs a spec- or pattern-level window covers. Both given: txs that
/// match both.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ValiditySelect {
    /// Only txs with this label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Only txs of this generation: 0 for those that spend and read only
    /// sources (the first tx of a chain), 1 for their children, and so on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u64>,
}

/// A resolved spec- or pattern-level window.
#[derive(Clone, Debug, PartialEq)]
pub struct Covering {
    pub window: Validity,
    pub step: u64,
    pub label: Option<String>,
    pub depth: Option<u64>,
}

impl Covering {
    /// Whether a tx with `labels` at generation `depth` takes this window.
    pub fn covers(&self, labels: &[String], depth: u64) -> bool {
        self.label
            .as_ref()
            .is_none_or(|label| labels.iter().any(|l| l == label))
            && self.depth.is_none_or(|d| d == depth)
    }

    /// The window a tx with `labels` at generation `depth` carries, if any.
    pub fn for_tx(&self, labels: &[String], depth: u64) -> Result<Option<Validity>> {
        if !self.covers(labels, depth) {
            return Ok(None);
        }
        let by = self.step.checked_mul(depth).ok_or_else(|| {
            Error::Spec("`validity`: `step` times the generation overflows".to_string())
        })?;
        self.window.shifted(by).map(Some)
    }

    /// `label `x` and generation 0`, for errors about a select that reaches no tx.
    pub fn select_text(&self) -> Option<String> {
        let parts: Vec<String> = [
            self.label.as_ref().map(|l| format!("the label `{l}`")),
            self.depth.map(|d| format!("generation {d}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!parts.is_empty()).then(|| parts.join(" and "))
    }
}

impl ValiditySpec {
    /// Evaluate params and check the result.
    pub fn resolve(&self, params: &BTreeMap<String, i64>) -> Result<Covering> {
        let value = |field: &str, value: &Option<serde_yaml::Value>| -> Result<Option<u64>> {
            value
                .as_ref()
                .map(|v| expr::eval(v, params))
                .transpose()
                .map_err(|e| match e {
                    Error::Spec(message) => Error::Spec(format!("validity `{field}`: {message}")),
                    other => other,
                })
        };
        let window = Validity {
            from: value("from", &self.from)?,
            until: value("until", &self.until)?,
            anchor: self.anchor.unwrap_or_default(),
        };
        window.validate()?;
        let step = value("step", &self.step)?.unwrap_or(0);
        if step > MAX_OFFSET {
            return Err(Error::Spec(format!(
                "validity `step` is {step} slots; the most is {MAX_OFFSET}"
            )));
        }
        let select = self.select.as_ref();
        if select.is_some_and(|s| s.label.is_none() && s.depth.is_none()) {
            return Err(Error::Spec(
                "validity `select` needs `label`, `depth` or both".to_string(),
            ));
        }
        Ok(Covering {
            window,
            step,
            label: select.and_then(|s| s.label.clone()),
            depth: select.and_then(|s| s.depth),
        })
    }
}

/// Windows that rule a tx out whatever happens: a child whose window closes
/// before its parent's opens can never follow it onto the chain. With the
/// network's slot length, also sends that fall outside their tx's window as
/// the clock goes: one sent after `until` can no longer be carried by a block
/// (a node whose tip is older may still take it in), and one sent before a
/// `from` counted from the start is refused until the node's tip passes it.
pub fn warnings(ir: &Ir, plan: Option<&Plan>, slot_ms: Option<u64>) -> Vec<String> {
    let mut warnings = vec![];
    for (name, tx) in &ir.txs {
        let Some(until) = tx.validity.and_then(|v| v.until.map(|u| (u, v.anchor))) else {
            continue;
        };
        for input in tx.inputs.iter().chain(&tx.reference_inputs) {
            let Ok(Ref::Output { tx: parent, .. }) = Ref::parse(input) else {
                continue;
            };
            let Some(from) = ir
                .txs
                .get(&parent)
                .and_then(|p| p.validity)
                .and_then(|v| v.from.map(|f| (f, v.anchor)))
            else {
                continue;
            };
            // The tip is at or before the start, so a window counted from the
            // tip closes no later than the same numbers counted from the start.
            let comparable =
                until.1 == from.1 || (until.1 == Anchor::Tip && from.1 == Anchor::Start);
            if comparable && until.0 <= from.0 {
                warnings.push(format!(
                    "`{name}` can never land: its window closes at slot {} after the {}, \
                     before its parent `{parent}`'s opens at slot {} after the {}",
                    until.0,
                    until.1.as_str(),
                    from.0,
                    from.1.as_str()
                ));
                break;
            }
        }
    }
    let (Some(plan), Some(slot_ms)) = (plan, slot_ms) else {
        return warnings;
    };
    for send in &plan.sends {
        let Some(window) = ir.txs.get(&send.tx).and_then(|t| t.validity) else {
            continue;
        };
        let head = format!(
            "`{}` goes to `{}` at {}",
            send.tx,
            send.target,
            show_ms(send.at_ms)
        );
        if let Some(until) = window.until {
            if send.at_ms >= until.saturating_mul(slot_ms) {
                warnings.push(format!(
                    "{head}, after its window closes (slot {until} after the {}): no block can \
                     carry it, though a node whose tip is older may still take it in",
                    window.anchor.as_str()
                ));
                continue;
            }
        }
        if let (Some(from), Anchor::Start) = (window.from, window.anchor) {
            if send.at_ms < from.saturating_mul(slot_ms) {
                warnings.push(format!(
                    "{head}, before its window opens (slot {from} after the start): a node \
                     refuses it until its tip reaches slot {} after the start",
                    from.saturating_sub(1)
                ));
            }
        }
    }
    warnings
}
