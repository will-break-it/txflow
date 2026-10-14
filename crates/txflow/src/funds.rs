//! The site's funds: a pool key split into slot addresses, and sweeps.
//!
//! The pool key holds the site's test ADA. [`split`] pays a fixed amount to
//! each slot address. A run leases one slot and is funded from that slot's
//! UTxOs, so runs at the same time never spend the same UTxO. [`sweep`]
//! returns what a run leaves at its run address to its slot.

use pallas_crypto::hash::{Hash, Hasher};
use pallas_crypto::key::ed25519::SecretKey;
use pallas_txbuilder::Input;
use serde::Serialize;

use crate::context::{NetworkName, OfferedUtxo, ProtocolParams, SecretKeyBytes};
use crate::error::{Error, Result};
use crate::materialise::{
    bech32, build_signed, change_address, key_address, message, watermark, PlannedOutput, TxFormat,
};

/// Which run a sweep returns, for its message: the run id and where the run
/// was built (see [`watermark`]).
pub struct RunTag<'a> {
    pub id: &'a str,
    pub origin: &'a str,
}

/// Inputs per sweep transaction, well under the size limit.
pub const SWEEP_INPUTS: usize = 100;

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
        });
    }
    let key = SecretKey::from(pool.0);
    planned.push(PlannedOutput {
        address: key_address(&key, network.into()),
        lovelace: None,
    });
    transfer(
        &key,
        utxos,
        &planned,
        &message(&[&watermark("cli"), "split"]),
        pp,
        format,
    )
}

/// Spend every one of `utxos` (held by `key`) to `to`, in as many
/// transactions as [`SWEEP_INPUTS`] needs. `run` goes in the metadata.
pub fn sweep(
    key: &SecretKeyBytes,
    utxos: &[OfferedUtxo],
    to: &str,
    run: &RunTag,
    network: NetworkName,
    pp: &ProtocolParams,
    format: TxFormat,
) -> Result<Vec<SignedTx>> {
    let to = change_address(to, network.into())?;
    let key = SecretKey::from(key.0);
    utxos
        .chunks(SWEEP_INPUTS)
        .map(|chunk| {
            let planned = [PlannedOutput {
                address: to.clone(),
                lovelace: None,
            }];
            transfer(
                &key,
                chunk,
                &planned,
                &message(&[&watermark(run.origin), "sweep", &format!("run:{}", run.id)]),
                pp,
                format,
            )
        })
        .collect()
}

fn transfer(
    key: &SecretKey,
    utxos: &[OfferedUtxo],
    planned: &[PlannedOutput],
    metadata: &[u8],
    pp: &ProtocolParams,
    format: TxFormat,
) -> Result<SignedTx> {
    if utxos.is_empty() {
        return Err(Error::Materialise("nothing to spend".to_string()));
    }
    let inputs: Vec<(Input, u64)> = utxos
        .iter()
        .map(|u| (Input::new(Hash::new(u.tx_id), u.index), u.lovelace))
        .collect();
    let built = build_signed(&inputs, &[], planned, metadata, key, pp, format)?;
    Ok(SignedTx {
        tx_id: hex::encode(built.tx_id),
        fee: built.fee,
        size: built.bytes.len(),
        outputs: built.outputs,
        cbor_hex: hex::encode(&built.bytes),
    })
}
