//! Validity windows in built transactions: the slots they become, the
//! context they need, and funding that does not depend on them.

use std::path::{Path, PathBuf};

use pallas_traverse::MultiEraTx;
use txflow::context::{ChainContext, RunSlots};
use txflow::expand::expand;
use txflow::materialise::{materialise, Manifest, ManifestTx, TxKind};
use txflow::spec::ExperimentSpec;

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name)
}

/// The demo context in Conway encoding, which Pallas decodes back.
fn context(slots: RunSlots) -> ChainContext {
    let mut ctx = ChainContext::load(&example("demo-context.yaml")).expect("demo context");
    ctx.era = "conway".to_string();
    ctx.slots = slots;
    ctx
}

fn slots(start: u64, tip: u64) -> RunSlots {
    RunSlots {
        start: Some(start),
        tip: Some(tip),
    }
}

fn build(yaml: &str, ctx: &ChainContext) -> txflow::error::Result<Manifest> {
    let spec = ExperimentSpec::from_yaml(yaml).expect("spec");
    let ir = expand(&spec).expect("ir");
    materialise(&spec.experiment, &ir, spec.seed, ctx)
}

fn expiring() -> String {
    std::fs::read_to_string(example("expiring-chain.yaml")).expect("example")
}

fn tx<'a>(m: &'a Manifest, name: &str) -> &'a ManifestTx {
    m.txs
        .iter()
        .find(|t| t.name == name)
        .expect("tx in manifest")
}

/// `(validity_interval_start, ttl)` of a built tx's body.
fn interval(t: &ManifestTx) -> (Option<u64>, Option<u64>) {
    let bytes = hex::decode(&t.cbor_hex).expect("hex");
    let decoded = MultiEraTx::decode(&bytes).expect("decodes");
    (decoded.validity_start(), decoded.ttl())
}

#[test]
fn windows_become_the_slots_after_their_anchor() {
    let m = build(&expiring(), &context(slots(5_000, 4_990))).expect("materialises");
    assert_eq!(m.slots, Some(slots(5_000, 4_990)));
    // `doomed`: only the first tx expires, 3 slots after the tip.
    assert_eq!(interval(tx(&m, "doomed/t1")), (None, Some(4_993)));
    assert_eq!(interval(tx(&m, "doomed/t2")), (None, None));
    // `ladder`: 25 slots more each generation.
    assert_eq!(interval(tx(&m, "ladder/t1")), (None, Some(5_015)));
    assert_eq!(interval(tx(&m, "ladder/t3")), (None, Some(5_065)));
    assert_eq!(interval(tx(&m, "control/t1")), (None, None));
    let doomed = tx(&m, "doomed/t1").validity.expect("window recorded");
    assert_eq!(doomed.window.invalid_hereafter, Some(4_993));
    assert_eq!(doomed.requested.until, Some(3));

    // Both edges, counted from the start.
    let spec = "version: 0\nexperiment: w\nseed: 1\nwallets: { alice: {} }\n\
        patterns:\n  - { id: c, use: chain, with: { length: 1 }, validity: { from: 2, until: 9 } }\n";
    let m = build(spec, &context(slots(5_000, 4_990))).expect("materialises");
    assert_eq!(interval(tx(&m, "c/t1")), (Some(5_002), Some(5_009)));
}

#[test]
fn funding_and_the_run_id_do_not_depend_on_the_slots() {
    let early = build(&expiring(), &context(slots(5_000, 4_990))).expect("early");
    let late = build(&expiring(), &context(slots(9_000, 8_985))).expect("late");
    assert_eq!(early.run_id, late.run_id);
    for (a, b) in early.txs.iter().zip(&late.txs) {
        assert_eq!(a.name, b.name);
        let windowed = a.validity.is_some();
        match a.kind {
            // Funding is what a run sends first, before it knows when it starts.
            TxKind::Funding => assert_eq!(a.cbor_hex, b.cbor_hex, "{}", a.name),
            TxKind::Experiment if windowed => assert_ne!(a.tx_id, b.tx_id, "{}", a.name),
            // Unwindowed children of windowed txs spend new tx ids.
            TxKind::Experiment => {}
        }
    }
    assert_eq!(
        tx(&early, "control/t5").tx_id,
        tx(&late, "control/t5").tx_id
    );
    assert_eq!(
        early,
        build(&expiring(), &context(slots(5_000, 4_990))).expect("again")
    );
}

#[test]
fn a_window_needs_the_slot_it_counts_from() {
    let only_start = RunSlots {
        start: Some(5_000),
        tip: None,
    };
    let error = build(&expiring(), &context(only_start))
        .expect_err("needs the tip")
        .to_string();
    assert!(
        error.contains("doomed/t1") || error.contains("ladder/t1"),
        "{error}"
    );
    assert!(error.contains("`slots.tip`"), "{error}");
    let error = build(&expiring(), &context(slots(5_000, 5_001)))
        .expect_err("tip after start")
        .to_string();
    assert!(error.contains("after `slots.start`"), "{error}");
}

#[test]
fn a_spec_without_windows_ignores_the_slots() {
    let text = std::fs::read_to_string(example("conflicting-chains.yaml")).expect("example");
    let with = build(&text, &context(slots(5_000, 4_990))).expect("with slots");
    let without = build(&text, &context(RunSlots::default())).expect("without slots");
    assert_eq!(with, without);
    // Nor are they checked: a tip after the start matters only to a window.
    let odd = build(&text, &context(slots(5_000, 5_001))).expect("odd slots");
    assert_eq!(with, odd);
    assert_eq!(with.slots, None);
}

#[test]
fn a_release_tip_window_is_not_fixed_offline() {
    let text = "version: 0\nexperiment: r\nseed: 1\nwallets: { alice: {} }\n\
        sources: { s: { owner: alice, lovelace: 9000000 } }\ntxs:\n  \
        t: { inputs: [s], outputs: [{ to: alice, lovelace: change }], \
        validity: { until: 2, anchor: release-tip } }\n";
    let error = build(text, &context(slots(5_000, 4_990)))
        .expect_err("refused")
        .to_string();
    assert!(error.contains("`anchor: release-tip`"), "{error}");
    assert!(error.contains("`anchor: tip`"), "{error}");
}
