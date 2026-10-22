//! Native assets: time-lock minting policies, mint and burn, and the tokens
//! each output holds.
//!
//! A policy is a native script that only says from which slot on it may be
//! used, `after_slot: N` (the ledger's `InvalidBefore N`). It needs no key, so
//! anyone can mint or burn under it once the chain is past slot N; on a test
//! network that is fine, and it keeps the IR free of keys and networks. Each
//! N is a different script and so a different policy id, which is how a spec
//! gets as many distinct policy ids as it likes: `after_slot: 1`, `2`, `3`, ….
//!
//! Assets are written `<policy>.<asset>`: an IR policy name and an asset name
//! of 1 to 32 letters, digits, `_` or `-`, which is also the asset's name on
//! chain. A tx mints (positive) or burns (negative) under `mint`, and an
//! output holds `tokens`. Sources hold ADA only, so every token starts at a
//! mint. The `change` output of a tx takes every token its inputs and mint
//! hold that its other outputs do not, as it takes the leftover ADA.
//! [`flow`] works out what each output holds and rejects a tx that pays or
//! burns more than it has.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ir::{Amount, AmountKeyword, Ir, Ref};

/// Longest asset name the ledger accepts, in bytes.
pub const MAX_ASSET_NAME: usize = 32;

/// A minting policy: a native script usable from slot `after_slot` on.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// The first slot a tx minting or burning under it may be valid in.
    /// Each value is a different script, so a different policy id.
    pub after_slot: u64,
}

/// Tokens by `<policy>.<asset>`.
pub type Tokens = BTreeMap<String, u64>;

/// What a tx mints (positive) or burns (negative), by `<policy>.<asset>`.
pub type Mint = BTreeMap<String, i64>;

/// Split `<policy>.<asset>` into the policy and the asset name, and check
/// the asset name's spelling. The policy may be namespaced (`p/gold.a1`).
pub fn split(key: &str) -> Result<(&str, &str)> {
    let (policy, asset) = key
        .rsplit_once('.')
        .ok_or_else(|| Error::Graph(format!("`{key}` is not `<policy>.<asset>`")))?;
    if policy.is_empty() {
        return Err(Error::Graph(format!("`{key}` names no policy")));
    }
    if asset.is_empty()
        || asset.len() > MAX_ASSET_NAME
        || !asset
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(Error::Graph(format!(
            "`{key}`: an asset name is 1 to {MAX_ASSET_NAME} letters, digits, `_` or `-`"
        )));
    }
    Ok((policy, asset))
}

/// Check the IR's policies and every `mint` and `tokens` key against them.
pub fn validate(ir: &Ir) -> Result<()> {
    let mut by_slot: BTreeMap<u64, &str> = BTreeMap::new();
    for (name, policy) in &ir.policies {
        if let Some(other) = by_slot.insert(policy.after_slot, name) {
            return Err(Error::Graph(format!(
                "policies `{other}` and `{name}` are both `after_slot: {}`, one script \
                 with one policy id; give each its own slot",
                policy.after_slot
            )));
        }
    }
    let known = |tx: &str, key: &str| -> Result<()> {
        let (policy, _) = split(key).map_err(|e| in_tx(tx, e))?;
        if !ir.policies.contains_key(policy) {
            return Err(Error::Graph(format!(
                "tx `{tx}`: `{key}` names unknown policy `{policy}`"
            )));
        }
        Ok(())
    };
    for (name, tx) in &ir.txs {
        for (key, quantity) in &tx.mint {
            known(name, key)?;
            if *quantity == 0 {
                return Err(Error::Graph(format!(
                    "tx `{name}`: mints 0 of `{key}`; leave it out"
                )));
            }
        }
        for output in &tx.outputs {
            if output.tokens.is_empty() {
                continue;
            }
            if output.lovelace == Amount::Keyword(AmountKeyword::Change) {
                return Err(Error::Graph(format!(
                    "tx `{name}`: the `change` output takes every token left, so it lists no \
                     `tokens`"
                )));
            }
            for (key, quantity) in &output.tokens {
                known(name, key)?;
                if *quantity == 0 {
                    return Err(Error::Graph(format!(
                        "tx `{name}`: an output holds 0 of `{key}`; leave it out"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// The tokens every output holds, by `tx.N`, for the txs in `order` (a
/// topological order), leaving out outputs that hold none. Fails naming the
/// tx that pays out or burns more of an asset than its inputs and mint hold.
pub fn flow(ir: &Ir, order: &[String]) -> Result<BTreeMap<String, Tokens>> {
    let mut held: BTreeMap<String, Tokens> = BTreeMap::new();
    if ir.policies.is_empty() {
        return Ok(held);
    }
    for name in order {
        let Some(tx) = ir.txs.get(name) else {
            continue;
        };
        // What the inputs hold plus the mint, as signed totals.
        let mut balance: BTreeMap<String, i128> = BTreeMap::new();
        for input in &tx.inputs {
            if let Ref::Output { .. } = Ref::parse(input)? {
                for (key, quantity) in held.get(input).into_iter().flatten() {
                    *balance.entry(key.clone()).or_default() += i128::from(*quantity);
                }
            }
        }
        for (key, quantity) in &tx.mint {
            let entry = balance.entry(key.clone()).or_default();
            *entry += i128::from(*quantity);
            if *entry < 0 {
                return Err(Error::Graph(format!(
                    "tx `{name}` burns {} of `{key}`, but its inputs hold only {}",
                    -quantity,
                    *entry - i128::from(*quantity)
                )));
            }
        }
        let mut change_index = None;
        for (index, output) in tx.outputs.iter().enumerate() {
            if output.lovelace == Amount::Keyword(AmountKeyword::Change) {
                change_index = Some(index);
                continue;
            }
            for (key, quantity) in &output.tokens {
                let entry = balance.entry(key.clone()).or_default();
                *entry -= i128::from(*quantity);
                if *entry < 0 {
                    return Err(Error::Graph(format!(
                        "tx `{name}` pays out more `{key}` than its inputs and mint hold \
                         ({} short)",
                        -*entry
                    )));
                }
            }
            if !output.tokens.is_empty() {
                held.insert(format!("{name}.{index}"), output.tokens.clone());
            }
        }
        let rest: Tokens = balance
            .into_iter()
            .filter(|(_, quantity)| *quantity > 0)
            .map(|(key, quantity)| {
                u64::try_from(quantity)
                    .map(|q| (key.clone(), q))
                    .map_err(|_| Error::Graph(format!("tx `{name}`: `{key}` overflows")))
            })
            .collect::<Result<_>>()?;
        if let (Some(index), false) = (change_index, rest.is_empty()) {
            held.insert(format!("{name}.{index}"), rest);
        }
    }
    Ok(held)
}

fn in_tx(tx: &str, error: Error) -> Error {
    match error {
        Error::Graph(message) => Error::Graph(format!("tx `{tx}`: {message}")),
        other => other,
    }
}
