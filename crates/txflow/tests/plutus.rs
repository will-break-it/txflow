//! Script cost: materialised txs run the burner and use what the spec asked.

use std::path::{Path, PathBuf};

use pallas_crypto::key::ed25519::{PublicKey, Signature};
use pallas_traverse::MultiEraTx;
use txflow::burner;
use txflow::context::ChainContext;
use txflow::expand::expand;
use txflow::materialise::{cbor_skip, materialise, Manifest, TxKind};
use txflow::spec::ExperimentSpec;

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name)
}

/// The Plutus demo context, as Dijkstra or as Conway (which Pallas decodes).
fn context(era: &str) -> ChainContext {
    let mut ctx =
        ChainContext::load(&example("demo-plutus-context.yaml")).expect("Plutus context loads");
    ctx.era = era.to_string();
    ctx
}

fn materialise_yaml(yaml: &str, ctx: &ChainContext) -> txflow::error::Result<Manifest> {
    let spec = ExperimentSpec::from_yaml(yaml).expect("spec");
    let ir = expand(&spec).expect("ir");
    materialise(&spec.experiment, &ir, spec.seed, ctx)
}

fn heavy_pairs(ctx: &ChainContext) -> Manifest {
    let text = std::fs::read_to_string(example("heavy-conflicting-pairs.yaml")).expect("example");
    materialise_yaml(&text, ctx).expect("materialises")
}

/// One tx `q` with `cost`, spending a source of its own.
fn single(cost: &str) -> String {
    format!(
        "version: 0\nexperiment: s\nseed: 1\nwallets: {{ alice: {{}} }}\n\
         sources: {{ f: {{ owner: alice, lovelace: 5000000 }} }}\n\
         txs:\n  q: {{ inputs: [f], outputs: [{{ to: alice, lovelace: change }}], cost: {cost} }}\n"
    )
}

#[test]
fn every_costed_tx_uses_just_under_its_target() {
    let m = heavy_pairs(&context("conway"));
    let experiment: Vec<_> = m
        .txs
        .iter()
        .filter(|t| t.kind == TxKind::Experiment)
        .collect();
    assert_eq!(experiment.len(), 20);
    for tx in experiment {
        let cost = tx.cost.as_ref().expect("every tx carries the spec's cost");
        // 25% of Musashi's 10_000_000_000 steps.
        assert_eq!(cost.target.steps, Some(2_500_000_000));
        assert_eq!(cost.target.mem, None);
        assert!(cost.measured.steps <= 2_500_000_000, "{}", tx.name);
        assert!(cost.measured.steps > 2_499_000_000, "{}", tx.name);
        assert!(tx.collateral.is_some());
        // The fuel input is listed with the tx's own inputs.
        assert!(tx
            .inputs
            .iter()
            .any(|i| i.reference == format!("{}:fuel", tx.name)));
    }
    let burner_info = m.burner.as_ref().expect("burner in the manifest");
    assert_eq!(burner_info.script_hash, hex::encode(burner::script_hash()));
    assert!(burner_info.address.starts_with("addr_test1w"));
}

#[test]
fn script_txs_carry_the_burner_redeemer_collateral_and_fee() {
    let ctx = context("conway");
    let m = heavy_pairs(&ctx);
    let tx = m.txs.iter().find(|t| t.name == "pairs/a1").expect("a1");
    let cost = tx.cost.as_ref().expect("cost");
    let bytes = hex::decode(&tx.cbor_hex).expect("hex");
    let decoded = MultiEraTx::decode(&bytes).expect("decodes");
    let conway = decoded.as_conway().expect("conway");
    let body = &conway.transaction_body;
    assert!(body.script_data_hash.is_some());
    assert_eq!(body.collateral.as_ref().map(|c| c.len()), Some(1));
    let witnesses = &conway.transaction_witness_set;
    let scripts = witnesses.plutus_v3_script.as_ref().expect("burner witness");
    assert_eq!(scripts.len(), 1);
    assert_eq!(
        scripts.first().map(|s| s.as_ref().to_vec()),
        Some(burner::script())
    );
    // Redeemers as a map, the only form Dijkstra accepts.
    let raw = witnesses.redeemer.as_ref().expect("redeemers").raw_cbor();
    assert_eq!(raw.first(), Some(&0xa1));
    // The signature is over the body as rewritten.
    let vkeys = witnesses.vkeywitness.as_ref().expect("signed");
    for witness in vkeys.iter() {
        let key: [u8; 32] = witness.vkey.as_slice().try_into().expect("key");
        let signature: [u8; 64] = witness.signature.as_slice().try_into().expect("sig");
        assert!(PublicKey::from(key).verify(decoded.hash(), &Signature::from(signature)));
    }
    let redeemers = decoded.redeemers();
    assert_eq!(redeemers.len(), 1);
    let units = redeemers.first().map(|r| r.ex_units()).expect("redeemer");
    assert_eq!(
        (units.steps, units.mem),
        (cost.measured.steps, cost.measured.mem)
    );

    // Fee: size fee plus ceil(0.0577 * mem + 0.0000721 * steps).
    let pp = &ctx.pparams;
    let script_fee = (577 * u128::from(cost.measured.mem) * 1000
        + 721 * u128::from(cost.measured.steps))
    .div_ceil(10_000_000) as u64;
    assert!(tx.fee >= pp.min_fee_a * tx.size as u64 + pp.min_fee_b + script_fee);
    assert_eq!(decoded.fee(), Some(tx.fee));
}

#[test]
fn reruns_are_identical_and_dijkstra_txs_keep_their_ids() {
    let m = heavy_pairs(&context("dijkstra"));
    assert_eq!(m, heavy_pairs(&context("dijkstra")));
    for entry in m.txs.iter().filter(|t| t.cost.is_some()) {
        let bytes = hex::decode(&entry.cbor_hex).expect("hex");
        // [body, witnesses, aux]: put `is_valid` back to decode it as Conway.
        let witnesses_end = cbor_skip(&bytes, cbor_skip(&bytes, 1).expect("body")).expect("wits");
        let mut conway = vec![0x84];
        conway.extend_from_slice(&bytes[1..witnesses_end]);
        conway.push(0xf5);
        conway.extend_from_slice(&bytes[witnesses_end..]);
        let tx = MultiEraTx::decode(&conway).expect("decodes");
        assert_eq!(hex::encode(tx.hash()), entry.tx_id, "{}", entry.name);
        assert_eq!(tx.redeemers().len(), 1, "{}", entry.name);
    }
}

#[test]
fn costs_without_a_spec_cost_change_nothing() {
    let plain = "version: 0\nexperiment: s\nseed: 1\nwallets: { alice: {} }\n\
                 sources: { f: { owner: alice, lovelace: 5000000 } }\n\
                 txs:\n  q: { inputs: [f], outputs: [{ to: alice, lovelace: change }] }\n";
    let m = materialise_yaml(plain, &context("conway")).expect("plain");
    assert!(m.burner.is_none());
    assert!(m
        .txs
        .iter()
        .all(|t| t.cost.is_none() && t.collateral.is_none()));
}

#[test]
fn a_run_without_a_cost_ignores_the_contexts_plutus_parameters() {
    // A real context's pparams file carries the Plutus fields; a run with no
    // script must not depend on them.
    let plain = "version: 0\nexperiment: s\nseed: 1\nwallets: { alice: {} }\n\
                 sources: { f: { owner: alice, lovelace: 5000000 } }\n\
                 txs:\n  q: { inputs: [f], outputs: [{ to: alice, lovelace: change }] }\n";
    let mut without = ChainContext::load(&example("demo-context.yaml")).expect("demo context");
    without.era = "conway".to_string();
    assert_eq!(
        materialise_yaml(plain, &context("conway")).expect("with"),
        materialise_yaml(plain, &without).expect("without")
    );
}

#[test]
fn fuel_pays_its_own_execution_fee() {
    // 1.5 ADA could not pay the ~1.67 ADA execution fee of a full budget;
    // the fuel output carries it.
    let spec = "version: 0\nexperiment: s\nseed: 1\nwallets: { alice: {} }\n\
                sources: { f: { owner: alice, lovelace: 1500000 } }\n\
                txs:\n  q: { inputs: [f], outputs: [{ to: alice, lovelace: change }], \
                cost: { cpu: 100%, mem: 100% } }\n";
    let m = materialise_yaml(spec, &context("conway")).expect("materialises");
    let q = m.txs.iter().find(|t| t.name == "q").expect("q");
    assert!(q.fee > 1_500_000, "{}", q.fee);
    assert!(q.outputs.first().is_some_and(|change| *change >= 1_400_000));
}

#[test]
fn absolute_units_and_memory_targets() {
    let m = materialise_yaml(&single("{ cpu: 1000000000, mem: 2% }"), &context("conway"))
        .expect("materialises");
    let q = m.txs.iter().find(|t| t.name == "q").expect("q");
    let cost = q.cost.as_ref().expect("cost");
    assert_eq!(cost.target.steps, Some(1_000_000_000));
    assert_eq!(cost.target.mem, Some(330_000));
    assert!(cost.measured.steps <= 1_000_000_000);
    assert!(cost.measured.mem <= 330_000);
    assert!(cost.measured.mem > 320_000);
}

#[test]
fn unreachable_or_oversized_costs_are_errors_naming_the_tx() {
    let ctx = context("conway");
    // Memory-heavy beyond what any script reaches.
    let error = materialise_yaml(&single("{ cpu: 1%, mem: 50% }"), &ctx)
        .expect_err("unreachable")
        .to_string();
    assert!(
        error.contains("`q`") && error.contains("script cost"),
        "{error}"
    );
    // Above the per-tx limit.
    let error = materialise_yaml(&single("{ cpu: 20000000000 }"), &ctx)
        .expect_err("too big")
        .to_string();
    assert!(error.contains("per-tx limit"), "{error}");
    // Below the burner's floor.
    let error = materialise_yaml(&single("{ cpu: 1000 }"), &ctx)
        .expect_err("too small")
        .to_string();
    assert!(error.contains("floor"), "{error}");
}

#[test]
fn a_context_without_plutus_parameters_says_what_is_missing() {
    let mut ctx = context("conway");
    ctx.pparams.cost_models = None;
    let error = materialise_yaml(&single("{ cpu: 10% }"), &ctx)
        .expect_err("no cost model")
        .to_string();
    assert!(error.contains("costModels.PlutusV3"), "{error}");
}
