//! The site's funds: a pool key split into slot addresses, and sweeps.
//!
//! The pool key holds the site's test ADA. [`split`] pays a fixed amount to
//! each slot address. A run leases one slot and is funded from that slot's
//! UTxOs, so runs at the same time never spend the same UTxO. [`sweep`]
//! returns what a run leaves at its run address to its slot, burning the
//! native assets the run minted on the way, so a slot only ever holds ADA.

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

/// A UTxO a run left, with the native assets it holds.
#[derive(Clone, Debug, PartialEq)]
pub struct HeldUtxo {
    pub utxo: OfferedUtxo,
    pub assets: Assets,
}

impl From<OfferedUtxo> for HeldUtxo {
    fn from(utxo: OfferedUtxo) -> HeldUtxo {
        HeldUtxo {
            utxo,
            assets: Assets::new(),
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
    let mut planned = Vec::new();
    for slot in slots {
        let key = SecretKey::from(slot_key(pool, slot).0);
        planned.push(PlannedOutput {
            address: key_address(&key, network.into()),
            lovelace: Some(lovelace),
            assets: Assets::new(),
        });
    }
    let key = SecretKey::from(pool.0);
    planned.push(PlannedOutput {
        address: key_address(&key, network.into()),
        lovelace: None,
        assets: Assets::new(),
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
/// transactions as [`SWEEP_INPUTS`] and [`SWEEP_BURN_BYTES`] need, burning
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
    let burnable = policies.by_id();
    let mut chunks: Vec<Vec<HeldUtxo>> = Vec::new();
    let mut bytes = 0;
    for utxo in utxos {
        if !utxo.assets.keys().all(|id| burnable.contains_key(id)) {
            continue;
        }
        let more = burn_bytes(&utxo.assets);
        match chunks.last_mut() {
            Some(chunk) if chunk.len() < SWEEP_INPUTS && bytes + more <= SWEEP_BURN_BYTES => {
                chunk.push(utxo.clone());
                bytes += more;
            }
            _ => {
                chunks.push(vec![utxo.clone()]);
                bytes = more;
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
    let built = build_signed_minting(&inputs, planned, metadata, key, pp, format, burn)?;
    Ok(SignedTx {
        tx_id: hex::encode(built.tx_id),
        fee: built.fee,
        size: built.bytes.len(),
        outputs: built.outputs,
        cbor_hex: hex::encode(&built.bytes),
    })
}
