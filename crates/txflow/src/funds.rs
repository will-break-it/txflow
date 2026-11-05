//! The site's funds: a pool key split into slot addresses, and sweeps.
//!
//! The pool key holds the site's test ADA. [`split`] pays a fixed amount to
//! each slot address. A run leases one slot and is funded from that slot's
//! UTxOs, so runs at the same time never spend the same UTxO. [`sweep`]
//! returns what a run leaves at its run address to its slot, burning the
//! native assets the run minted on the way, so a slot only ever holds ADA,
//! and paying the reference-script fee for outputs that carry scripts.

use pallas_crypto::hash::{Hash, Hasher};
use pallas_crypto::key::ed25519::SecretKey;
use pallas_txbuilder::Input;
use serde::Serialize;

use crate::assets::{self, Assets, Policies};
use crate::context::{NetworkName, OfferedUtxo, ProtocolParams, SecretKeyBytes};
use crate::error::{Error, Result};
use crate::materialise::{
    bech32, build_signed_minting, change_address, key_address, message, watermark, PlannedOutput,
    TxFormat,
};
use crate::refscript;

/// Which run a sweep returns, for its message: the run id and where the run
/// was built (see [`watermark`]).
pub struct RunTag<'a> {
    pub id: &'a str,
    pub origin: &'a str,
}

/// Inputs per sweep transaction, well under the size limit.
pub const SWEEP_INPUTS: usize = 100;

/// Bytes of burned assets per sweep transaction: what a sweep's mint field
/// may take, well under the size limit with its inputs.
pub const SWEEP_BURN_BYTES: usize = 8_000;

/// About how many bytes a sweep tx of one input takes: body, metadata and
/// one witness.
const SWEEP_TX_BYTES: u64 = 300;

/// A UTxO a run left, with the native assets it holds and the size of the
/// reference script it carries, which its sweep pays a fee for. Callers must
/// fill `reference_script_bytes` from the UTxO as queried: for a Plutus
/// script, the size the ledger counts, its flat bytes in a CBOR byte string
/// with that string's header. `referenceScript.script.cborHex` in
/// `cardano-cli query utxo --output-json` wraps those bytes in one more CBOR
/// byte string: count what is inside that outer one; a UTxO
/// given 0 that does carry a script gets a sweep the ledger refuses as
/// underpaid.
#[derive(Clone, Debug, PartialEq)]
pub struct HeldUtxo {
    pub utxo: OfferedUtxo,
    pub assets: Assets,
    pub reference_script_bytes: u32,
}

impl From<OfferedUtxo> for HeldUtxo {
    fn from(utxo: OfferedUtxo) -> HeldUtxo {
        HeldUtxo {
            utxo,
            assets: Assets::new(),
            reference_script_bytes: 0,
        }
    }
}

/// About how many bytes burning `assets` adds to a tx's mint field.
fn burn_bytes(assets: &Assets) -> usize {
    assets
        .values()
        .map(|names| 31 + names.keys().map(|n| n.len() + 10).sum::<usize>())
        .sum()
}

/// A built and signed transaction, ready to submit.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct SignedTx {
    pub tx_id: String,
    pub fee: u64,
    pub size: usize,
    /// Lovelace of each output, in output order.
    pub outputs: Vec<u64>,
    pub cbor_hex: String,
}

/// The key of slot `slot`: blake2b256 over a tag, the pool key and the slot.
pub fn slot_key(pool: &SecretKeyBytes, slot: u32) -> SecretKeyBytes {
    let mut hasher = Hasher::<256>::new();
    hasher.input(b"txflow/slot-key/v0");
    hasher.input(&pool.0);
    hasher.input(&slot.to_be_bytes());
    SecretKeyBytes(*hasher.finalize())
}

/// The enterprise address of `key`, in bech32.
pub fn address(key: &SecretKeyBytes, network: NetworkName) -> Result<String> {
    bech32(&key_address(&SecretKey::from(key.0), network.into()))
}

/// Pay `lovelace` to each slot address in `slots` from the pool's `utxos`;
/// the rest goes back to the pool address. Slots are numbered from 1, as the
/// Worker's funding pool numbers them, so `6..=30` adds lanes 6 to 30.
pub fn split(
    pool: &SecretKeyBytes,
    utxos: &[OfferedUtxo],
    slots: std::ops::RangeInclusive<u32>,
    lovelace: u64,
    network: NetworkName,
    pp: &ProtocolParams,
    format: TxFormat,
) -> Result<SignedTx> {
    let pp = &format.parameters(pp);
    let mut planned = Vec::new();
    for slot in slots {
        let key = SecretKey::from(slot_key(pool, slot).0);
        planned.push(PlannedOutput {
            address: key_address(&key, network.into()),
            lovelace: Some(lovelace),
            assets: Assets::new(),
            script: None,
        });
    }
    let key = SecretKey::from(pool.0);
    planned.push(PlannedOutput {
        address: key_address(&key, network.into()),
        lovelace: None,
        assets: Assets::new(),
        script: None,
    });
    let held: Vec<HeldUtxo> = utxos.iter().cloned().map(HeldUtxo::from).collect();
    transfer(
        &key,
        &held,
        &planned,
        &message(&[&watermark("cli"), "split"]),
        pp,
        format,
        None,
    )
}

/// Spend every one of `utxos` (held by `key`) to `to`, in as many
/// transactions as [`SWEEP_INPUTS`], [`SWEEP_BURN_BYTES`] and the ledger's
/// per-tx limit on reference scripts need, paying the reference-script fee
/// for those the UTxOs carry (one whose script cannot be paid for, as `pp`
/// has no price or the script is past its limit, is left where it is), and
/// burning
/// their native assets under the run's `policies`, so only ADA reaches `to`.
/// A UTxO holding an asset of any other policy cannot be burned, so it is
/// left where it is. `run` goes in the metadata.
#[allow(clippy::too_many_arguments)]
pub fn sweep(
    key: &SecretKeyBytes,
    utxos: &[HeldUtxo],
    policies: &Policies,
    to: &str,
    run: &RunTag,
    network: NetworkName,
    pp: &ProtocolParams,
    format: TxFormat,
) -> Result<Vec<SignedTx>> {
    let to = change_address(to, network.into())?;
    let key = SecretKey::from(key.0);
    let pp = &format.parameters(pp);
    let burnable = policies.by_id();
    // A UTxO with a script it cannot pay for (no price, or past the limit)
    // stays where it is, like one with assets it cannot burn; pricing the
    // network gives but that makes no sense is an error.
    let carries_scripts = utxos.iter().any(|u| u.reference_script_bytes > 0);
    let pricing = match carries_scripts && pp.min_fee_ref_script_cost_per_byte.is_some() {
        true => Some(refscript::Pricing::of(pp)?),
        false => None,
    };
    let payable = |u: &HeldUtxo| {
        u.reference_script_bytes == 0
            || pricing.is_some_and(|p| u64::from(u.reference_script_bytes) <= p.max_per_tx)
    };
    let sweepable: Vec<&HeldUtxo> = utxos
        .iter()
        .filter(|u| u.assets.keys().all(|id| burnable.contains_key(id)) && payable(u))
        .collect();
    let script_cap = match (
        &pricing,
        sweepable.iter().any(|u| u.reference_script_bytes > 0),
    ) {
        (Some(pricing), true) => {
            let total = sweepable
                .iter()
                .map(|u| u64::from(u.reference_script_bytes))
                .sum();
            sweep_script_bytes(pricing, pp, total)?
        }
        _ => u64::MAX,
    };
    let mut chunks: Vec<Vec<HeldUtxo>> = Vec::new();
    let (mut bytes, mut scripts) = (0, 0u64);
    for utxo in sweepable {
        let more = burn_bytes(&utxo.assets);
        let script = u64::from(utxo.reference_script_bytes);
        match chunks.last_mut() {
            Some(chunk)
                if chunk.len() < SWEEP_INPUTS
                    && bytes + more <= SWEEP_BURN_BYTES
                    && (script == 0 || scripts == 0 || scripts + script <= script_cap) =>
            {
                chunk.push(utxo.clone());
                bytes += more;
                scripts += script;
            }
            _ => {
                chunks.push(vec![utxo.clone()]);
                bytes = more;
                scripts = script;
            }
        }
    }
    chunks
        .iter()
        .map(|chunk| {
            let mut burned = Assets::new();
            for utxo in chunk {
                assets::add(&mut burned, &utxo.assets);
            }
            let planned = [PlannedOutput {
                address: to.clone(),
                lovelace: None,
                assets: Assets::new(),
                script: None,
            }];
            transfer(
                &key,
                chunk,
                &planned,
                &message(&[&watermark(run.origin), "sweep", &format!("run:{}", run.id)]),
                pp,
                format,
                assets::burn_all(&burned, &burnable)?.as_ref(),
            )
        })
        .collect()
}

/// Bytes of reference scripts one sweep tx spends at most: as many fee tiers
/// as cost less extra, over the first tier's price, than a sweep tx of their
/// own (`txFeeFixed` plus [`SWEEP_TX_BYTES`] at `txFeePerByte`), and never
/// past the network's per-tx limit or `total`, all there is to sweep. Each tier is
/// dearer per byte than the one before, so from some tier on another sweep
/// tx is cheaper: the third, at Conway's 15 lovelace a byte and 1.2.
fn sweep_script_bytes(
    pricing: &refscript::Pricing,
    pp: &ProtocolParams,
    total: u64,
) -> Result<u64> {
    let limit = pricing.max_per_tx.min(total).max(1);
    let another_tx = pp
        .min_fee_a
        .saturating_mul(SWEEP_TX_BYTES)
        .saturating_add(pp.min_fee_b);
    // What `here` bytes would cost at the first tier's price.
    let flat = |here: u64| {
        u128::from(here) * u128::from(pricing.price.numerator)
            / u128::from(pricing.price.denominator)
    };
    // Tiers that cost no more than the first never call for another tx.
    if pricing.multiplier.numerator <= pricing.multiplier.denominator {
        return Ok(limit);
    }
    // Whether the `here` bytes from `start` cost no more extra than another
    // tx. A fee past u64 is surely not worth it.
    let worth = |start: u64, here: u64| -> Result<bool> {
        let (Ok(end), Ok(begin)) = (pricing.fee(start + here), pricing.fee(start)) else {
            return Ok(false);
        };
        Ok(u128::from(end - begin).saturating_sub(flat(here)) <= u128::from(another_tx))
    };
    // Each tier is dearer than the last, so the full tiers worth taking are
    // a prefix: find its end by bisection, whatever the stride. The first
    // costs nothing extra.
    let stride = pricing.stride.min(limit);
    let full = limit / stride;
    let (mut lo, mut hi) = (1, full);
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        match worth((mid - 1) * stride, stride)? {
            true => lo = mid,
            false => hi = mid - 1,
        }
    }
    let bytes = lo * stride;
    // Then the partial tier up to the limit, if every full one was worth it.
    let rest = limit - bytes;
    if lo == full && rest > 0 && worth(bytes, rest)? {
        return Ok(limit);
    }
    Ok(bytes)
}

fn transfer(
    key: &SecretKey,
    utxos: &[HeldUtxo],
    planned: &[PlannedOutput],
    metadata: &[u8],
    pp: &ProtocolParams,
    format: TxFormat,
    burn: Option<&assets::Minting>,
) -> Result<SignedTx> {
    if utxos.is_empty() {
        return Err(Error::Materialise("nothing to spend".to_string()));
    }
    let inputs: Vec<(Input, u64)> = utxos
        .iter()
        .map(|u| {
            (
                Input::new(Hash::new(u.utxo.tx_id), u.utxo.index),
                u.utxo.lovelace,
            )
        })
        .collect();
    let scripts: u64 = utxos
        .iter()
        .map(|u| u64::from(u.reference_script_bytes))
        .sum();
    let built = build_signed_minting(&inputs, planned, metadata, key, pp, format, burn, scripts)?;
    Ok(SignedTx {
        tx_id: hex::encode(built.tx_id),
        fee: built.fee,
        size: built.bytes.len(),
        outputs: built.outputs,
        cbor_hex: hex::encode(&built.bytes),
    })
}
