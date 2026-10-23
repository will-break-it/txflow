//! Schedule: where and when each transaction is released.
//!
//! A release selects transactions (by label, by name or by pattern) and sends
//! them to one target, at an offset from the start of the run, optionally
//! spaced out in dependency order. [`plan`] resolves a schedule against the
//! expanded graph and a network into the list of sends, so it can be shown
//! and checked before anything is built or submitted.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::expr;
use crate::ir::Ir;
use crate::network::Network;
use crate::spec::ExperimentSpec;

/// The `schedule:` section of a spec.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    pub releases: Vec<Release>,
    /// Extra targets, by name, for runs against nodes outside the site's
    /// network (a local cluster). A name here shadows a relay of the same name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub targets: BTreeMap<String, Target>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Options>,
}

/// A release offset: `200ms`, `2s`, `1m`, `1h`, `{{ param }}` (the param's
/// integer as milliseconds — the whole offset), or a bare integer (milliseconds,
/// which `check` warns about because `2` for 2 ms is easily misread).
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum Offset {
    /// A bare integer, counted in milliseconds.
    Ms(u64),
    /// `0ms`, `200ms`, `2s`, `1m`, `1h`, a bare number, or `{{ param }}`.
    Text(String),
}

impl Offset {
    pub fn text(&self) -> String {
        match self {
            Offset::Ms(ms) => ms.to_string(),
            Offset::Text(text) => text.clone(),
        }
    }
}

impl<'de> Deserialize<'de> for Offset {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let value = serde_yaml::Value::deserialize(deserializer)?;
        match value {
            serde_yaml::Value::Number(n) => n.as_u64().map(Offset::Ms).ok_or_else(|| {
                D::Error::custom(format!(
                    "`{n}` is not an offset: write `0ms`, `200ms`, `2s` or `1m`, \
                         or leave it to a param (`{{{{ lag }}}}`)"
                ))
            }),
            serde_yaml::Value::String(text) => Ok(Offset::Text(text)),
            other => Err(D::Error::custom(format!(
                "an offset is text with a unit or a non-negative integer, got {other:?}"
            ))),
        }
    }
}

/// Send the selected transactions to one target.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub select: Select,
    /// A relay of the network, or a name in `targets`.
    pub target: String,
    #[serde(default)]
    pub via: Via,
    /// Offset from the start of the run, e.g. `0ms`, `200ms`, `2s`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<Offset>,
    /// Gap between consecutive selected transactions, in dependency order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spacing: Option<Offset>,
    /// Repeat the release this often, re-counting `release-tip` windows from
    /// the tip before each fire. Needs `until`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every: Option<Offset>,
    /// Last offset a repeating release fires at; a wave starts only before it.
    /// Needs `every`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<Offset>,
}

/// Which transactions a release sends. Exactly one field is set.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct Select {
    /// Every transaction carrying this label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// These transactions, by full name (`ds/x1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub names: Option<Vec<String>>,
    /// Every transaction a pattern instance emits, all copies included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

/// How a transaction reaches the target.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    /// Node-to-node TxSubmission, as a peer would.
    N2n,
    /// Node-to-client LocalTxSubmission over the node's socket, which says
    /// whether the node accepted or rejected each transaction. The default.
    #[default]
    N2c,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Via::N2n => "n2n",
            Via::N2c => "n2c",
        }
    }
}

/// A node outside the site's network.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Target {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Options {
    /// Stop sending a tx's descendants once it is rejected (n2c only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_descendants_of_rejected: Option<bool>,
    /// How long to watch for inclusion, e.g. `120s`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observe_for: Option<String>,
}

/// The schedule resolved against the graph: every send, in time order.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Plan {
    pub sends: Vec<Send>,
    /// Transactions no release sends.
    pub unreleased: Vec<String>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Send {
    pub tx: String,
    pub target: String,
    pub via: Via,
    pub at_ms: u64,
}

/// Longest offset a schedule may use: one hour.
pub const MAX_OFFSET_MS: u64 = 3_600_000;

/// Resolve `schedule` against the expanded graph and, when given, the
/// network whose relays it may target.
pub fn plan(
    schedule: &Schedule,
    spec: &ExperimentSpec,
    ir: &Ir,
    network: Option<&Network>,
) -> Result<Plan> {
    let order = ir
        .derived
        .as_ref()
        .map(|d| d.topo_order.clone())
        .unwrap_or_else(|| ir.txs.keys().cloned().collect());
    let mut sends = Vec::new();
    let mut released = BTreeSet::new();
    for (i, release) in schedule.releases.iter().enumerate() {
        let at = |e: String| Error::Spec(format!("schedule release {}: {e}", i + 1));
        let target = check_target(release, schedule, network).map_err(at)?;
        // A template must be the whole offset; substitution yields milliseconds.
        let ms_of = |offset: &Offset, field: &str| -> Result<u64> {
            match offset {
                Offset::Ms(ms) => Ok(*ms),
                Offset::Text(text) => {
                    let trimmed = text.trim();
                    if trimmed.contains("{{") {
                        if !(trimmed.starts_with("{{") && trimmed.ends_with("}}")) {
                            return Err(at(format!(
                                "`{field}` is `{text}`: a `{{{{ param }}}}` must be the whole \
                                 offset — it is already milliseconds, so no unit may sit \
                                 next to it"
                            )));
                        }
                        expr::eval(
                            &serde_yaml::Value::String(trimmed.to_string()),
                            &spec.params,
                        )
                        .map_err(|e| at(e.to_string()))
                    } else {
                        duration(trimmed).map_err(at)
                    }
                }
            }
        };
        let start = ms_of(
            release
                .at
                .as_ref()
                .unwrap_or(&Offset::Text("0ms".to_string())),
            "at",
        )?;
        let spacing = ms_of(
            release
                .spacing
                .as_ref()
                .unwrap_or(&Offset::Text("0ms".to_string())),
            "spacing",
        )?;
        let every = match &release.every {
            None => 0,
            Some(e) => ms_of(e, "every")?,
        };
        let until = match &release.until {
            None => None,
            Some(u) => Some(ms_of(u, "until")?),
        };
        if every == 0 && until.is_some() {
            return Err(at("`until` needs `every`".to_string()));
        }
        let until = if every > 0 {
            let until = until.ok_or_else(|| at("`every` needs `until`".to_string()))?;
            if until <= start {
                return Err(at(format!(
                    "`every` with `until` {} never fires: `until` must be after `at` {}",
                    show_ms(until),
                    show_ms(start)
                )));
            }
            Some(until)
        } else {
            None
        };
        let chosen = select(&release.select, spec, ir, &order).map_err(at)?;
        // One pass per wave: a repeated release re-runs its selection from the
        // top each period, so a `release-tip` window can re-anchor to the tip.
        let mut wave: u64 = 0;
        loop {
            let wave_start = start.saturating_add(every.saturating_mul(wave));
            if let Some(until) = until {
                if wave_start >= until {
                    break;
                }
            }
            for (k, tx) in chosen.iter().enumerate() {
                let at_ms = wave_start.saturating_add(spacing.saturating_mul(k as u64));
                if at_ms > MAX_OFFSET_MS {
                    return Err(at(format!(
                        "`{tx}` would be sent {} after the start; the most is one hour",
                        show_ms(at_ms)
                    )));
                }
                released.insert(tx.clone());
                sends.push(Send {
                    tx: tx.clone(),
                    target: target.clone(),
                    via: release.via,
                    at_ms,
                });
            }
            if every == 0 {
                break;
            }
            wave += 1;
        }
    }
    // Stable: equal times keep release order.
    sends.sort_by_key(|s| s.at_ms);
    let unreleased = order
        .iter()
        .filter(|tx| !released.contains(*tx))
        .cloned()
        .collect();
    Ok(Plan { sends, unreleased })
}

/// Sends that a relay would reject or that only work if a parent reached the
/// relay by propagation first.
///
/// A node accepts a transaction only when every output it spends or reads is
/// already in its ledger or mempool; it keeps no pool of orphans. So each
/// send of a transaction to a relay needs every parent transaction sent to
/// that same relay before it (earlier, or earlier in the same moment's order).
/// A parent sent only elsewhere can still arrive by propagation, which takes
/// time; a parent sent nowhere yet cannot.
pub fn ordering(plan: &Plan, ir: &Ir) -> Vec<String> {
    let mut warnings = Vec::new();
    for (i, send) in plan.sends.iter().enumerate() {
        let Some(tx) = ir.txs.get(&send.tx) else {
            continue;
        };
        let parents: BTreeSet<&str> = tx
            .inputs
            .iter()
            .chain(&tx.reference_inputs)
            .filter_map(|r| r.rsplit_once('.').map(|(p, _)| p))
            .filter(|p| ir.txs.contains_key(*p))
            .collect();
        let mut missing = Vec::new();
        let mut elsewhere = Vec::new();
        for parent in parents {
            let earlier = &plan.sends[..i];
            if earlier
                .iter()
                .any(|e| e.tx == parent && e.target == send.target)
            {
                continue;
            }
            let sent: Vec<&Send> = earlier.iter().filter(|e| e.tx == parent).collect();
            let at = |sends: &[&Send]| {
                sends
                    .iter()
                    .map(|e| format!("`{}` at {}", e.target, show_ms(e.at_ms)))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            if sent.is_empty() {
                let later: Vec<&Send> = plan.sends[i + 1..]
                    .iter()
                    .filter(|e| e.tx == parent)
                    .collect();
                let when = if later.is_empty() {
                    "it is never sent".to_string()
                } else {
                    format!("it goes to {}", at(&later))
                };
                missing.push(format!("`{parent}` ({when})"));
            } else {
                elsewhere.push(format!("`{parent}` (sent to {})", at(&sent)));
            }
        }
        let head = format!(
            "`{}` goes to `{}` at {}",
            send.tx,
            send.target,
            show_ms(send.at_ms)
        );
        if !missing.is_empty() {
            warnings.push(format!(
                "{head} before its parent {} is sent anywhere, so the relay will reject it",
                missing.join(" and ")
            ));
        } else if !elsewhere.is_empty() {
            warnings.push(format!(
                "{head}, but its parent {} is not sent there first; it is accepted only if \
                 `{}` has received the parent by propagation in time",
                elsewhere.join(" and "),
                send.target
            ));
        }
    }
    warnings
}

/// Releases that name a relay by an earlier id: they still work, but say so.
pub fn renamed(schedule: &Schedule, network: Option<&Network>) -> Vec<String> {
    let Some(network) = network else {
        return vec![];
    };
    let mut seen = BTreeSet::new();
    schedule
        .releases
        .iter()
        .filter(|r| !schedule.targets.contains_key(&r.target))
        .filter_map(|r| {
            network
                .relay(&r.target)
                .filter(|relay| relay.id != r.target)
                .map(|relay| (r, relay))
        })
        .filter(|(r, _)| seen.insert(r.target.clone()))
        .map(|(r, relay)| {
            format!(
                "target `{}` is now called `{}`; it still works, but use the new name",
                r.target, relay.id
            )
        })
        .collect()
}

/// The target a release sends to: its own name in `targets`, or the relay's
/// current id (a release may still name it by an earlier one).
fn check_target(
    release: &Release,
    schedule: &Schedule,
    network: Option<&Network>,
) -> std::result::Result<String, String> {
    if schedule.targets.contains_key(&release.target) {
        return Ok(release.target.clone());
    }
    let Some(network) = network else {
        // Without a network only the spec's own targets resolve; the CLI
        // without `--network` checks shape only.
        return Ok(release.target.clone());
    };
    match network.relay(&release.target) {
        Some(relay) if relay.via.contains(&release.via) => Ok(relay.id.clone()),
        Some(relay) => Err(format!(
            "relay `{}` does not accept `{}`; it takes {}",
            relay.id,
            release.via.as_str(),
            relay
                .via
                .iter()
                .map(|v| format!("`{}`", v.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        )),
        None => Err(format!(
            "unknown target `{}`; the relays of {} are {}",
            release.target,
            network.name,
            network
                .relays
                .iter()
                .map(|r| format!("`{}`", r.id))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// The transactions `select` picks, in dependency order.
fn select(
    select: &Select,
    spec: &ExperimentSpec,
    ir: &Ir,
    order: &[String],
) -> std::result::Result<Vec<String>, String> {
    let set = [
        select.label.is_some(),
        select.names.is_some(),
        select.pattern.is_some(),
    ]
    .iter()
    .filter(|x| **x)
    .count();
    if set != 1 {
        return Err("`select` needs exactly one of `label`, `names` or `pattern`".to_string());
    }
    let chosen: Vec<String> = if let Some(label) = &select.label {
        let chosen: Vec<String> = order
            .iter()
            .filter(|tx| ir.txs[*tx].labels.iter().any(|l| l == label))
            .cloned()
            .collect();
        if chosen.is_empty() {
            return Err(format!("no transaction has the label `{label}`"));
        }
        chosen
    } else if let Some(names) = &select.names {
        if names.is_empty() {
            return Err("`names` is empty".to_string());
        }
        for name in names {
            if !ir.txs.contains_key(name) {
                return Err(format!("no transaction is named `{name}`"));
            }
        }
        order
            .iter()
            .filter(|tx| names.contains(tx))
            .cloned()
            .collect()
    } else {
        let id = select.pattern.as_deref().unwrap_or_default();
        let instance = spec
            .patterns
            .iter()
            .find(|p| p.id == id)
            .ok_or_else(|| format!("no pattern has the id `{id}`"))?;
        let counted = instance.count.is_some();
        order
            .iter()
            .filter(|tx| in_pattern(tx, id, counted))
            .cloned()
            .collect()
    };
    Ok(chosen)
}

/// Whether `tx` was emitted by pattern `id`: `id/…`, or `id_<k>/…` for a
/// counted pattern.
fn in_pattern(tx: &str, id: &str, counted: bool) -> bool {
    let Some((namespace, _)) = tx.split_once('/') else {
        return false;
    };
    if counted {
        namespace
            .strip_prefix(id)
            .and_then(|rest| rest.strip_prefix('_'))
            .is_some_and(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_digit()))
    } else {
        namespace == id
    }
}

/// `250ms`, `2s`, `1m`, `1h` or `0` as milliseconds. A bare integer is
/// milliseconds, so a param can scale an offset: `every: "{{ wave }}"`.
pub fn duration(text: &str) -> std::result::Result<u64, String> {
    let text = text.trim();
    let bad = || format!("`{text}` is not a duration; write e.g. `0ms`, `200ms`, `45s` or `1m`");
    let (number, scale) = if let Some(n) = text.strip_suffix("ms") {
        (n, 1)
    } else if let Some(n) = text.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = text.strip_suffix('m') {
        (n, 60_000)
    } else if let Some(n) = text.strip_suffix('h') {
        (n, 3_600_000)
    } else {
        (text, 1)
    };
    let number: u64 = number.trim().parse().map_err(|_| bad())?;
    number
        .checked_mul(scale)
        .filter(|ms| *ms <= MAX_OFFSET_MS)
        .ok_or_else(|| format!("`{text}` is longer than one hour"))
}

pub(crate) fn show_ms(ms: u64) -> String {
    if ms > 0 && ms.is_multiple_of(3_600_000) {
        format!("{}h", ms / 3_600_000)
    } else if ms > 0 && ms.is_multiple_of(60_000) {
        format!("{}m", ms / 60_000)
    } else if ms > 0 && ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}
