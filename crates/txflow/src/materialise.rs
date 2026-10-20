//! Bind an IR to a chain context and build every transaction offline.
//!
//! Wallets all bind to one run key, derived from the funding key, the IR
//! hash and the seed, so a rerun of the same experiment and context yields
//! byte-identical transactions. Funding transactions spend the offered UTxOs
//! and create one output per IR source at the run address; experiment
//! transactions follow in topological order. Nothing is submitted.
//!
//! A tx with a script cost also spends a small output locked by the burner
//! script ([`crate::burner`]), its "fuel", with a redeemer that makes the
//! burner use that cost. Funding creates the fuel outputs and one collateral
//! output the script txs share; none of them is in the IR, so they add no
//! conflicts. Every script run is evaluated before the manifest is written,
//! and its measured cost is what the tx declares.

use std::collections::BTreeMap;

use pallas_addresses::{
    Address, Network, ShelleyAddress, ShelleyDelegationPart, ShelleyPaymentPart,
};
use pallas_crypto::hash::{Hash, Hasher};
use pallas_crypto::key::ed25519::SecretKey;
use pallas_txbuilder::{BuildConway, Input, Output, StagingTransaction};
use pallas_txbuilder::{Bytes, Bytes32, ExUnits, ScriptKind};
use serde::Serialize;

use crate::burner::{self, Calibration, Knobs, Units};
use crate::context::{ChainContext, NetworkName, OfferedUtxo, ProtocolParams, SecretKeyBytes};
use crate::derive;
use crate::error::{Error, Result};
use crate::ir::{Amount, AmountKeyword, Ir, Tx};
use crate::metadata::{auxiliary_data, Metadata};
use txflow_core::cost::Cost;

/// CIP-20 transaction message label, used to attribute txs on chain.
pub const METADATA_LABEL: u64 = txflow_core::metadata::MESSAGE_LABEL;

/// The first entry of every txflow tx's message: the tool and the version of
/// this message's layout. [`watermark`] adds where the tx was built.
pub const WATERMARK: &str = "txflow/1";

/// `txflow/1 <origin>`: the first message entry, e.g. `txflow/1 txflow.fun`
/// for the site or `txflow/1 cli` for the command line.
pub fn watermark(origin: &str) -> String {
    format!("{WATERMARK} {origin}")
}
/// At most this many IR sources are created by one funding transaction.
const SOURCES_PER_FUNDING_TX: usize = 200;
/// Upper bound on one input's serialised size: `[bytes(32), uint]`.
const INPUT_SIZE_BOUND: u64 = 44;
/// Upper bound on a change output, which may go to a base address.
const CHANGE_SIZE_BOUND: u64 = 100;
/// Upper bound on everything in a funding tx besides inputs and outputs:
/// headers, fee, metadata hash and metadata, one vkey witness.
const TX_OVERHEAD_BOUND: u64 = 512;
/// Upper bound on the serialised size of an ADA-only output to an enterprise
/// address (map header, address bytes, coin), used for the min-UTxO rule.
const OUTPUT_SIZE_BOUND: u64 = 65;
/// Constant overhead the ledger adds to an output's size for min-UTxO.
const OUTPUT_OVERHEAD: u64 = 160;
/// Fee iterations before giving up. Two or three are enough in practice.
const FEE_ROUNDS: usize = 8;

/// The materialised experiment: every transaction, signed, in release order.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Manifest {
    pub version: u32,
    pub experiment: String,
    pub ir_hash: String,
    pub run_id: String,
    pub seed: u64,
    /// Era of the target network, from the chain context.
    pub era: String,
    /// Era format the transactions are encoded in: `dijkstra` when the
    /// context names that era, otherwise `conway`.
    pub tx_format: String,
    pub network: NetworkName,
    pub network_magic: u32,
    /// The protocol parameters the fees and min-UTxO were computed with.
    pub pparams: ProtocolParams,
    /// `blake2b256:<hex>` over `pparams` (FR-014), to compare runs at a glance.
    pub pparams_hash: String,
    /// Enterprise address of the funding key: where the offered UTxOs are spent from.
    pub funding_address: String,
    /// Where the last funding tx returns change, if not `funding_address`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_address: Option<String>,
    pub run_address: String,
    /// Offered UTxOs the funding transactions spend, as `<tx id>#<index>`.
    pub funding_utxos: Vec<String>,
    /// The burner script, when any tx has a script cost.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub burner: Option<BurnerInfo>,
    pub txs: Vec<ManifestTx>,
}

/// The script txs with a cost run, and the collateral they share.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct BurnerInfo {
    /// blake2b-224 of the script, hex.
    pub script_hash: String,
    /// Enterprise address of the script, where fuel outputs are locked.
    pub address: String,
    /// The collateral output every script tx names, as `<tx id>#<index>`.
    pub collateral: String,
    /// The least a burner run costs under this cost model.
    pub floor: Units,
}

/// What a tx's script run was asked for and what it used.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ManifestCost {
    /// The cost as the spec wrote it.
    pub requested: Cost,
    /// The requested cost in units against this network's per-tx limit;
    /// `None` where the spec left a dimension to the burner.
    pub target: TargetUnits,
    /// What the run used when evaluated: what the tx declares and pays for.
    pub measured: Units,
    /// The redeemer that makes the burner use it.
    pub knobs: Knobs,
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq)]
pub struct TargetUnits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem: Option<u64>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ManifestTx {
    /// Position in `txs`; also written into the tx's metadata.
    pub index: usize,
    /// IR name, or `fund/<k>` for funding transactions.
    pub name: String,
    pub kind: TxKind,
    pub tx_id: String,
    pub fee: u64,
    pub size: usize,
    /// Inputs spent: IR reference and the UTxO it resolved to.
    pub inputs: Vec<ResolvedInput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reference_inputs: Vec<ResolvedInput>,
    /// Lovelace of each output, in output order.
    pub outputs: Vec<u64>,
    /// Funding txs: the IR source each output holds, by output index (the
    /// last output, change, is not a source).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Conflict sets (contested outputs) this tx spends from.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conflict_sets: Vec<String>,
    /// Conflict sets among its ancestors: it is invalid if an ancestor loses.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub orphan_on: Vec<String>,
    /// Script txs: the collateral input, as `<tx id>#<index>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collateral: Option<String>,
    /// Script txs: the cost asked for and used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<ManifestCost>,
    pub cbor_hex: String,
}

/// An input as named in the IR and as spent on chain.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ResolvedInput {
    /// IR reference; for funding txs, the offered UTxO or `fund/<k>.<ix>`.
    #[serde(rename = "ref")]
    pub reference: String,
    /// `<tx id>#<index>`.
    pub utxo: String,
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TxKind {
    Funding,
    Experiment,
}

/// One output of a transaction being built.
pub(crate) struct PlannedOutput {
    pub(crate) address: Address,
    /// `None` for the change output.
    pub(crate) lovelace: Option<u64>,
}

/// What [`build_signed`] returns.
/// The wire format of the built transactions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxFormat {
    /// `[body, witnesses, is_valid, auxiliary_data]`, as Pallas builds it.
    Conway,
    /// `[body, witnesses, auxiliary_data]`: Dijkstra drops `is_valid`. The
    /// body and witnesses are the same bytes, so the tx id and signatures
    /// do not change.
    Dijkstra,
}

impl TxFormat {
    pub fn for_era(era: &str) -> TxFormat {
        if era.eq_ignore_ascii_case("dijkstra") {
            TxFormat::Dijkstra
        } else {
            TxFormat::Conway
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            TxFormat::Conway => "conway",
            TxFormat::Dijkstra => "dijkstra",
        }
    }

    /// The `cardano-cli` text envelope type for a signed tx in this format.
    pub fn envelope_type(self) -> &'static str {
        match self {
            TxFormat::Conway => "Tx ConwayEra",
            TxFormat::Dijkstra => "Tx DijkstraEra",
        }
    }
}

/// Re-encode a signed Conway tx as a Dijkstra one by dropping `is_valid`.
pub fn conway_to_dijkstra(conway: &[u8]) -> Result<Vec<u8>> {
    let bad = |what: &str| Error::Materialise(format!("unexpected Conway tx encoding: {what}"));
    if conway.first() != Some(&0x84) {
        return Err(bad("not an array of four"));
    }
    let after_body = cbor_skip(conway, 1).ok_or_else(|| bad("body"))?;
    let after_witnesses = cbor_skip(conway, after_body).ok_or_else(|| bad("witnesses"))?;
    if conway.get(after_witnesses) != Some(&0xf5) {
        return Err(bad("is_valid is not `true`"));
    }
    let mut out = Vec::with_capacity(conway.len() - 1);
    out.push(0x83);
    out.extend_from_slice(&conway[1..after_witnesses]);
    out.extend_from_slice(&conway[after_witnesses + 1..]);
    Ok(out)
}

/// Offset just past the CBOR item that starts at `at`, or `None` if the
/// item is malformed or runs past the end.
pub fn cbor_skip(bytes: &[u8], at: usize) -> Option<usize> {
    let initial = *bytes.get(at)?;
    let (major, info) = (initial >> 5, initial & 0x1f);
    let mut at = at + 1;
    let value = match info {
        0..=23 => u64::from(info),
        24..=27 => {
            let width = 1usize << (info - 24);
            let raw = bytes.get(at..at + width)?;
            at += width;
            raw.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b))
        }
        31 if (2..=5).contains(&major) => {
            // Indefinite length: items (or chunks) until the break byte.
            while *bytes.get(at)? != 0xff {
                at = cbor_skip(bytes, at)?;
            }
            return Some(at + 1);
        }
        _ => return None,
    };
    match major {
        0 | 1 | 7 => Some(at),
        2 | 3 => at
            .checked_add(usize::try_from(value).ok()?)
            .filter(|end| *end <= bytes.len()),
        4 | 5 => {
            let items = if major == 5 {
                value.checked_mul(2)?
            } else {
                value
            };
            for _ in 0..items {
                at = cbor_skip(bytes, at)?;
            }
            Some(at)
        }
        6 => cbor_skip(bytes, at),
        _ => None,
    }
}

pub(crate) struct Built {
    pub(crate) tx_id: [u8; 32],
    pub(crate) bytes: Vec<u8>,
    /// The same tx in the Conway format, which the script evaluator reads.
    pub(crate) conway_bytes: Vec<u8>,
    pub(crate) fee: u64,
    pub(crate) outputs: Vec<u64>,
}

/// The script part of a tx with a script cost.
pub(crate) struct ScriptRun<'a> {
    /// The burner-locked input; it is also in the tx's inputs.
    pub(crate) fuel: Input,
    pub(crate) collateral: Input,
    pub(crate) knobs: Knobs,
    /// What the run uses: declared in the redeemer and paid for.
    pub(crate) units: Units,
    pub(crate) cost_model: &'a [i64],
    /// Lovelace for `units` at the execution prices.
    pub(crate) script_fee: u64,
}

/// What script txs need from the context, read once.
struct ScriptSetup<'a> {
    calibration: Calibration,
    cost_model: &'a [i64],
    protocol_major: u16,
    limit: crate::context::ExecutionUnits,
    prices: crate::context::ExecutionPrices,
    address: Address,
}

/// Build and sign every transaction of `ir` against `context`.
pub fn materialise(
    experiment: &str,
    ir: &Ir,
    seed: u64,
    context: &ChainContext,
) -> Result<Manifest> {
    let derived = derive::derive(ir)?;
    let pp = &context.pparams;
    let format = TxFormat::for_era(&context.era);
    let network = match context.network {
        NetworkName::Testnet => Network::Testnet,
        NetworkName::Mainnet => Network::Mainnet,
    };

    let funding_key = SecretKey::from(context.funding_key.0);
    let funding_address = key_address(&funding_key, network);
    // Intermediate funding txs are spent by the next one, which only the
    // funding key signs, so only the last funding tx pays `change_address`.
    let final_change = match &context.change_address {
        Some(text) => change_address(text, network)?,
        None => funding_address.clone(),
    };
    let run_key = SecretKey::from(run_key(&context.funding_key, &derived.hash, seed).0);
    let run_address = key_address(&run_key, network);
    let ir_prefix = derived
        .hash
        .strip_prefix("blake2b256:")
        .and_then(|h| h.get(..16))
        .ok_or_else(|| Error::Materialise("unexpected IR hash format".to_string()))?
        .to_string();
    let min_lovelace = min_for(&run_address, pp)?;

    let mut txs: Vec<ManifestTx> = Vec::new();
    // Reference text (`source` or `tx.N`) → spendable input and its value.
    let mut utxos: BTreeMap<String, (Input, u64)> = BTreeMap::new();

    // --- Script setup -----------------------------------------------------
    let script_txs: Vec<&String> = derived
        .topo_order
        .iter()
        .filter(|name| ir.txs.get(*name).is_some_and(|tx| tx.cost.is_some()))
        .collect();
    let setup = if script_txs.is_empty() {
        None
    } else {
        Some(script_setup(pp, network)?)
    };

    // --- Funding ----------------------------------------------------------
    for (name, source) in &ir.sources {
        if source.lovelace < min_lovelace {
            return Err(Error::Materialise(format!(
                "source `{name}` holds {} lovelace, below the minimum {min_lovelace}",
                source.lovelace
            )));
        }
    }
    // IR sources by name, then the collateral and each script tx's fuel.
    let mut sources: Vec<(String, u64, Address)> = ir
        .sources
        .iter()
        .map(|(n, s)| (n.clone(), s.lovelace, run_address.clone()))
        .collect();
    if let Some(setup) = &setup {
        sources.push((
            COLLATERAL.to_string(),
            collateral_lovelace(pp, setup)?.max(min_lovelace),
            run_address.clone(),
        ));
        let fuel = min_for(&setup.address, pp)?;
        for name in &script_txs {
            sources.push((fuel_name(name), fuel, setup.address.clone()));
        }
    }
    let total_sources = sources
        .iter()
        .try_fold(0u64, |acc, (_, l, _)| acc.checked_add(*l))
        .ok_or_else(|| Error::Materialise("source total overflows".to_string()))?;
    let (mut selected, chunk_sizes) = plan_funding(
        &context.offered,
        sources.len(),
        total_sources,
        min_for(&final_change, pp)?,
        pp,
    )?;
    // Spent as a set; sorted so the manifest does not depend on offer order.
    selected.sort_by_key(|u| (u.tx_id, u.index));
    let run_id = run_id(
        &derived.hash,
        seed,
        pp,
        &selected,
        &final_change,
        &funding_address,
        context.magic,
    );
    let mut chunks: Vec<&[(String, u64, Address)]> = Vec::new();
    let mut rest: &[(String, u64, Address)] = &sources;
    for size in &chunk_sizes {
        let (chunk, tail) = rest.split_at((*size).min(rest.len()));
        chunks.push(chunk);
        rest = tail;
    }

    let mut carry: Vec<(Input, u64)> = selected
        .iter()
        .map(|u| (Input::new(Hash::new(u.tx_id), u.index), u.lovelace))
        .collect();
    let mut carry_names: Vec<ResolvedInput> = selected
        .iter()
        .map(|u| ResolvedInput {
            reference: utxo_text(u),
            utxo: utxo_text(u),
        })
        .collect();
    for (k, chunk) in chunks.iter().enumerate() {
        let index = txs.len();
        let mut planned: Vec<PlannedOutput> = chunk
            .iter()
            .map(|(_, lovelace, address)| PlannedOutput {
                address: address.clone(),
                lovelace: Some(*lovelace),
            })
            .collect();
        let change_to = if k + 1 == chunks.len() {
            &final_change
        } else {
            &funding_address
        };
        planned.push(PlannedOutput {
            address: change_to.clone(),
            lovelace: None,
        });
        let built = build_signed(
            &carry,
            &[],
            &planned,
            &metadata(&context.origin, &ir_prefix, &run_id, index, None),
            &funding_key,
            pp,
            format,
        )
        .map_err(|e| in_tx(&format!("fund/{k}"), e))?;
        let tx_hash = Hash::new(built.tx_id);
        for (i, (name, lovelace, _)) in chunk.iter().enumerate() {
            utxos.insert(name.clone(), (Input::new(tx_hash, i as u64), *lovelace));
        }
        let change_index = chunk.len();
        let change = built
            .outputs
            .get(change_index)
            .copied()
            .ok_or_else(|| Error::Materialise("funding change output missing".to_string()))?;
        let name = format!("fund/{k}");
        let mut entry = manifest_tx(
            index,
            &name,
            TxKind::Funding,
            &built,
            carry_names,
            vec![],
            vec![],
        );
        entry.sources = chunk.iter().map(|(name, _, _)| name.clone()).collect();
        txs.push(entry);
        carry = vec![(Input::new(tx_hash, change_index as u64), change)];
        carry_names = vec![ResolvedInput {
            reference: format!("{name}.{change_index}"),
            utxo: format!("{}#{change_index}", hex::encode(built.tx_id)),
        }];
    }

    // --- Experiment -------------------------------------------------------
    let member_of = derive::membership(&derived.conflict_sets);
    for name in &derived.topo_order {
        let tx = ir
            .txs
            .get(name)
            .ok_or_else(|| Error::Graph(format!("missing tx `{name}`")))?;
        let index = txs.len();
        let mut inputs = resolve_all(&utxos, &tx.inputs)?;
        let reference_inputs = resolve_all(&utxos, &tx.reference_inputs)?;
        let refs: Vec<Input> = reference_inputs
            .iter()
            .map(|(input, _)| input.clone())
            .collect();
        let mut planned = Vec::new();
        for output in &tx.outputs {
            let lovelace = match output.lovelace {
                Amount::Fixed(l) => {
                    if l < min_lovelace {
                        return Err(Error::Materialise(format!(
                            "tx `{name}`: output of {l} lovelace is below the minimum {min_lovelace}"
                        )));
                    }
                    Some(l)
                }
                Amount::Keyword(AmountKeyword::Min) => Some(min_lovelace),
                Amount::Keyword(AmountKeyword::Change) => None,
            };
            planned.push(PlannedOutput {
                address: run_address.clone(),
                lovelace,
            });
        }
        let mut input_refs = tx.inputs.clone();
        let run = match (&tx.cost, &setup) {
            (Some(cost), Some(setup)) => {
                let (fuel, fuel_lovelace) = utxos
                    .get(&fuel_name(name))
                    .cloned()
                    .ok_or_else(|| Error::Materialise(format!("no fuel for `{name}`")))?;
                let collateral = utxos
                    .get(COLLATERAL)
                    .map(|(input, _)| input.clone())
                    .ok_or_else(|| Error::Materialise("no collateral output".to_string()))?;
                inputs.push((fuel.clone(), fuel_lovelace));
                input_refs.push(fuel_name(name));
                let (target, knobs, units) = plan_cost(cost, setup).map_err(|e| in_tx(name, e))?;
                Some((
                    *cost,
                    target,
                    ScriptRun {
                        fuel,
                        collateral,
                        knobs,
                        units,
                        cost_model: setup.cost_model,
                        script_fee: script_fee(&setup.prices, units)?,
                    },
                ))
            }
            _ => None,
        };
        let built = build_signed_with(
            &inputs,
            &refs,
            &planned,
            &metadata(&context.origin, &ir_prefix, &run_id, index, Some(tx)),
            &run_key,
            pp,
            format,
            run.as_ref().map(|(_, _, run)| run),
        )
        .map_err(|e| in_tx(name, e))?;
        let mut cost_entry = None;
        let mut collateral_entry = None;
        if let (Some((requested, target, run)), Some(setup)) = (&run, &setup) {
            // Every input the evaluator needs, with the address it sits at.
            let mut resolved_inputs: Vec<(Input, u64, &Address)> = Vec::new();
            for (input, lovelace) in inputs.iter().chain(reference_inputs.iter()) {
                let address = if *input == run.fuel {
                    &setup.address
                } else {
                    &run_address
                };
                resolved_inputs.push((input.clone(), *lovelace, address));
            }
            let collateral_lovelace = utxos.get(COLLATERAL).map(|(_, l)| *l).unwrap_or(0);
            resolved_inputs.push((run.collateral.clone(), collateral_lovelace, &run_address));
            let measured = measure(&built.conway_bytes, &resolved_inputs, setup)
                .map_err(|e| in_tx(name, e))?;
            if measured != run.units {
                return Err(in_tx(
                    name,
                    Error::Materialise(format!(
                        "the burner used {measured:?}, not the {:?} it was built for",
                        run.units
                    )),
                ));
            }
            cost_entry = Some(ManifestCost {
                requested: *requested,
                target: *target,
                measured,
                knobs: run.knobs,
            });
            collateral_entry = Some(format!(
                "{}#{}",
                hex::encode(run.collateral.tx_hash.0),
                run.collateral.txo_index
            ));
        }
        let tx_hash = Hash::new(built.tx_id);
        for (i, lovelace) in built.outputs.iter().enumerate() {
            utxos.insert(
                format!("{name}.{i}"),
                (Input::new(tx_hash, i as u64), *lovelace),
            );
        }
        let mut entry = manifest_tx(
            index,
            name,
            TxKind::Experiment,
            &built,
            resolved(&input_refs, &inputs),
            resolved(&tx.reference_inputs, &reference_inputs),
            tx.labels.clone(),
        );
        entry.collateral = collateral_entry;
        entry.cost = cost_entry;
        entry.conflict_sets = member_of
            .get(name.as_str())
            .map(|sets| sets.iter().cloned().collect())
            .unwrap_or_default();
        entry.orphan_on = derived.orphan_on.get(name).cloned().unwrap_or_default();
        txs.push(entry);
    }

    Ok(Manifest {
        version: 0,
        experiment: experiment.to_string(),
        ir_hash: derived.hash,
        run_id,
        seed,
        era: context.era.clone(),
        tx_format: format.name().to_string(),
        network: context.network,
        network_magic: context.magic,
        pparams: pp.clone(),
        pparams_hash: pparams_hash(pp),
        funding_address: bech32(&funding_address)?,
        change_address: match &context.change_address {
            Some(_) => Some(bech32(&final_change)?),
            None => None,
        },
        run_address: bech32(&run_address)?,
        funding_utxos: selected.iter().map(utxo_text).collect(),
        burner: match &setup {
            Some(setup) => Some(BurnerInfo {
                script_hash: hex::encode(burner::script_hash()),
                address: bech32(&setup.address)?,
                collateral: utxos
                    .get(COLLATERAL)
                    .map(|(input, _)| {
                        format!("{}#{}", hex::encode(input.tx_hash.0), input.txo_index)
                    })
                    .unwrap_or_default(),
                floor: setup.calibration.floor(),
            }),
            None => None,
        },
        txs,
    })
}

/// Name of the collateral output in funding. `:` cannot appear in IR names.
const COLLATERAL: &str = ":collateral";

/// Name of a script tx's fuel output in funding.
fn fuel_name(tx: &str) -> String {
    format!("{tx}:fuel")
}

/// Read what script txs need from the protocol parameters.
fn script_setup(pp: &ProtocolParams, network: Network) -> Result<ScriptSetup<'_>> {
    let missing = |field: &str| {
        Error::Context(format!(
            "txs with a script cost need `{field}` in the protocol parameters \
             (`cardano-cli query protocol-parameters` prints it)"
        ))
    };
    let cost_model = pp
        .cost_models
        .as_ref()
        .and_then(|c| c.plutus_v3.as_deref())
        .ok_or_else(|| missing("costModels.PlutusV3"))?;
    let protocol_major = pp
        .protocol_version
        .map(|v| v.major)
        .ok_or_else(|| missing("protocolVersion"))?;
    let limit = pp
        .max_tx_execution_units
        .ok_or_else(|| missing("maxTxExecutionUnits"))?;
    let prices = pp
        .execution_unit_prices
        .clone()
        .ok_or_else(|| missing("executionUnitPrices"))?;
    pp.collateral_percentage
        .ok_or_else(|| missing("collateralPercentage"))?;
    Ok(ScriptSetup {
        calibration: Calibration::new(cost_model, protocol_major)?,
        cost_model,
        protocol_major,
        limit,
        prices,
        address: Address::Shelley(ShelleyAddress::new(
            network,
            ShelleyPaymentPart::script_hash(Hash::new(burner::script_hash())),
            ShelleyDelegationPart::Null,
        )),
    })
}

/// The cost in units against the per-tx limit, the burner's knobs for it,
/// and what those knobs use.
fn plan_cost(cost: &Cost, setup: &ScriptSetup) -> Result<(TargetUnits, Knobs, Units)> {
    let target = TargetUnits {
        steps: cost.cpu.map(|v| v.units(setup.limit.steps)),
        mem: cost.mem.map(|v| v.units(setup.limit.memory)),
    };
    for (what, asked, limit) in [
        ("CPU steps", target.steps, setup.limit.steps),
        ("memory units", target.mem, setup.limit.memory),
    ] {
        if let Some(asked) = asked {
            if asked > limit {
                return Err(Error::Materialise(format!(
                    "script cost of {asked} {what} is above the per-tx limit of {limit}"
                )));
            }
        }
    }
    let knobs = setup
        .calibration
        .solve(target.steps, target.mem)
        .map_err(|e| match e {
            Error::Materialise(message) => Error::Materialise(format!("script cost: {message}")),
            other => other,
        })?;
    let units = setup.calibration.predict(knobs);
    if units.steps > setup.limit.steps || units.mem > setup.limit.memory {
        return Err(Error::Materialise(format!(
            "script cost: the burner needs {units:?}, above the per-tx limit"
        )));
    }
    Ok((target, knobs, units))
}

/// `ceil(price_memory * mem + price_steps * steps)`, as the ledger computes it.
fn script_fee(prices: &crate::context::ExecutionPrices, units: Units) -> Result<u64> {
    let (m, s) = (prices.price_memory, prices.price_steps);
    let numerator = u128::from(m.numerator) * u128::from(units.mem) * u128::from(s.denominator)
        + u128::from(s.numerator) * u128::from(units.steps) * u128::from(m.denominator);
    let denominator = u128::from(m.denominator) * u128::from(s.denominator);
    u64::try_from(numerator.div_ceil(denominator))
        .map_err(|_| Error::Materialise("script fee overflows".to_string()))
}

/// Enough collateral for any tx: the fee of a maximum-size tx using the
/// whole per-tx execution limit, times the collateral percentage.
fn collateral_lovelace(pp: &ProtocolParams, setup: &ScriptSetup) -> Result<u64> {
    let percentage = pp.collateral_percentage.unwrap_or(150);
    let most = max_fee(pp)?
        .checked_add(script_fee(
            &setup.prices,
            Units {
                steps: setup.limit.steps,
                mem: setup.limit.memory,
            },
        )?)
        .ok_or_else(|| Error::Materialise("collateral overflows".to_string()))?;
    let needed = u128::from(most) * u128::from(percentage);
    u64::try_from(needed.div_ceil(100))
        .map_err(|_| Error::Materialise("collateral overflows".to_string()))
}

/// Evaluate the tx's script run with the Plutus evaluator and return what it used.
fn measure(conway: &[u8], inputs: &[(Input, u64, &Address)], setup: &ScriptSetup) -> Result<Units> {
    let utxos: Vec<(Vec<u8>, Vec<u8>)> = inputs
        .iter()
        .map(|(input, lovelace, address)| {
            let mut key = vec![0x82, 0x58, 0x20];
            key.extend_from_slice(&input.tx_hash.0);
            burner::cbor_uint(&mut key, 0, input.txo_index);
            let address = address.to_vec();
            let mut value = vec![0xa2, 0x00];
            burner::cbor_uint(&mut value, 2, address.len() as u64);
            value.extend_from_slice(&address);
            value.push(0x01);
            burner::cbor_uint(&mut value, 0, *lovelace);
            (key, value)
        })
        .collect();
    // `{2: [costs]}`: the Plutus V3 cost model.
    let mut cost_models = vec![0xa1, 0x02];
    burner::cbor_uint(&mut cost_models, 4, setup.cost_model.len() as u64);
    for cost in setup.cost_model {
        if *cost >= 0 {
            burner::cbor_uint(&mut cost_models, 0, *cost as u64);
        } else {
            burner::cbor_uint(&mut cost_models, 1, (-1 - *cost) as u64);
        }
    }
    let results = uplc::tx::eval_phase_two_raw_with_protocol(
        conway,
        &utxos,
        Some(&cost_models),
        (setup.limit.steps, setup.limit.memory),
        (0, 0, 1000),
        setup.protocol_major,
        false,
        |_| (),
    )
    .map_err(|e| Error::Materialise(format!("script evaluation: {e}")))?;
    match results.as_slice() {
        [(_, result)] => {
            result
                .result()
                .map_err(|e| Error::Materialise(format!("the burner failed: {e}")))?;
            let used = result.cost();
            Ok(Units {
                steps: u64::try_from(used.cpu).unwrap_or(0),
                mem: u64::try_from(used.mem).unwrap_or(0),
            })
        }
        other => Err(Error::Materialise(format!(
            "expected one script run, evaluated {}",
            other.len()
        ))),
    }
}

/// The run key that owns every source and experiment output: blake2b256 over
/// a tag, the funding key, the IR hash and the seed. `txflow run-key` writes
/// it out so leftover funds can be spent with other tools.
pub fn run_key(funding_key: &SecretKeyBytes, ir_hash: &str, seed: u64) -> SecretKeyBytes {
    let mut hasher = Hasher::<256>::new();
    hasher.input(b"txflow/run-key/v0");
    hasher.input(&funding_key.0);
    hasher.input(ir_hash.as_bytes());
    hasher.input(&seed.to_be_bytes());
    SecretKeyBytes(*hasher.finalize())
}

/// Name the transaction a materialise error happened in.
fn in_tx(name: &str, error: Error) -> Error {
    match error {
        Error::Materialise(message) => Error::Materialise(format!("tx `{name}`: {message}")),
        other => other,
    }
}

/// First 8 bytes of blake2b256 over the IR hash, the seed, the protocol
/// parameters used, the funding UTxOs spent (with their lovelace), the change
/// and funding addresses and the network magic: the same inputs give the same id.
/// `funding` must already be sorted, so offer order does not matter.
fn run_id(
    ir_hash: &str,
    seed: u64,
    pp: &ProtocolParams,
    funding: &[&OfferedUtxo],
    change_address: &Address,
    funding_address: &Address,
    magic: u32,
) -> String {
    let mut hasher = Hasher::<256>::new();
    hasher.input(b"txflow/run-id/v0");
    hasher.input(ir_hash.as_bytes());
    hasher.input(&seed.to_be_bytes());
    hasher.input(pparams_hash(pp).as_bytes());
    for utxo in funding {
        hasher.input(&utxo.tx_id);
        hasher.input(&utxo.index.to_be_bytes());
        hasher.input(&utxo.lovelace.to_be_bytes());
    }
    hasher.input(&change_address.to_vec());
    hasher.input(&funding_address.to_vec());
    hasher.input(&magic.to_be_bytes());
    let digest = hasher.finalize();
    hex::encode(digest.as_ref().iter().take(8).copied().collect::<Vec<u8>>())
}

/// Enterprise address (payment key only) of `key`.
pub(crate) fn key_address(key: &SecretKey, network: Network) -> Address {
    let key_hash = Hasher::<224>::hash(key.public_key().as_ref());
    Address::Shelley(ShelleyAddress::new(
        network,
        ShelleyPaymentPart::key_hash(key_hash),
        ShelleyDelegationPart::Null,
    ))
}

pub(crate) fn bech32(address: &Address) -> Result<String> {
    address
        .to_bech32()
        .map_err(|e| Error::Materialise(format!("address encoding: {e}")))
}

fn utxo_text(utxo: &&OfferedUtxo) -> String {
    format!("{}#{}", hex::encode(utxo.tx_id), utxo.index)
}

/// Choose the offered UTxOs to spend and how many sources each funding tx
/// creates, so that every funding tx stays under `max_tx_size`. The first
/// funding tx spends all selected UTxOs, so it has room for fewer sources.
fn plan_funding<'a>(
    offered: &'a [OfferedUtxo],
    sources: usize,
    total_sources: u64,
    min_change: u64,
    pp: &ProtocolParams,
) -> Result<(Vec<&'a OfferedUtxo>, Vec<usize>)> {
    if sources == 0 {
        return Ok((Vec::new(), Vec::new()));
    }
    let rest_capacity = funding_capacity(1, pp);
    if rest_capacity == 0 {
        return Err(Error::Materialise(format!(
            "max_tx_size {} is too small for a funding transaction",
            pp.max_tx_size
        )));
    }
    let mut reserved_txs: usize = 1;
    for _ in 0..8 {
        let reserve = max_fee(pp)?
            .checked_mul(reserved_txs as u64)
            .ok_or_else(|| Error::Materialise("fee reserve overflows".to_string()))?;
        let needed = total_sources
            .checked_add(reserve)
            .and_then(|n| n.checked_add(min_change))
            .ok_or_else(|| Error::Materialise("funding total overflows".to_string()))?;
        let selected = select_offered(offered, needed)?;
        let first_capacity = funding_capacity(selected.len(), pp);
        if first_capacity == 0 {
            return Err(Error::Materialise(format!(
                "the {} offered UTxOs needed do not fit in one funding transaction; \
                 offer fewer, larger UTxOs",
                selected.len()
            )));
        }
        let mut sizes = vec![first_capacity.min(sources)];
        let mut left = sources.saturating_sub(first_capacity);
        while left > 0 {
            sizes.push(rest_capacity.min(left));
            left = left.saturating_sub(rest_capacity);
        }
        if sizes.len() <= reserved_txs {
            return Ok((selected, sizes));
        }
        reserved_txs = sizes.len();
    }
    Err(Error::Materialise(
        "funding plan did not converge".to_string(),
    ))
}

/// How many sources a funding tx with `inputs` inputs can create.
fn funding_capacity(inputs: usize, pp: &ProtocolParams) -> usize {
    let fixed = TX_OVERHEAD_BOUND
        .saturating_add(CHANGE_SIZE_BOUND)
        .saturating_add(INPUT_SIZE_BOUND.saturating_mul(inputs as u64));
    let room = pp.max_tx_size.saturating_sub(fixed) / OUTPUT_SIZE_BOUND;
    usize::try_from(room)
        .unwrap_or(usize::MAX)
        .min(SOURCES_PER_FUNDING_TX)
}

/// The smallest prefix of `offered` whose value covers `needed`.
fn select_offered(offered: &[OfferedUtxo], needed: u64) -> Result<Vec<&OfferedUtxo>> {
    let mut selected = Vec::new();
    let mut total: u64 = 0;
    for utxo in offered {
        if total >= needed {
            break;
        }
        total = total.saturating_add(utxo.lovelace);
        selected.push(utxo);
    }
    if total < needed {
        return Err(Error::Materialise(format!(
            "offered UTxOs hold {total} lovelace; the experiment needs {needed} \
             (sources, a fee reserve per funding tx, and change)"
        )));
    }
    Ok(selected)
}

fn resolved(refs: &[String], inputs: &[(Input, u64)]) -> Vec<ResolvedInput> {
    refs.iter()
        .zip(inputs)
        .map(|(r, (input, _))| ResolvedInput {
            reference: r.clone(),
            utxo: format!("{}#{}", hex::encode(input.tx_hash.0), input.txo_index),
        })
        .collect()
}

fn resolve_all(
    utxos: &BTreeMap<String, (Input, u64)>,
    refs: &[String],
) -> Result<Vec<(Input, u64)>> {
    refs.iter()
        .map(|r| {
            // Validated by derive; this only fails on an internal ordering bug.
            utxos
                .get(r)
                .cloned()
                .ok_or_else(|| Error::Materialise(format!("`{r}` is not built yet")))
        })
        .collect()
}

/// `(160 + output size) * coinsPerUTxOByte` for an ADA-only output to
/// `address`, sized with the largest coin encoding so it is an upper bound.
fn min_for(address: &Address, pp: &ProtocolParams) -> Result<u64> {
    // Map header, two keys, the address's bytes header (2) and bytes, a 9-byte coin.
    let size = 1 + 1 + 2 + address.to_vec().len() as u64 + 1 + 9;
    (OUTPUT_OVERHEAD + size)
        .checked_mul(pp.coins_per_utxo_byte)
        .ok_or_else(|| Error::Materialise("min UTxO overflows".to_string()))
}

/// Parse `change_address`: a Shelley payment address on `network`.
pub(crate) fn change_address(text: &str, network: Network) -> Result<Address> {
    let address =
        Address::from_bech32(text).map_err(|e| Error::Context(format!("change_address: {e}")))?;
    match &address {
        Address::Shelley(shelley)
            if shelley.network() == network
                && matches!(shelley.payment(), ShelleyPaymentPart::Key(_)) =>
        {
            Ok(address)
        }
        Address::Shelley(shelley) if matches!(shelley.payment(), ShelleyPaymentPart::Script(_)) => {
            Err(Error::Context(format!(
                "change_address `{text}` is a script address; use a key address"
            )))
        }
        Address::Shelley(_) => Err(Error::Context(format!(
            "change_address `{text}` is for another network"
        ))),
        _ => Err(Error::Context(format!(
            "change_address `{text}` is not a Shelley payment address"
        ))),
    }
}

/// `blake2b256:<hex>` over the protocol parameters used.
fn pparams_hash(pp: &ProtocolParams) -> String {
    let mut hasher = Hasher::<256>::new();
    for value in [
        pp.min_fee_a,
        pp.min_fee_b,
        pp.max_tx_size,
        pp.coins_per_utxo_byte,
    ] {
        hasher.input(&value.to_be_bytes());
    }
    // Script parameters only when present, so a context without them hashes
    // as it did before they were read.
    let script = (
        &pp.collateral_percentage,
        &pp.execution_unit_prices,
        &pp.max_tx_execution_units,
        &pp.cost_models,
        &pp.protocol_version,
    );
    if script != (&None, &None, &None, &None, &None) {
        let json = serde_json::to_vec(&script).unwrap_or_default();
        hasher.input(&json);
    }
    format!("blake2b256:{}", hex::encode(hasher.finalize()))
}

/// Fee of a transaction of the maximum size.
pub(crate) fn max_fee(pp: &ProtocolParams) -> Result<u64> {
    pp.min_fee_a
        .checked_mul(pp.max_tx_size)
        .and_then(|f| f.checked_add(pp.min_fee_b))
        .ok_or_else(|| Error::Materialise("fee overflows".to_string()))
}

/// Build, sign and balance: the change output takes inputs minus outputs minus
/// fee, and the fee is raised until it covers `a * size + b` of the signed tx.
pub(crate) fn build_signed(
    inputs: &[(Input, u64)],
    reference_inputs: &[Input],
    planned: &[PlannedOutput],
    metadata: &[u8],
    key: &SecretKey,
    pp: &ProtocolParams,
    format: TxFormat,
) -> Result<Built> {
    build_signed_with(
        inputs,
        reference_inputs,
        planned,
        metadata,
        key,
        pp,
        format,
        None,
    )
}

/// [`build_signed`], with a script run when `script` is given: the burner as
/// a witness, the redeemer on the fuel input, the collateral, the script data
/// hash, and the script's execution fee on top of the size fee.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_signed_with(
    inputs: &[(Input, u64)],
    reference_inputs: &[Input],
    planned: &[PlannedOutput],
    metadata: &[u8],
    key: &SecretKey,
    pp: &ProtocolParams,
    format: TxFormat,
    script: Option<&ScriptRun>,
) -> Result<Built> {
    let min_change = match planned.iter().find(|o| o.lovelace.is_none()) {
        Some(change) => min_for(&change.address, pp)?,
        None => return Err(Error::Materialise("no change output".to_string())),
    };
    let total_in = inputs
        .iter()
        .try_fold(0u64, |acc, (_, l)| acc.checked_add(*l))
        .ok_or_else(|| Error::Materialise("input total overflows".to_string()))?;
    let fixed = planned
        .iter()
        .filter_map(|o| o.lovelace)
        .try_fold(0u64, |acc, l| acc.checked_add(l))
        .ok_or_else(|| Error::Materialise("output total overflows".to_string()))?;

    let mut fee: u64 = 0;
    for _ in 0..FEE_ROUNDS {
        let change = total_in
            .checked_sub(fixed)
            .and_then(|c| c.checked_sub(fee))
            .ok_or_else(|| {
                Error::Materialise(format!(
                    "inputs hold {total_in} lovelace, outputs need {fixed} plus fee {fee}"
                ))
            })?;
        if change < min_change {
            return Err(Error::Materialise(format!(
                "change of {change} lovelace is below the minimum {min_change} \
                 (inputs {total_in}, outputs {fixed}, fee {fee})"
            )));
        }
        let mut staging = StagingTransaction::new();
        for (input, _) in inputs {
            staging = staging.input(input.clone());
        }
        // Pallas sorts spent inputs but not reference inputs; sort both so
        // the tx id does not depend on the order the IR lists them in.
        let mut sorted_refs: Vec<&Input> = reference_inputs.iter().collect();
        sorted_refs.sort_by_key(|i| (i.tx_hash.0, i.txo_index));
        for input in sorted_refs {
            staging = staging.reference_input(input.clone());
        }
        let mut amounts = Vec::new();
        for output in planned {
            let lovelace = output.lovelace.unwrap_or(change);
            amounts.push(lovelace);
            staging = staging.output(Output::new(output.address.clone(), lovelace));
        }
        let mut staging = staging.fee(fee).add_auxiliary_data(metadata.to_vec());
        if let Some(run) = script {
            staging = staging
                .collateral_input(run.collateral.clone())
                .script(ScriptKind::PlutusV3, burner::script())
                .add_spend_redeemer(
                    run.fuel.clone(),
                    run.knobs.redeemer(),
                    Some(ExUnits {
                        mem: run.units.mem,
                        steps: run.units.steps,
                    }),
                )
                .add_language(ScriptKind::PlutusV3, run.cost_model.to_vec());
        }
        // Pallas drops metadata it cannot decode without an error, and the
        // metadata is what keeps the members of a conflict set distinct.
        if staging.auxiliary_data.is_none() {
            return Err(Error::Materialise(
                "the attribution metadata does not decode".to_string(),
            ));
        }
        let mut unsigned = staging
            .build_conway_raw()
            .map_err(|e| Error::Materialise(format!("build: {e}")))?;
        if let Some(run) = script {
            let (tx, body_hash) = redeemers_as_map(&unsigned.tx_bytes.0, run.cost_model)?;
            unsigned.tx_bytes = Bytes(tx);
            unsigned.tx_hash = Bytes32(body_hash);
        }
        let signed = unsigned
            .sign(key)
            .map_err(|e| Error::Materialise(format!("build: {e}")))?;
        let conway_bytes = signed.tx_bytes.0;
        let bytes = match format {
            TxFormat::Conway => conway_bytes.clone(),
            TxFormat::Dijkstra => conway_to_dijkstra(&conway_bytes)?,
        };
        let size = bytes.len();
        let required = pp
            .min_fee_a
            .checked_mul(size as u64)
            .and_then(|f| f.checked_add(pp.min_fee_b))
            .and_then(|f| f.checked_add(script.map_or(0, |run| run.script_fee)))
            .ok_or_else(|| Error::Materialise("fee overflows".to_string()))?;
        if fee >= required {
            if size as u64 > pp.max_tx_size {
                return Err(Error::Materialise(format!(
                    "{size} bytes exceeds the maximum transaction size {}",
                    pp.max_tx_size
                )));
            }
            return Ok(Built {
                tx_id: signed.tx_hash.0,
                bytes,
                conway_bytes,
                fee,
                outputs: amounts,
            });
        }
        fee = required;
    }
    Err(Error::Materialise("fee did not converge".to_string()))
}

/// Pallas writes redeemers as the array `[[tag, index, data, ex_units]]`,
/// which Dijkstra no longer accepts. Rewrite them as the map
/// `{[tag, index] => [data, ex_units]}`, which Conway accepts too, and the
/// script data hash over them: `blake2b-256(redeemers ‖ language views)`, with
/// no datums. Returns the unsigned tx and its new body hash.
fn redeemers_as_map(tx: &[u8], cost_model: &[i64]) -> Result<(Vec<u8>, [u8; 32])> {
    let bad = || Error::Materialise("unexpected encoding of a built script tx".to_string());
    if tx.first() != Some(&0x84) {
        return Err(bad());
    }
    let body_end = cbor_skip(tx, 1).ok_or_else(bad)?;
    let witnesses_end = cbor_skip(tx, body_end).ok_or_else(bad)?;

    // The witness set with the redeemers re-encoded.
    let (major, entries, mut at) = cbor_head(tx, body_end).ok_or_else(bad)?;
    if major != 5 {
        return Err(bad());
    }
    let mut witnesses = tx.get(body_end..at).ok_or_else(bad)?.to_vec();
    let mut redeemers = None;
    for _ in 0..entries {
        let (_, key, _) = cbor_head(tx, at).ok_or_else(bad)?;
        let key_end = cbor_skip(tx, at).ok_or_else(bad)?;
        let value_end = cbor_skip(tx, key_end).ok_or_else(bad)?;
        witnesses.extend_from_slice(tx.get(at..key_end).ok_or_else(bad)?);
        let value = tx.get(key_end..value_end).ok_or_else(bad)?;
        if key == 5 {
            let map = redeemer_map(value).ok_or_else(bad)?;
            witnesses.extend_from_slice(&map);
            redeemers = Some(map);
        } else {
            witnesses.extend_from_slice(value);
        }
        at = value_end;
    }
    if at != witnesses_end {
        return Err(bad());
    }
    let redeemers = redeemers.ok_or_else(bad)?;

    let mut views = vec![0xa1, 0x02];
    burner::cbor_uint(&mut views, 4, cost_model.len() as u64);
    for cost in cost_model {
        match u64::try_from(*cost) {
            Ok(cost) => burner::cbor_uint(&mut views, 0, cost),
            Err(_) => burner::cbor_uint(&mut views, 1, cost.unsigned_abs() - 1),
        }
    }
    let mut hashed = redeemers;
    hashed.extend_from_slice(&views);
    let data_hash = Hasher::<256>::hash(&hashed);

    // The body with field 11, the script data hash, replaced in place.
    let mut body = tx.get(1..body_end).ok_or_else(bad)?.to_vec();
    let (major, entries, mut at) = cbor_head(&body, 0).ok_or_else(bad)?;
    if major != 5 {
        return Err(bad());
    }
    let mut replaced = false;
    for _ in 0..entries {
        let (_, key, key_end) = cbor_head(&body, at).ok_or_else(bad)?;
        let value_end = cbor_skip(&body, key_end).ok_or_else(bad)?;
        if key == 11 {
            let hash = body.get_mut(key_end..value_end).ok_or_else(bad)?;
            if hash.len() != 34 || hash.get(..2) != Some(&[0x58, 0x20][..]) {
                return Err(bad());
            }
            hash.get_mut(2..)
                .ok_or_else(bad)?
                .copy_from_slice(data_hash.as_ref());
            replaced = true;
        }
        at = value_end;
    }
    if !replaced {
        return Err(bad());
    }

    let mut out = vec![0x84];
    out.extend_from_slice(&body);
    out.extend_from_slice(&witnesses);
    out.extend_from_slice(tx.get(witnesses_end..).ok_or_else(bad)?);
    Ok((out, *Hasher::<256>::hash(&body)))
}

/// `[[tag, index, data, ex_units], ...]` as `{[tag, index] => [data, ex_units]}`.
fn redeemer_map(array: &[u8]) -> Option<Vec<u8>> {
    let (major, count, mut at) = cbor_head(array, 0)?;
    if major != 4 || count == 0 {
        return None;
    }
    let mut map = Vec::new();
    burner::cbor_uint(&mut map, 5, count);
    for _ in 0..count {
        let (major, items, start) = cbor_head(array, at)?;
        if major != 4 || items != 4 {
            return None;
        }
        let index_end = cbor_skip(array, cbor_skip(array, start)?)?;
        let units_end = cbor_skip(array, cbor_skip(array, index_end)?)?;
        map.push(0x82);
        map.extend_from_slice(array.get(start..index_end)?);
        map.push(0x82);
        map.extend_from_slice(array.get(index_end..units_end)?);
        at = units_end;
    }
    (at == array.len()).then_some(map)
}

/// The major type and argument of the definite-length CBOR head at `at`, and
/// the offset just past it.
fn cbor_head(bytes: &[u8], at: usize) -> Option<(u8, u64, usize)> {
    let initial = *bytes.get(at)?;
    let (major, info) = (initial >> 5, initial & 0x1f);
    match info {
        0..=23 => Some((major, u64::from(info), at + 1)),
        24..=27 => {
            let width = 1usize << (info - 24);
            let raw = bytes.get(at + 1..at + 1 + width)?;
            let value = raw.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
            Some((major, value, at + 1 + width))
        }
        _ => None,
    }
}

fn manifest_tx(
    index: usize,
    name: &str,
    kind: TxKind,
    built: &Built,
    inputs: Vec<ResolvedInput>,
    reference_inputs: Vec<ResolvedInput>,
    labels: Vec<String>,
) -> ManifestTx {
    ManifestTx {
        index,
        name: name.to_string(),
        kind,
        tx_id: hex::encode(built.tx_id),
        fee: built.fee,
        size: built.bytes.len(),
        inputs,
        reference_inputs,
        outputs: built.outputs.clone(),
        sources: vec![],
        labels,
        conflict_sets: vec![],
        orphan_on: vec![],
        collateral: None,
        cost: None,
        cbor_hex: hex::encode(&built.bytes),
    }
}

/// Shelley-format auxiliary data:
/// `{674: {"msg": ["txflow/1 <origin>", "spec:<ir>", "run:<run>#<index>"]}}`,
/// plus the tx's own metadata and colour tag from the spec, if any.
/// The run id and index also keep the members of a conflict set distinct.
fn metadata(origin: &str, ir_prefix: &str, run_id: &str, index: usize, tx: Option<&Tx>) -> Vec<u8> {
    let empty = Metadata::new();
    auxiliary_data(
        &[
            &watermark(origin),
            &format!("spec:{ir_prefix}"),
            &format!("run:{run_id}#{index}"),
        ],
        tx.map_or(&empty, |tx| &tx.metadata),
        tx.and_then(|tx| tx.color.as_deref()),
    )
}

/// `{674: {"msg": [parts...]}}`.
pub(crate) fn message(parts: &[&str]) -> Vec<u8> {
    auxiliary_data(parts, &Metadata::new(), None)
}
