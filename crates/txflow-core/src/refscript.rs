//! Reference scripts (CIP-33) on outputs.
//!
//! An output may carry a reference script, asked for by size only:
//! `reference_script: { bytes: N }`. Materialise makes it a Plutus V3 script
//! of exactly N bytes that nothing runs. A tx that spends or reads such an
//! output pays the ledger's reference-script fee for it, so a spec can load
//! transactions with reference scripts without writing any script. The
//! graph does not change: a reference script is part of the output, and
//! reading it is a reference input like any other.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::ir::{Ir, Ref, Tx};

/// The smallest reference script txflow makes: the program around an empty
/// bytestring.
pub const MIN_BYTES: u32 = 8;
/// The largest transaction mainnet and Musashi take (`maxTxSize`), which
/// the outputs one tx creates, scripts and all, must fit in.
pub const MAX_TX_BYTES: u64 = 16_384;
/// About what a txflow tx takes besides its scripts at the least: one input,
/// the script output and the change, a signature and the attribution
/// metadata, with some room (a tx with one 15 000-byte script is 15 350 bytes).
const TX_OVERHEAD_BYTES: u64 = 400;
/// Sizes no Plutus script has. A script's bytes are its flat encoding in a
/// CBOR byte string, whose header grows from one byte to two at 24 bytes of
/// flat, from two to three at 256 and from three to five at 65 536. One more
/// byte of flat there adds two or three bytes to the script, so the sizes in
/// between are skipped.
pub const UNREACHABLE: [u32; 4] = [25, 258, 65_539, 65_540];
/// The most bytes of reference scripts one transaction may spend or read
/// on Conway (`maxRefScriptSizePerTx`, 200 KiB). Dijkstra makes it a
/// protocol parameter, which materialise checks; `check` warns past this.
/// It also bounds one script, which must fit in a transaction anyway.
pub const MAX_BYTES_PER_TX: u64 = 204_800;

/// A reference script an output carries.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReferenceScript {
    /// The script's size, as the ledger counts it for the reference-script fee.
    pub bytes: u32,
}

impl ReferenceScript {
    /// The script is one txflow can make: a size some Plutus script has.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.bytes < MIN_BYTES {
            return Err(format!(
                "a reference script of {} bytes is below the smallest, {MIN_BYTES}",
                self.bytes
            ));
        }
        if u64::from(self.bytes) > MAX_BYTES_PER_TX {
            return Err(format!(
                "a reference script of {} bytes is above {MAX_BYTES_PER_TX}, more than a \
                 transaction can hold",
                self.bytes
            ));
        }
        if UNREACHABLE.contains(&self.bytes) {
            let reachable = |b: &u32| !UNREACHABLE.contains(b);
            let below = (MIN_BYTES..self.bytes)
                .rev()
                .find(reachable)
                .unwrap_or(MIN_BYTES);
            let above = (self.bytes + 1..).find(reachable).unwrap_or(self.bytes);
            return Err(format!(
                "no Plutus script is exactly {} bytes (its CBOR header lengthens there, \
                 so the sizes skip it); use {below} or {above}",
                self.bytes
            ));
        }
        Ok(())
    }
}

/// The reference script on `reference`, a source or `tx.N`, if any. Sources
/// never carry one.
pub fn on(ir: &Ir, reference: &str) -> Result<Option<ReferenceScript>> {
    Ok(match Ref::parse(reference)? {
        Ref::Source(_) => None,
        Ref::Output { tx, index } => ir
            .txs
            .get(&tx)
            .and_then(|t| t.outputs.get(index))
            .and_then(|o| o.reference_script),
    })
}

/// Bytes of reference scripts `tx` spends or reads, as the ledger counts them
/// for its fee and its per-tx limit. Validation refuses a tx that lists an
/// output twice, so each counts once.
pub fn read_by(ir: &Ir, tx: &Tx) -> Result<u64> {
    let mut total = 0u64;
    for reference in tx.inputs.iter().chain(tx.reference_inputs.iter()) {
        if let Some(script) = on(ir, reference)? {
            total += u64::from(script.bytes);
        }
    }
    Ok(total)
}

/// Bytes of reference scripts `tx`'s own outputs carry.
pub fn carried_by(tx: &Tx) -> u64 {
    tx.outputs
        .iter()
        .filter_map(|o| o.reference_script.map(|s| u64::from(s.bytes)))
        .sum()
}

/// Bytes of reference scripts each tx spends or reads, by name; txs that
/// read none are left out.
pub fn reads(ir: &Ir) -> Result<BTreeMap<&String, u64>> {
    let mut out = BTreeMap::new();
    for (name, tx) in &ir.txs {
        let bytes = read_by(ir, tx)?;
        if bytes > 0 {
            out.insert(name, bytes);
        }
    }
    Ok(out)
}

/// Warnings about reference scripts no Cardano network takes today: a tx
/// that creates more script bytes than a [`MAX_TX_BYTES`] transaction holds,
/// and one that spends or reads more than Conway's [`MAX_BYTES_PER_TX`].
/// Both limits are protocol parameters, which only materialise knows, so
/// these are warnings here.
pub fn warnings(ir: &Ir, reads: &BTreeMap<&String, u64>) -> Vec<String> {
    let mut out = Vec::new();
    for (name, tx) in &ir.txs {
        let made = carried_by(tx);
        if made + TX_OVERHEAD_BYTES > MAX_TX_BYTES {
            out.push(format!(
                "tx `{name}` creates {made} bytes of reference scripts; with its input, \
                 change, signature and txflow's metadata that is more than a \
                 {MAX_TX_BYTES}-byte transaction (maxTxSize on mainnet and Musashi) holds"
            ));
        }
    }
    for (name, read) in reads {
        if *read > MAX_BYTES_PER_TX {
            out.push(format!(
                "tx `{name}` spends or reads {read} bytes of reference scripts, more than \
                 the {MAX_BYTES_PER_TX} a Conway network allows per tx; a Dijkstra network \
                 accepts it only with a larger maxRefScriptSizePerTx"
            ));
        }
    }
    out
}
