//! Expand an experiment spec into a graph IR.

use std::collections::{BTreeMap, BTreeSet};

use crate::cost::Covering;
use crate::derive;
use crate::error::{Error, Result};
use crate::expr;
use crate::ir::{Ir, Source, Tx};
use crate::metadata::{self, Metadata};
use crate::patterns::{self, Args, Flags, MAX_ELEMENTS};
use crate::spec::{ExperimentSpec, VERSION};

/// Expand patterns, merge explicit sources and txs, validate, and derive relations.
pub fn expand(spec: &ExperimentSpec) -> Result<Ir> {
    if spec.version != VERSION {
        return Err(Error::Spec(format!(
            "unsupported version {}, this build reads version {VERSION}",
            spec.version
        )));
    }
    if spec.wallets.is_empty() {
        return Err(Error::Spec("at least one wallet is required".to_string()));
    }
    // With one wallet it is the default owner; with several, each pattern names one.
    let default_wallet = match spec.wallets.keys().collect::<Vec<_>>().as_slice() {
        [only] => Some((*only).clone()),
        _ => None,
    };
    for wallet in spec.wallets.keys() {
        check_name(wallet)?;
    }
    metadata::validate(&spec.metadata)?;
    let spec_color = spec
        .color
        .as_deref()
        .map(metadata::parse_color)
        .transpose()?;
    let spec_cost = spec
        .cost
        .as_ref()
        .map(|c| c.resolve(&spec.params))
        .transpose()
        .map_err(|e| match e {
            Error::Spec(message) => Error::Spec(format!("spec: {message}")),
            other => other,
        })?;
    let mut ids = BTreeSet::new();
    let mut namespaces = BTreeSet::new();
    let mut total_nodes: u64 = 0;

    let mut sources: BTreeMap<String, Source> = BTreeMap::new();
    let mut txs: BTreeMap<String, Tx> = BTreeMap::new();

    for instance in &spec.patterns {
        check_name(&instance.id)?;
        if !ids.insert(instance.id.as_str()) {
            return Err(Error::Spec(format!(
                "pattern id `{}` is used twice",
                instance.id
            )));
        }
        let count = match &instance.count {
            Some(value) => {
                expr::eval(value, &spec.params).map_err(|e| in_arg(&instance.id, "count", e))?
            }
            None => 1,
        };
        if count > patterns::MAX_SIZE {
            return Err(Error::Spec(format!(
                "pattern `{}`: count is {count}, the maximum is {}",
                instance.id,
                patterns::MAX_SIZE
            )));
        }
        if count == 0 {
            return Err(Error::Spec(format!(
                "pattern `{}`: count must be at least 1",
                instance.id
            )));
        }
        metadata::validate(&instance.metadata).map_err(|e| in_pattern(&instance.id, e))?;
        let tag = Tag {
            metadata: metadata::merge(&spec.metadata, &instance.metadata),
            color: match &instance.color {
                Some(color) => {
                    Some(metadata::parse_color(color).map_err(|e| in_pattern(&instance.id, e))?)
                }
                None => spec_color.clone(),
            },
            cost: match &instance.cost {
                Some(cost) => Some(
                    cost.resolve(&spec.params)
                        .map_err(|e| in_pattern(&instance.id, e))?,
                ),
                None => spec_cost.clone(),
            },
        };
        let mut with = instance.with.clone();
        let flags = Flags {
            conflict: take_string(&mut with, "conflict")
                .map_err(|e| in_pattern(&instance.id, e))?,
        };
        let owner =
            match take_string(&mut with, "wallet").map_err(|e| in_pattern(&instance.id, e))? {
                Some(wallet) => wallet,
                None => default_wallet.clone().ok_or_else(|| {
                    Error::Spec(format!(
                        "pattern `{}`: several wallets are declared, so `wallet` is required",
                        instance.id
                    ))
                })?,
            };
        if !spec.wallets.contains_key(&owner) {
            return Err(Error::Spec(format!(
                "pattern `{}`: unknown wallet `{owner}`",
                instance.id
            )));
        }
        let mut values = BTreeMap::new();
        for (key, value) in &with {
            values.insert(
                key.clone(),
                expr::eval(value, &spec.params).map_err(|e| in_arg(&instance.id, key, e))?,
            );
        }
        let fragment = patterns::generate(
            &instance.pattern,
            Args::new(&instance.pattern, values),
            &flags,
            &owner,
        )
        .map_err(|e| in_pattern(&instance.id, e))?;
        if let Some(label) = instance
            .cost
            .as_ref()
            .and(tag.cost.as_ref())
            .and_then(|c| c.label.as_ref())
        {
            if !fragment.txs.values().any(|t| t.labels.contains(label)) {
                return Err(Error::Spec(format!(
                    "pattern `{}`: cost: no transaction it emits has the label `{label}`",
                    instance.id
                )));
            }
        }
        // Every source, tx, input and output costs memory; count them all.
        let nodes = (fragment.sources.len()
            + fragment
                .txs
                .values()
                .map(|t| 1 + t.inputs.len() + t.reference_inputs.len() + t.outputs.len())
                .sum::<usize>()) as u64;
        total_nodes = total_nodes.saturating_add(nodes.saturating_mul(count));
        if total_nodes > MAX_ELEMENTS {
            return Err(Error::Spec(format!(
                "pattern `{}`: the experiment would exceed {MAX_ELEMENTS} graph elements \
                 (sources, txs, inputs and outputs)",
                instance.id
            )));
        }
        for k in 0..count {
            // Named by whether `count` is written, not by its value, so that
            // scaling `count` through a param keeps names stable.
            let prefix = if instance.count.is_none() {
                format!("{}/", instance.id)
            } else {
                format!("{}_{k}/", instance.id)
            };
            // `x` with count 2 and an instance named `x_0` would share `x_0/`.
            if !namespaces.insert(prefix.clone()) {
                return Err(Error::Spec(format!(
                    "pattern `{}`: namespace `{prefix}` is already used by another instance",
                    instance.id
                )));
            }
            for (name, source) in &fragment.sources {
                insert_unique(&mut sources, format!("{prefix}{name}"), source.clone())?;
            }
            for (name, tx) in &fragment.txs {
                let mut tx = tx.clone();
                tx.inputs = tx.inputs.iter().map(|r| format!("{prefix}{r}")).collect();
                tx.reference_inputs = tx
                    .reference_inputs
                    .iter()
                    .map(|r| format!("{prefix}{r}"))
                    .collect();
                tx.metadata = tag.metadata.clone();
                tx.color = tag.color.clone();
                tx.cost = tag.cost.as_ref().and_then(|c| c.for_labels(&tx.labels));
                insert_unique(&mut txs, format!("{prefix}{name}"), tx)?;
            }
        }
    }

    for (name, source) in &spec.sources {
        check_name(name)?;
        insert_unique(&mut sources, name.clone(), source.clone())?;
    }
    for (name, tx) in &spec.txs {
        check_name(name)?;
        let in_tx = |e: Error| match e {
            Error::Spec(message) => Error::Spec(format!("tx `{name}`: {message}")),
            other => other,
        };
        metadata::validate(&tx.metadata).map_err(in_tx)?;
        let mut tx = tx.clone();
        tx.metadata = metadata::merge(&spec.metadata, &tx.metadata);
        tx.color = match &tx.color {
            Some(color) => Some(metadata::parse_color(color).map_err(in_tx)?),
            None => spec_color.clone(),
        };
        if tx.cost.is_none() {
            tx.cost = spec_cost.as_ref().and_then(|c| c.for_labels(&tx.labels));
        }
        insert_unique(&mut txs, name.clone(), tx)?;
    }

    if let Some(label) = spec_cost.as_ref().and_then(|c| c.label.as_ref()) {
        if !txs.values().any(|t| t.labels.contains(label)) {
            return Err(Error::Spec(format!(
                "spec: cost: no transaction has the label `{label}`"
            )));
        }
    }
    for (name, tx) in &txs {
        let size = metadata::encoded_size(&tx.metadata, tx.color.as_deref());
        if size > metadata::MAX_BYTES {
            return Err(Error::Spec(format!(
                "tx `{name}` asks for {size} bytes of metadata; the most is {}",
                metadata::MAX_BYTES
            )));
        }
    }

    // Canonical form: inputs and reference inputs are sets, so list them sorted.
    for tx in txs.values_mut() {
        tx.inputs.sort();
        tx.reference_inputs.sort();
    }

    let mut ir = Ir {
        version: VERSION,
        wallets: spec.wallets.keys().cloned().collect(),
        sources,
        txs,
        derived: None,
    };
    ir.derived = Some(derive::derive(&ir)?);
    Ok(ir)
}

/// Metadata, colour and script cost a pattern instance puts on the txs it emits.
struct Tag {
    metadata: Metadata,
    color: Option<String>,
    cost: Option<Covering>,
}

/// Name the pattern instance an error came from.
fn in_pattern(id: &str, error: Error) -> Error {
    match error {
        Error::Spec(message) => Error::Spec(format!("pattern `{id}`: {message}")),
        other => other,
    }
}

/// Name the pattern argument an expression error came from.
fn in_arg(id: &str, arg: &str, error: Error) -> Error {
    match error {
        Error::Spec(message) => Error::Spec(format!("pattern `{id}`, `{arg}`: {message}")),
        other => other,
    }
}

fn take_string(
    with: &mut BTreeMap<String, serde_yaml::Value>,
    key: &str,
) -> Result<Option<String>> {
    match with.remove(key) {
        None => Ok(None),
        Some(serde_yaml::Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(Error::Spec(format!(
            "`{key}` must be a string, got {other:?}"
        ))),
    }
}

fn check_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase());
    if !first_ok || !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        return Err(Error::Spec(format!(
            "name `{name}` must match [a-z][a-z0-9_]*"
        )));
    }
    Ok(())
}

fn insert_unique<V>(map: &mut BTreeMap<String, V>, name: String, value: V) -> Result<()> {
    if map.contains_key(&name) {
        return Err(Error::Spec(format!("`{name}` is defined twice")));
    }
    map.insert(name, value);
    Ok(())
}
