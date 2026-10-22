//! Validate an IR and derive its relations: topological order, conflict
//! sets, reference contention, orphans, and the content hash.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use blake2::digest::{Update, VariableOutput};
use blake2::Blake2bVar;

use crate::cost::CostValue;
use crate::error::{Error, Result};
use crate::ir::{Amount, AmountKeyword, Contention, Derived, Ir, Ref};
use crate::metadata::Metadata;
use crate::validity::Anchor;

/// Largest total of (tx, upstream conflict) relations `derive` will compute.
const MAX_RELATIONS: u64 = 2_000_000;

/// Validate `ir` and compute its [`Derived`] block. Any existing `derived` is ignored.
pub fn derive(ir: &Ir) -> Result<Derived> {
    validate(ir)?;
    let topo_order = topo_order(ir)?;

    // Who spends and who reads each output.
    let mut spent_by: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut read_by: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, tx) in &ir.txs {
        for input in &tx.inputs {
            spent_by
                .entry(input.clone())
                .or_default()
                .push(name.clone());
        }
        for input in &tx.reference_inputs {
            read_by.entry(input.clone()).or_default().push(name.clone());
        }
    }

    let conflict_sets: BTreeMap<String, Vec<String>> = spent_by
        .iter()
        .filter(|(_, spenders)| spenders.len() >= 2)
        .map(|(output, spenders)| (output.clone(), spenders.clone()))
        .collect();

    let reference_contention: BTreeMap<String, Contention> = read_by
        .iter()
        .filter_map(|(output, readers)| {
            spent_by.get(output).map(|spenders| {
                (
                    output.clone(),
                    Contention {
                        spent_by: spenders.clone(),
                        read_by: readers.clone(),
                    },
                )
            })
        })
        .collect();

    // Walk the graph once, in topological order, carrying for each tx the
    // tracked outputs (contested, or read by someone) that it or an ancestor
    // spends, and by whom. That one map yields `orphan_on` and rejects txs
    // that can never be valid: ones that depend on both sides of a conflict,
    // or spend or read what an ancestor already spent.
    let tracked: BTreeSet<&str> = conflict_sets
        .keys()
        .chain(reference_contention.keys())
        .map(String::as_str)
        .collect();
    // Distinct parents of each tx, and how many children still need each
    // tx's map, so a map is dropped as soon as its last child has used it.
    let mut parents_of: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    let mut waiting: BTreeMap<String, usize> = BTreeMap::new();
    for (name, tx) in &ir.txs {
        let parents = parents_of.entry(name).or_default();
        for input in tx.inputs.iter().chain(tx.reference_inputs.iter()) {
            if let Ref::Output { tx: parent, .. } = Ref::parse(input)? {
                if parents.insert(parent.clone()) {
                    *waiting.entry(parent).or_default() += 1;
                }
            }
        }
    }
    let mut upstream: BTreeMap<&str, BTreeMap<&str, &str>> = BTreeMap::new();
    let mut orphan: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // Ancestor relations carried so far; bounded because `orphan_on` (and
    // this walk) grow with chain length times conflicts upstream.
    let mut relations: u64 = 0;
    for name in &topo_order {
        let tx = ir
            .txs
            .get(name)
            .ok_or_else(|| Error::Graph(format!("missing tx `{name}`")))?;
        let parents = parents_of
            .get(name.as_str())
            .ok_or_else(|| Error::Graph(format!("missing tx `{name}`")))?;
        // What strict ancestors spend. A parent with no other child hands
        // its map over instead of having it copied (the common chain case).
        let mut inherited: BTreeMap<&str, &str> = BTreeMap::new();
        if let (1, Some(only)) = (parents.len(), parents.iter().next()) {
            if waiting.get(only) == Some(&1) {
                inherited = upstream.remove(only.as_str()).unwrap_or_default();
            }
        }
        for parent in parents {
            for (output, spender) in upstream.get(parent.as_str()).into_iter().flatten() {
                match inherited.insert(output, spender) {
                    Some(other) if other != *spender => {
                        return Err(Error::Graph(format!(
                            "tx `{name}` depends on both `{other}` and `{spender}`, \
                             which spend the same output `{output}`"
                        )))
                    }
                    _ => {}
                }
            }
        }
        for input in &tx.reference_inputs {
            if let Some(spender) = inherited.get(input.as_str()) {
                return Err(Error::Graph(format!(
                    "tx `{name}` reads `{input}`, which its ancestor `{spender}` already spends"
                )));
            }
        }
        let sets: Vec<String> = inherited
            .keys()
            .filter(|output| conflict_sets.contains_key(**output))
            .map(|output| output.to_string())
            .collect();
        if !sets.is_empty() {
            orphan.insert(name.clone(), sets);
        }
        relations = relations.saturating_add(inherited.len() as u64);
        if relations > MAX_RELATIONS {
            return Err(Error::Graph(format!(
                "more than {MAX_RELATIONS} ancestor-conflict relations; shorten the \
                 conflicting chains or reduce their count"
            )));
        }
        for parent in parents {
            if let Some(left) = waiting.get_mut(parent) {
                *left = left.saturating_sub(1);
                if *left == 0 {
                    upstream.remove(parent.as_str());
                }
            }
        }
        let mut own = inherited;
        for input in &tx.inputs {
            if let Some(output) = tracked.get(input.as_str()) {
                if let Some(spender) = own.insert(output, name.as_str()) {
                    return Err(Error::Graph(format!(
                        "tx `{name}` spends `{input}`, which its ancestor `{spender}` already spends"
                    )));
                }
            }
        }
        // Only txs with children pass their map on.
        if waiting.contains_key(name.as_str()) {
            upstream.insert(name.as_str(), own);
        }
    }

    Ok(Derived {
        topo_order,
        conflict_sets,
        reference_contention,
        orphan_on: orphan,
        hash: ir_hash(ir)?,
    })
}

/// Transaction → the conflict sets it belongs to.
pub fn membership(
    conflict_sets: &BTreeMap<String, Vec<String>>,
) -> BTreeMap<&str, BTreeSet<String>> {
    let mut member_of: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for (set, members) in conflict_sets {
        for member in members {
            member_of.entry(member).or_default().insert(set.clone());
        }
    }
    member_of
}

/// `blake2b256:<hex>` over the canonical JSON of the IR without `derived`:
/// object keys sorted, `inputs` and `reference_inputs` sorted (they are sets),
/// empty `labels`, `reference_inputs` and `metadata`, a missing `color`,
/// `cost` and `validity`, and a window's `anchor` when it is `start`, omitted,
/// so a graph without them hashes as it did before they existed. Streamed
/// into the hasher, so no copy of the IR is made, and independent of struct field order and
/// of serde_json's `preserve_order` feature.
pub fn ir_hash(ir: &Ir) -> Result<String> {
    let mut hasher = Blake2bVar::new(32)
        .map_err(|_| Error::Graph("blake2b-256 is not available".to_string()))?;
    canonical(&mut HashWriter(&mut hasher), ir)?;
    let mut digest = [0u8; 32];
    hasher
        .finalize_variable(&mut digest)
        .map_err(|_| Error::Graph("blake2b-256 output size".to_string()))?;
    Ok(format!("blake2b256:{}", hex::encode(digest)))
}

/// Feeds written bytes to a hasher.
struct HashWriter<'a>(&'a mut Blake2bVar);

impl Write for HashWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A JSON string, escaped.
fn text(out: &mut impl Write, s: &str) -> Result<()> {
    Ok(serde_json::to_writer(out, s)?)
}

/// A JSON array of strings, in the given order.
fn list(out: &mut impl Write, items: &[&String]) -> Result<()> {
    out.write_all(b"[")?;
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.write_all(b",")?;
        }
        text(out, item)?;
    }
    Ok(out.write_all(b"]")?)
}

/// Metadata as JSON: labels as strings, every object's keys sorted.
fn metadata_json(out: &mut impl Write, metadata: &Metadata) -> Result<()> {
    let mut labels: Vec<(String, &serde_json::Value)> =
        metadata.iter().map(|(l, v)| (l.to_string(), v)).collect();
    labels.sort_by(|a, b| a.0.cmp(&b.0));
    out.write_all(b"{")?;
    for (i, (label, value)) in labels.into_iter().enumerate() {
        if i > 0 {
            out.write_all(b",")?;
        }
        text(out, &label)?;
        out.write_all(b":")?;
        json_value(out, value)?;
    }
    Ok(out.write_all(b"}")?)
}

fn json_value(out: &mut impl Write, value: &serde_json::Value) -> Result<()> {
    match value {
        serde_json::Value::Array(items) => {
            out.write_all(b"[")?;
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.write_all(b",")?;
                }
                json_value(out, item)?;
            }
            Ok(out.write_all(b"]")?)
        }
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.write_all(b"{")?;
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.write_all(b",")?;
                }
                text(out, key)?;
                out.write_all(b":")?;
                json_value(out, &map[key])?;
            }
            Ok(out.write_all(b"}")?)
        }
        scalar => Ok(serde_json::to_writer(out, scalar)?),
    }
}

fn sorted_refs(items: &[String]) -> Vec<&String> {
    let mut refs: Vec<&String> = items.iter().collect();
    refs.sort();
    refs
}

fn canonical(out: &mut impl Write, ir: &Ir) -> Result<()> {
    out.write_all(b"{\"sources\":{")?;
    for (i, (name, source)) in ir.sources.iter().enumerate() {
        if i > 0 {
            out.write_all(b",")?;
        }
        text(out, name)?;
        write!(out, ":{{\"lovelace\":{},\"owner\":", source.lovelace)?;
        text(out, &source.owner)?;
        out.write_all(b"}")?;
    }
    out.write_all(b"},\"txs\":{")?;
    for (i, (name, tx)) in ir.txs.iter().enumerate() {
        if i > 0 {
            out.write_all(b",")?;
        }
        text(out, name)?;
        out.write_all(b":{")?;
        if let Some(color) = &tx.color {
            out.write_all(b"\"color\":")?;
            text(out, color)?;
            out.write_all(b",")?;
        }
        if let Some(cost) = &tx.cost {
            out.write_all(b"\"cost\":{")?;
            let fields = [("cpu", cost.cpu), ("mem", cost.mem)];
            let mut first = true;
            for (field, value) in fields {
                let Some(value) = value else { continue };
                if !first {
                    out.write_all(b",")?;
                }
                first = false;
                write!(out, "\"{field}\":")?;
                match value {
                    CostValue::Share(_) => text(out, &value.to_string())?,
                    CostValue::Units(units) => write!(out, "{units}")?,
                }
            }
            out.write_all(b"},")?;
        }
        out.write_all(b"\"inputs\":")?;
        list(out, &sorted_refs(&tx.inputs))?;
        if !tx.labels.is_empty() {
            out.write_all(b",\"labels\":")?;
            list(out, &tx.labels.iter().collect::<Vec<_>>())?;
        }
        if !tx.metadata.is_empty() {
            out.write_all(b",\"metadata\":")?;
            metadata_json(out, &tx.metadata)?;
        }
        out.write_all(b",\"outputs\":[")?;
        for (k, output) in tx.outputs.iter().enumerate() {
            if k > 0 {
                out.write_all(b",")?;
            }
            out.write_all(b"{\"lovelace\":")?;
            match output.lovelace {
                Amount::Fixed(l) => write!(out, "{l}")?,
                Amount::Keyword(AmountKeyword::Min) => out.write_all(b"\"min\"")?,
                Amount::Keyword(AmountKeyword::Change) => out.write_all(b"\"change\"")?,
            }
            out.write_all(b",\"to\":")?;
            text(out, &output.to)?;
            out.write_all(b"}")?;
        }
        out.write_all(b"]")?;
        if !tx.reference_inputs.is_empty() {
            out.write_all(b",\"reference_inputs\":")?;
            list(out, &sorted_refs(&tx.reference_inputs))?;
        }
        if let Some(validity) = &tx.validity {
            out.write_all(b",\"validity\":{")?;
            let mut fields = vec![];
            if validity.anchor != Anchor::Start {
                fields.push(format!("\"anchor\":\"{}\"", validity.anchor.as_str()));
            }
            if let Some(from) = validity.from {
                fields.push(format!("\"from\":{from}"));
            }
            if let Some(until) = validity.until {
                fields.push(format!("\"until\":{until}"));
            }
            out.write_all(fields.join(",").as_bytes())?;
            out.write_all(b"}")?;
        }
        out.write_all(b"}")?;
    }
    write!(out, "}},\"version\":{},\"wallets\":", ir.version)?;
    list(out, &sorted_refs(&ir.wallets))?;
    Ok(out.write_all(b"}")?)
}

fn validate(ir: &Ir) -> Result<()> {
    let mut wallets = BTreeSet::new();
    for wallet in &ir.wallets {
        if !wallets.insert(wallet) {
            return Err(Error::Graph(format!("wallet `{wallet}` is listed twice")));
        }
    }
    for (name, source) in &ir.sources {
        if !ir.wallets.contains(&source.owner) {
            return Err(Error::Graph(format!(
                "source `{name}`: unknown wallet `{}`",
                source.owner
            )));
        }
    }
    for (name, tx) in &ir.txs {
        if tx.inputs.is_empty() {
            return Err(Error::Graph(format!("tx `{name}` has no inputs")));
        }
        if tx.outputs.is_empty() {
            return Err(Error::Graph(format!("tx `{name}` has no outputs")));
        }
        let changes = tx
            .outputs
            .iter()
            .filter(|o| o.lovelace == Amount::Keyword(AmountKeyword::Change))
            .count();
        if changes != 1 {
            return Err(Error::Graph(format!(
                "tx `{name}` must have exactly one `change` output, has {changes}"
            )));
        }
        if let Some(cost) = &tx.cost {
            cost.validate()
                .map_err(|e| Error::Graph(format!("tx `{name}`: {e}")))?;
        }
        if let Some(validity) = &tx.validity {
            validity
                .validate()
                .map_err(|e| Error::Graph(format!("tx `{name}`: {e}")))?;
        }
        for output in &tx.outputs {
            if !ir.wallets.contains(&output.to) {
                return Err(Error::Graph(format!(
                    "tx `{name}`: unknown wallet `{}`",
                    output.to
                )));
            }
        }
        let mut seen = BTreeSet::new();
        for input in tx.inputs.iter().chain(tx.reference_inputs.iter()) {
            if !seen.insert(input) {
                return Err(Error::Graph(format!("tx `{name}` lists `{input}` twice")));
            }
            resolve(ir, name, input)?;
        }
    }
    Ok(())
}

fn resolve(ir: &Ir, tx_name: &str, input: &str) -> Result<()> {
    match Ref::parse(input)? {
        Ref::Source(source) => {
            if !ir.sources.contains_key(&source) {
                return Err(Error::Graph(format!(
                    "tx `{tx_name}`: `{input}` is neither a source nor `tx.N`"
                )));
            }
        }
        Ref::Output { tx, index } => {
            let parent = ir.txs.get(&tx).ok_or_else(|| {
                Error::Graph(format!("tx `{tx_name}`: `{input}` names unknown tx `{tx}`"))
            })?;
            if index >= parent.outputs.len() {
                return Err(Error::Graph(format!(
                    "tx `{tx_name}`: `{tx}` has no output {index}"
                )));
            }
        }
    }
    Ok(())
}

/// Each tx's generation: 0 when it spends and reads only sources, otherwise
/// one more than the latest tx it spends or reads from. `order` is the IR's
/// topological order.
pub fn generations(ir: &Ir, order: &[String]) -> Result<BTreeMap<String, u64>> {
    let mut generation: BTreeMap<String, u64> = BTreeMap::new();
    for name in order {
        let tx = ir
            .txs
            .get(name)
            .ok_or_else(|| Error::Graph(format!("missing tx `{name}`")))?;
        let mut own = 0;
        for input in tx.inputs.iter().chain(tx.reference_inputs.iter()) {
            if let Ref::Output { tx: parent, .. } = Ref::parse(input)? {
                own = own.max(generation.get(&parent).map_or(0, |g| g + 1));
            }
        }
        generation.insert(name.clone(), own);
    }
    Ok(generation)
}

/// Kahn's algorithm, one layer at a time: every tx whose parents are all
/// placed is emitted, in name order, before any of its children. So the
/// order is deterministic and independent chains interleave step by step
/// (`a1, b1, a2, b2, ...`).
fn topo_order(ir: &Ir) -> Result<Vec<String>> {
    let mut parents: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    let mut children: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    for (name, tx) in &ir.txs {
        let entry = parents.entry(name).or_default();
        for input in tx.inputs.iter().chain(tx.reference_inputs.iter()) {
            if let Ref::Output { tx: parent, .. } = Ref::parse(input)? {
                if parent == *name {
                    return Err(Error::Graph(format!(
                        "tx `{name}` spends or reads its own output `{input}`"
                    )));
                }
                entry.insert(parent.clone());
                children.entry(parent).or_default().insert(name);
            }
        }
    }
    let mut layer: BTreeSet<&str> = parents
        .iter()
        .filter(|(_, p)| p.is_empty())
        .map(|(name, _)| *name)
        .collect();
    let mut order = Vec::with_capacity(ir.txs.len());
    while !layer.is_empty() {
        let mut next_layer = BTreeSet::new();
        for done in &layer {
            order.push(done.to_string());
            if let Some(kids) = children.get(*done) {
                for kid in kids {
                    if let Some(p) = parents.get_mut(kid) {
                        p.remove(*done);
                        if p.is_empty() {
                            next_layer.insert(*kid);
                        }
                    }
                }
            }
        }
        layer = next_layer;
    }
    if order.len() != ir.txs.len() {
        let stuck: Vec<&str> = parents
            .iter()
            .filter(|(_, p)| !p.is_empty())
            .map(|(name, _)| *name)
            .collect();
        return Err(Error::Graph(format!(
            "cycle involving {}",
            stuck.join(", ")
        )));
    }
    Ok(order)
}
