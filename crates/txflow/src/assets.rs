//! Native assets on chain: the IR's time-lock policies as native scripts,
//! tokens as multi-asset values, and the min-UTxO and size of such values.
//!
//! A policy `after_slot: N` is the native script `InvalidBefore N`, CBOR
//! `[4, N]`; its policy id is blake2b-224 over a 0 byte and that CBOR. It
//! needs no signature, so a tx that mints or burns under it only has to
//! carry the script and be valid from slot N on. An asset's name on chain is
//! its IR name's bytes (`a1`, `gold`).

use std::collections::BTreeMap;

use pallas_addresses::Address;
use pallas_codec::minicbor;
use pallas_codec::utils::KeepRaw;
use pallas_crypto::hash::{Hash, Hasher};
use pallas_primitives::conway;
use serde::Serialize;

use crate::context::ProtocolParams;
use crate::error::{Error, Result};
use crate::ir::Ir;
use txflow_core::assets::{split, Mint, Tokens};

/// Multi-asset quantities by policy id and asset name, in ledger order.
pub type Assets = BTreeMap<[u8; 28], BTreeMap<Vec<u8>, u64>>;

/// Constant overhead the ledger adds to an output's size for min-UTxO.
const OUTPUT_OVERHEAD: u64 = 160;

/// The native script of policy `after_slot: N`, as CBOR.
pub fn policy_script(after_slot: u64) -> Vec<u8> {
    // `[4, N]`: an array of two, the `InvalidBefore` tag and the slot.
    let mut cbor = Vec::with_capacity(11);
    minicbor::encode(conway::NativeScript::InvalidBefore(after_slot), &mut cbor)
        .unwrap_or_default();
    cbor
}

/// The policy id of a native script: blake2b-224 over a 0 tag and its CBOR.
pub fn script_hash(script: &[u8]) -> [u8; 28] {
    *Hasher::<224>::hash_tagged(script, 0)
}

/// One IR policy, bound to its script.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundPolicy {
    pub after_slot: u64,
    pub id: [u8; 28],
    pub script: Vec<u8>,
}

/// What the manifest says about a policy.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ManifestPolicy {
    pub after_slot: u64,
    /// The policy id, hex.
    pub policy_id: String,
}

/// The IR's policies, bound to scripts and ids.
#[derive(Clone, Debug, Default)]
pub struct Policies {
    by_name: BTreeMap<String, BoundPolicy>,
}

/// The mint or burn part of a tx: quantities, the scripts that allow them,
/// and the slot the tx is valid from.
#[derive(Clone, Debug, PartialEq)]
pub struct Minting {
    pub mint: BTreeMap<[u8; 28], BTreeMap<Vec<u8>, i64>>,
    /// Native scripts, one per policy used, in policy id order.
    pub scripts: Vec<Vec<u8>>,
    /// The latest `after_slot` of the policies used.
    pub valid_from: u64,
}

impl Policies {
    pub fn of(ir: &Ir) -> Policies {
        Policies {
            by_name: ir
                .policies
                .iter()
                .map(|(name, policy)| {
                    let script = policy_script(policy.after_slot);
                    (
                        name.clone(),
                        BoundPolicy {
                            after_slot: policy.after_slot,
                            id: script_hash(&script),
                            script,
                        },
                    )
                })
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// The policies by name, as the manifest lists them.
    pub fn manifest(&self) -> BTreeMap<String, ManifestPolicy> {
        self.by_name
            .iter()
            .map(|(name, p)| {
                (
                    name.clone(),
                    ManifestPolicy {
                        after_slot: p.after_slot,
                        policy_id: hex::encode(p.id),
                    },
                )
            })
            .collect()
    }

    /// Every policy, by id: what a sweep may burn under.
    pub fn by_id(&self) -> BTreeMap<[u8; 28], &BoundPolicy> {
        self.by_name.values().map(|p| (p.id, p)).collect()
    }

    fn asset<'a>(&self, key: &'a str) -> Result<(&BoundPolicy, &'a str)> {
        let (policy, name) = split(key)?;
        let bound = self
            .by_name
            .get(policy)
            .ok_or_else(|| Error::Materialise(format!("`{key}` names unknown policy")))?;
        Ok((bound, name))
    }

    /// `tokens` as a value's multi-asset part.
    pub fn assets(&self, tokens: &Tokens) -> Result<Assets> {
        let mut assets = Assets::new();
        for (key, quantity) in tokens {
            let (policy, name) = self.asset(key)?;
            *assets
                .entry(policy.id)
                .or_default()
                .entry(name.as_bytes().to_vec())
                .or_default() += quantity;
        }
        Ok(assets)
    }

    /// The mint part of a tx, or `None` when it mints nothing.
    pub fn minting(&self, mint: &Mint) -> Result<Option<Minting>> {
        if mint.is_empty() {
            return Ok(None);
        }
        let mut quantities: BTreeMap<[u8; 28], BTreeMap<Vec<u8>, i64>> = BTreeMap::new();
        let mut used: BTreeMap<[u8; 28], &BoundPolicy> = BTreeMap::new();
        for (key, quantity) in mint {
            let (policy, name) = self.asset(key)?;
            quantities
                .entry(policy.id)
                .or_default()
                .insert(name.as_bytes().to_vec(), *quantity);
            used.insert(policy.id, policy);
        }
        Ok(Some(Minting {
            mint: quantities,
            valid_from: used.values().map(|p| p.after_slot).max().unwrap_or(0),
            scripts: used.values().map(|p| p.script.clone()).collect(),
        }))
    }
}

/// Burn every asset of `assets`, which must all be under `policies`
/// (by id): the mint part of a sweep.
pub fn burn_all(
    assets: &Assets,
    policies: &BTreeMap<[u8; 28], &BoundPolicy>,
) -> Result<Option<Minting>> {
    if assets.is_empty() {
        return Ok(None);
    }
    let mut mint = BTreeMap::new();
    let mut scripts = Vec::new();
    let mut valid_from = 0;
    for (id, names) in assets {
        let policy = policies.get(id).ok_or_else(|| {
            Error::Materialise(format!("no script for policy {}", hex::encode(id)))
        })?;
        let mut burned = BTreeMap::new();
        for (name, quantity) in names {
            let quantity = i64::try_from(*quantity)
                .map_err(|_| Error::Materialise("burn quantity overflows".to_string()))?;
            burned.insert(name.clone(), -quantity);
        }
        mint.insert(*id, burned);
        scripts.push(policy.script.clone());
        valid_from = valid_from.max(policy.after_slot);
    }
    Ok(Some(Minting {
        mint,
        scripts,
        valid_from,
    }))
}

/// Add `more` to `assets`.
pub fn add(assets: &mut Assets, more: &Assets) {
    for (id, names) in more {
        let entry = assets.entry(*id).or_default();
        for (name, quantity) in names {
            *entry.entry(name.clone()).or_default() += quantity;
        }
    }
}

/// Number of distinct assets.
pub fn count(assets: &Assets) -> usize {
    assets.values().map(BTreeMap::len).sum()
}

/// `lovelace` and `assets` as a ledger value.
pub fn value(lovelace: u64, assets: &Assets) -> Result<conway::Value> {
    if assets.is_empty() {
        return Ok(conway::Value::Coin(lovelace));
    }
    let mut multiasset = BTreeMap::new();
    for (id, names) in assets {
        let mut inner = BTreeMap::new();
        for (name, quantity) in names {
            let positive = conway::PositiveCoin::try_from(*quantity)
                .map_err(|_| Error::Materialise("an output holds 0 of an asset".to_string()))?;
            inner.insert(name.clone().into(), positive);
        }
        multiasset.insert(Hash::new(*id), inner);
    }
    Ok(conway::Value::Multiasset(lovelace, multiasset))
}

/// Serialised size of the value, as the ledger compares it with maxValueSize.
pub fn value_size(lovelace: u64, assets: &Assets) -> Result<u64> {
    let encoded = minicbor::to_vec(value(lovelace, assets)?)
        .map_err(|e| Error::Materialise(format!("encoding a value: {e}")))?;
    Ok(encoded.len() as u64)
}

/// `(160 + output size) * coinsPerUTxOByte` for an output to `address`
/// holding `assets`, sized with the largest coin encoding so it is an upper
/// bound.
pub fn min_lovelace(address: &Address, assets: &Assets, pp: &ProtocolParams) -> Result<u64> {
    let output =
        conway::TransactionOutput::PostAlonzo(KeepRaw::from(conway::PostAlonzoTransactionOutput {
            address: address.to_vec().into(),
            value: value(u64::MAX, assets)?,
            datum_option: None,
            script_ref: None,
        }));
    let size = minicbor::to_vec(&output)
        .map_err(|e| Error::Materialise(format!("encoding an output: {e}")))?
        .len() as u64;
    (OUTPUT_OVERHEAD + size)
        .checked_mul(pp.coins_per_utxo_byte)
        .ok_or_else(|| Error::Materialise("min UTxO overflows".to_string()))
}
