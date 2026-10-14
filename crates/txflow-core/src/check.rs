//! Check spec text end to end and describe the result.
//!
//! [`check`] parses a spec (YAML, or JSON, which is valid YAML), expands it
//! and validates the graph. On success it returns the IR and a plain summary
//! a person can confirm against what they asked for; on failure, the error
//! message, which names the pattern, tx or field at fault.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::expand::expand;
use crate::ir::Ir;
use crate::limits::Limits;
use crate::network::Network;
use crate::schedule::{self, Plan};
use crate::spec::ExperimentSpec;

/// The outcome of [`check`].
#[derive(Serialize, Debug)]
pub struct Report {
    pub ok: bool,
    /// Why the spec is not runnable. Empty when `ok`.
    pub errors: Vec<String>,
    /// Runnable, but probably not what was meant.
    pub warnings: Vec<String>,
    pub summary: Option<Summary>,
    pub ir: Option<Ir>,
    /// The schedule resolved into sends, when the spec has one.
    pub plan: Option<Plan>,
}

/// What a valid spec expands to.
#[derive(Serialize, Debug)]
pub struct Summary {
    pub experiment: String,
    pub txs: usize,
    pub sources: usize,
    pub funding_lovelace: u64,
    pub conflict_sets: usize,
    /// Txs that are lost if a conflict among their ancestors goes the other way.
    pub orphanable: usize,
    pub reference_contention: usize,
    pub hash: String,
    /// The same facts as sentences, for a person to confirm.
    pub description: Vec<String>,
}

/// Most conflict sets spelled out in the description before it summarises.
const LISTED: usize = 5;

/// Parse, expand and validate `text`.
pub fn check(text: &str) -> Report {
    check_within(text, &Limits::default(), None)
}

/// [`check`], and also hold the expanded graph to a deployment's `limits`
/// and resolve the schedule against its `network`. Without a network a
/// schedule's targets are not checked.
pub fn check_within(text: &str, limits: &Limits, network: Option<&Network>) -> Report {
    let result = ExperimentSpec::from_yaml(text).and_then(|spec| {
        let ir = expand(&spec)?;
        let plan = match &spec.schedule {
            Some(s) => Some(schedule::plan(s, &spec, &ir, network)?),
            None => None,
        };
        Ok((spec, ir, plan))
    });
    match result {
        Ok((spec, ir, plan)) => {
            let exceeded = limits.check(&ir);
            if !exceeded.is_empty() {
                return failed(exceeded);
            }
            let mut warnings = vec![];
            if let Some(schedule) = &spec.schedule {
                warnings.extend(schedule::renamed(schedule, network));
            }
            if let Some(plan) = &plan {
                if !plan.unreleased.is_empty() {
                    warnings.push(format!("no release sends {}", list(&plan.unreleased)));
                }
                warnings.extend(schedule::ordering(plan, &ir));
            }
            let summary = summarise(&spec, &ir, plan.as_ref());
            Report {
                ok: true,
                errors: vec![],
                warnings,
                summary: Some(summary),
                ir: Some(ir),
                plan,
            }
        }
        Err(error) => failed(vec![error.to_string()]),
    }
}

fn failed(errors: Vec<String>) -> Report {
    Report {
        ok: false,
        errors,
        warnings: vec![],
        summary: None,
        ir: None,
        plan: None,
    }
}

/// `a`, `a` and `b`, or `a`, `b` and 3 more.
fn list(names: &[String]) -> String {
    let quoted: Vec<String> = names
        .iter()
        .take(LISTED)
        .map(|n| format!("`{n}`"))
        .collect();
    match names.len() {
        1 => quoted[0].clone(),
        n if n <= LISTED => format!("{} and {}", quoted[..n - 1].join(", "), quoted[n - 1]),
        n => format!("{} and {} more", quoted.join(", "), n - LISTED),
    }
}

fn summarise(spec: &ExperimentSpec, ir: &Ir, plan: Option<&Plan>) -> Summary {
    let funding_lovelace = ir
        .sources
        .values()
        .fold(0u64, |sum, s| sum.saturating_add(s.lovelace));
    let derived = ir.derived.clone().unwrap_or_default();
    let mut description = vec![format!(
        "{} transaction{} funded by {} source{} ({} ADA in total).",
        ir.txs.len(),
        plural(ir.txs.len()),
        ir.sources.len(),
        plural(ir.sources.len()),
        ada(funding_lovelace),
    )];
    for instance in &spec.patterns {
        let copies = match &instance.count {
            Some(_) => " (counted copies)",
            None => "",
        };
        description.push(format!(
            "Pattern `{}` uses `{}`{copies}.",
            instance.id, instance.pattern
        ));
    }
    if derived.conflict_sets.is_empty() {
        description.push("No conflicts: every output is spent at most once.".to_string());
    } else {
        description.push(format!(
            "{} conflict set{} (at most one tx of each can land):",
            derived.conflict_sets.len(),
            plural(derived.conflict_sets.len())
        ));
        for (output, spenders) in derived.conflict_sets.iter().take(LISTED) {
            description.push(format!("  {} all spend {output}", spenders.join(", ")));
        }
        if derived.conflict_sets.len() > LISTED {
            description.push(format!(
                "  … and {} more",
                derived.conflict_sets.len() - LISTED
            ));
        }
    }
    if !derived.orphan_on.is_empty() {
        description.push(format!(
            "{} transaction{} can be orphaned when a conflict upstream goes the other way.",
            derived.orphan_on.len(),
            plural(derived.orphan_on.len())
        ));
    }
    if !derived.reference_contention.is_empty() {
        description.push(format!(
            "{} output{} both spent and read by different transactions (reference contention).",
            derived.reference_contention.len(),
            plural(derived.reference_contention.len())
        ));
    }
    describe_tags(ir, &mut description);
    if let Some(plan) = plan {
        describe_plan(plan, &mut description);
    }
    Summary {
        experiment: spec.experiment.clone(),
        txs: ir.txs.len(),
        sources: ir.sources.len(),
        funding_lovelace,
        conflict_sets: derived.conflict_sets.len(),
        orphanable: derived.orphan_on.len(),
        reference_contention: derived.reference_contention.len(),
        hash: derived.hash,
        description,
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Lovelace as ADA with up to six decimals, trailing zeros dropped.
fn ada(lovelace: u64) -> String {
    let whole = lovelace / 1_000_000;
    let fraction = lovelace % 1_000_000;
    if fraction == 0 {
        whole.to_string()
    } else {
        let digits = format!("{fraction:06}");
        format!("{whole}.{}", digits.trim_end_matches('0'))
    }
}

/// Which colours and metadata labels the transactions carry, with counts.
fn describe_tags(ir: &Ir, description: &mut Vec<String>) {
    let mut colors: BTreeMap<&str, usize> = BTreeMap::new();
    let mut labels: BTreeMap<u64, usize> = BTreeMap::new();
    for tx in ir.txs.values() {
        if let Some(color) = &tx.color {
            *colors.entry(color).or_default() += 1;
        }
        for label in tx.metadata.keys() {
            *labels.entry(label.0).or_default() += 1;
        }
    }
    let counted = |items: Vec<(String, usize)>| {
        items
            .into_iter()
            .map(|(item, n)| format!("{item} on {n} tx{}", plural(n)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !colors.is_empty() {
        description.push(format!(
            "Colour tag: {}.",
            counted(
                colors
                    .into_iter()
                    .map(|(c, n)| (c.to_string(), n))
                    .collect()
            )
        ));
    }
    if !labels.is_empty() {
        description.push(format!(
            "Metadata label{} {}.",
            plural(labels.len()),
            counted(
                labels
                    .into_iter()
                    .map(|(l, n)| (l.to_string(), n))
                    .collect()
            )
        ));
    }
}

/// One sentence per target, in the order targets are first used.
fn describe_plan(plan: &Plan, description: &mut Vec<String>) {
    let mut targets: Vec<(&str, Vec<&schedule::Send>)> = vec![];
    for send in &plan.sends {
        match targets.iter_mut().find(|(t, _)| *t == send.target) {
            Some((_, sends)) => sends.push(send),
            None => targets.push((&send.target, vec![send])),
        }
    }
    description.push(format!(
        "{} send{} to {} target{}:",
        plan.sends.len(),
        plural(plan.sends.len()),
        targets.len(),
        plural(targets.len())
    ));
    for (target, sends) in targets {
        let first = sends.iter().map(|s| s.at_ms).min().unwrap_or(0);
        let last = sends.iter().map(|s| s.at_ms).max().unwrap_or(0);
        let when = if first == last {
            format!("at {first} ms")
        } else {
            format!("from {first} to {last} ms")
        };
        description.push(format!(
            "  {target}: {} tx{} via {}, {when}",
            sends.len(),
            plural(sends.len()),
            sends[0].via.as_str()
        ));
    }
    if !plan.unreleased.is_empty() {
        description.push(format!(
            "{} transaction{} not sent by any release.",
            plan.unreleased.len(),
            plural(plan.unreleased.len())
        ));
    }
}
