//! Reference scripts on outputs: the script each output carries, the fee a
//! tx pays to spend or read them, and sweeping outputs that carry them.

use std::path::{Path, PathBuf};

use pallas_primitives::conway::ScriptRef;
use pallas_traverse::{MultiEraOutput, MultiEraTx};
use txflow::context::{ChainContext, NetworkName, OfferedUtxo, ProtocolParams, Rational};
use txflow::expand::expand;
use txflow::funds::{sweep, HeldUtxo, RunTag};
use txflow::materialise::{materialise, Manifest, ManifestTx, TxFormat};
use txflow::spec::ExperimentSpec;

const HEAD: &str = "version: 0\nexperiment: r\nseed: 1\nwallets: { alice: {} }\n";

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name)
}

/// The demo context in Conway encoding, which Pallas decodes back.
fn context(name: &str) -> ChainContext {
    let mut ctx = ChainContext::load(&example(name)).expect("context");
    ctx.era = "conway".to_string();
    ctx
}

fn dijkstra() -> ChainContext {
    let mut ctx = context("demo-context.yaml");
    ctx.era = "dijkstra".to_string();
    ctx
}

fn build(yaml: &str, ctx: &ChainContext) -> txflow::error::Result<Manifest> {
    let spec = ExperimentSpec::from_yaml(yaml).expect("spec");
    let ir = expand(&spec).expect("ir");
    materialise(&spec.experiment, &ir, spec.seed, ctx)
}

fn tx<'a>(m: &'a Manifest, name: &str) -> &'a ManifestTx {
    m.txs.iter().find(|t| t.name == name).expect("tx")
}

/// The size of the Plutus V3 reference script on each output of `t`.
fn scripts(t: &ManifestTx) -> Vec<Option<usize>> {
    let bytes = hex::decode(&t.cbor_hex).expect("hex");
    let decoded = MultiEraTx::decode(&bytes).expect("decodes");
    decoded
        .outputs()
        .iter()
        .map(|o: &MultiEraOutput| match o.script_ref() {
            Some(ScriptRef::PlutusV3Script(script)) => Some(script.as_ref().len()),
            Some(_) => panic!("not a Plutus V3 script"),
            None => None,
        })
        .collect()
}

/// One tx makes an output with a 12 000-byte script; `r` reads it and `s`
/// spends it.
fn spec() -> String {
    format!(
        "{HEAD}sources: {{ a: {{ owner: alice, lovelace: 100000000 }}, \
         b: {{ owner: alice, lovelace: 10000000 }} }}\ntxs:\n  \
         p: {{ inputs: [a], outputs: [{{ to: alice, lovelace: min, reference_script: {{ bytes: 12000 }} }}, \
         {{ to: alice, lovelace: change }}] }}\n  \
         r: {{ inputs: [b], reference_inputs: [p.0], outputs: [{{ to: alice, lovelace: change }}] }}\n  \
         s: {{ inputs: [p.0, p.1], outputs: [{{ to: alice, lovelace: change }}] }}\n"
    )
}

#[test]
fn an_output_carries_a_script_of_the_size_asked_for() {
    let m = build(&spec(), &context("demo-context.yaml")).expect("materialises");
    let p = tx(&m, "p");
    assert_eq!(scripts(p), vec![Some(12_000), None]);
    assert_eq!(p.reference_scripts, vec![12_000, 0]);
    assert!(p.reference_scripts_read.is_none());
    // The min-UTxO covers the script: (160 + output size) * 4310.
    assert!(p.outputs[0] > (160 + 12_000) * 4_310, "{}", p.outputs[0]);
    // The price is recorded, as a tx reads a script.
    assert_eq!(
        m.pparams.min_fee_ref_script_cost_per_byte,
        Some(Rational {
            numerator: 15,
            denominator: 1
        })
    );
}

#[test]
fn reading_or_spending_a_script_pays_its_fee() {
    let m = build(&spec(), &context("demo-context.yaml")).expect("materialises");
    for name in ["r", "s"] {
        let t = tx(&m, name);
        let read = t.reference_scripts_read.expect("reads a script");
        assert_eq!(read.bytes, 12_000);
        assert_eq!(read.fee, 15 * 12_000);
        // Size fee plus the reference-script fee, and no more than one
        // fee round's slack.
        let size_fee = 44 * t.size as u64 + 155_381;
        assert!(t.fee >= size_fee + read.fee, "{name}: {}", t.fee);
        assert!(t.fee <= size_fee + read.fee + 44 * 9, "{name}: {}", t.fee);
        assert!(t.reference_scripts.is_empty());
    }
}

#[test]
fn a_spec_without_scripts_records_no_price() {
    let yaml = format!(
        "{HEAD}sources: {{ a: {{ owner: alice, lovelace: 10000000 }} }}\ntxs:\n  \
         t: {{ inputs: [a], outputs: [{{ to: alice, lovelace: change }}] }}\n"
    );
    let m = build(&yaml, &context("demo-context.yaml")).expect("materialises");
    assert_eq!(m.pparams.min_fee_ref_script_cost_per_byte, None);
    let mut without = context("demo-context.yaml");
    without.pparams.min_fee_ref_script_cost_per_byte = None;
    let other = build(&yaml, &without).expect("materialises");
    assert_eq!(m.run_id, other.run_id);
    assert_eq!(m.pparams_hash, other.pparams_hash);
}

#[test]
fn an_unread_script_needs_and_records_the_price() {
    // The sweep of what the run leaves pays for the script.
    let yaml = format!(
        "{HEAD}sources: {{ a: {{ owner: alice, lovelace: 100000000 }} }}\ntxs:\n  \
         p: {{ inputs: [a], outputs: [{{ to: alice, lovelace: change, reference_script: {{ bytes: 500 }} }}] }}\n"
    );
    let m = build(&yaml, &context("demo-context.yaml")).expect("materialises");
    assert_eq!(scripts(tx(&m, "p")), vec![Some(500)]);
    assert!(m.pparams.min_fee_ref_script_cost_per_byte.is_some());
    let mut ctx = context("demo-context.yaml");
    ctx.pparams.min_fee_ref_script_cost_per_byte = None;
    let e = build(&yaml, &ctx).expect_err("refused").to_string();
    assert!(e.contains("minFeeRefScriptCostPerByte"), "{e}");
}

#[test]
fn equal_prices_hash_alike() {
    let mut halves = context("demo-context.yaml");
    halves.pparams.min_fee_ref_script_cost_per_byte = Some(Rational {
        numerator: 30,
        denominator: 2,
    });
    let a = build(&spec(), &context("demo-context.yaml")).expect("materialises");
    let b = build(&spec(), &halves).expect("materialises");
    assert_eq!(a.pparams_hash, b.pparams_hash);
    assert_eq!(a.run_id, b.run_id);
}

#[test]
fn reading_a_script_needs_the_price() {
    let mut ctx = context("demo-context.yaml");
    ctx.pparams.min_fee_ref_script_cost_per_byte = None;
    let e = build(&spec(), &ctx).expect_err("refused").to_string();
    assert!(e.contains("minFeeRefScriptCostPerByte"), "{e}");
}

#[test]
fn a_script_too_large_for_its_tx_is_refused() {
    let yaml = format!(
        "{HEAD}sources: {{ a: {{ owner: alice, lovelace: 200000000 }} }}\ntxs:\n  \
         p: {{ inputs: [a], outputs: [{{ to: alice, lovelace: change, reference_script: {{ bytes: 16384 }} }}] }}\n"
    );
    let e = build(&yaml, &context("demo-context.yaml"))
        .expect_err("refused")
        .to_string();
    assert!(e.contains("maximum transaction size"), "{e}");
}

#[test]
fn a_script_tx_reading_a_script_evaluates() {
    // The burner's run sees the reference input with its script, as a node would.
    let yaml = format!(
        "{HEAD}sources: {{ a: {{ owner: alice, lovelace: 100000000 }}, \
         b: {{ owner: alice, lovelace: 10000000 }} }}\ntxs:\n  \
         p: {{ inputs: [a], outputs: [{{ to: alice, lovelace: change, reference_script: {{ bytes: 2000 }} }}] }}\n  \
         r: {{ inputs: [b], reference_inputs: [p.0], outputs: [{{ to: alice, lovelace: change }}], \
         cost: {{ cpu: 10% }} }}\n"
    );
    let m = build(&yaml, &context("demo-plutus-context.yaml")).expect("materialises");
    let r = tx(&m, "r");
    let read = r.reference_scripts_read.expect("reads a script");
    assert_eq!(read.bytes, 2_000);
    assert!(r.cost.is_some());
}

fn pp(price: Option<u64>) -> ProtocolParams {
    ProtocolParams {
        min_fee_a: 44,
        min_fee_b: 155_381,
        max_tx_size: 16_384,
        coins_per_utxo_byte: 4_310,
        min_fee_ref_script_cost_per_byte: price.map(|p| Rational {
            numerator: p,
            denominator: 1,
        }),
        ..ProtocolParams::default()
    }
}

fn held(n: usize, bytes: u32) -> Vec<HeldUtxo> {
    (0..n)
        .map(|i| HeldUtxo {
            utxo: OfferedUtxo {
                tx_id: [i as u8; 32],
                index: 0,
                lovelace: 100_000_000,
            },
            assets: Default::default(),
            reference_script_bytes: bytes,
        })
        .collect()
}

fn sweep_with(
    utxos: &[HeldUtxo],
    pp: &ProtocolParams,
) -> txflow::error::Result<Vec<txflow::funds::SignedTx>> {
    sweep_on(utxos, pp, TxFormat::Conway)
}

fn sweep_on(
    utxos: &[HeldUtxo],
    pp: &ProtocolParams,
    format: TxFormat,
) -> txflow::error::Result<Vec<txflow::funds::SignedTx>> {
    let to = txflow::funds::address(
        &txflow::context::SecretKeyBytes([9; 32]),
        NetworkName::Testnet,
    )
    .expect("address");
    sweep(
        &txflow::context::SecretKeyBytes([7; 32]),
        utxos,
        &txflow::assets::Policies::default(),
        &to,
        &RunTag {
            id: "0123456789abcdef",
            origin: "cli",
        },
        NetworkName::Testnet,
        pp,
        format,
    )
}

#[test]
fn a_sweep_pays_for_the_scripts_it_spends_in_cheap_chunks() {
    // 20 outputs of 15 000 bytes, three to a sweep tx: two fee tiers
    // (51 200 bytes) at most. The third tier's bytes would cost 168 960
    // lovelace more than at the first tier's price, more than the 155 381
    // of another tx.
    let txs = sweep_with(&held(20, 15_000), &pp(Some(15))).expect("sweeps");
    assert_eq!(txs.len(), 7);
    for t in &txs {
        let size_fee = 44 * t.size as u64 + 155_381;
        assert!(t.fee > size_fee + 15 * 15_000, "{}", t.fee);
    }
}

#[test]
fn a_sweep_without_the_price_leaves_scripts_behind() {
    let mut utxos = held(3, 0);
    utxos[1].reference_script_bytes = 1_000;
    let txs = sweep_with(&utxos, &pp(None)).expect("sweeps");
    assert_eq!(txs.len(), 1);
    // The ADA-only two are swept, the script one is not.
    let bytes = hex::decode(&txs[0].cbor_hex).unwrap();
    assert_eq!(MultiEraTx::decode(&bytes).unwrap().inputs().len(), 2);
}

#[test]
fn the_example_reads_three_scripts_per_reader() {
    let yaml = std::fs::read_to_string(example("reference-script-readers.yaml")).expect("example");
    let m = build(&yaml, &context("demo-plutus-context.yaml")).expect("materialises");
    for name in ["read1", "read2", "read3", "read4"] {
        let read = tx(&m, name).reference_scripts_read.expect("reads");
        // 25 600 bytes at 15, 19 400 at 18.
        assert_eq!(
            read,
            txflow::materialise::ManifestScriptsRead {
                bytes: 45_000,
                fee: 733_200
            }
        );
    }
    assert!(tx(&m, "plain1").reference_scripts_read.is_none());
}

#[test]
fn collateral_covers_a_script_tx_that_reads_scripts() {
    // `read1` runs the burner and reads 3 x 15 000 bytes, 733 200 lovelace of
    // reference-script fee, which the collateral (150% of the most a script
    // tx can pay) must cover on top of the size and execution fees.
    let example =
        std::fs::read_to_string(example("reference-script-readers.yaml")).expect("example");
    let reader = "read1: { inputs: [r1], reference_inputs: [p1.0, p2.0, p3.0], outputs: [{ to: alice, lovelace: change }], labels: [readers] }";
    assert!(example.contains(reader));
    let collateral = |line: &str| {
        let m = build(
            &example.replace(reader, line),
            &context("demo-plutus-context.yaml"),
        )
        .expect("materialises");
        let utxo = tx(&m, "read1").collateral.clone().expect("collateral");
        let index: usize = utxo.rsplit_once('#').unwrap().1.parse().unwrap();
        m.txs
            .iter()
            .find(|t| utxo.starts_with(&t.tx_id))
            .expect("funding tx")
            .outputs[index]
    };
    let reading = collateral(&reader.replace(
        "labels: [readers]",
        "labels: [readers], cost: { cpu: 100% }",
    ));
    let not_reading = collateral(
        &reader
            .replace("reference_inputs: [p1.0, p2.0, p3.0], ", "")
            .replace(
                "labels: [readers]",
                "labels: [readers], cost: { cpu: 100% }",
            ),
    );
    assert!(
        reading >= not_reading + 733_200 * 3 / 2,
        "{reading} vs {not_reading}"
    );
}

#[test]
fn a_network_limit_below_the_scripts_read_is_refused() {
    let mut ctx = dijkstra();
    ctx.pparams.max_ref_script_size_per_tx = Some(10_000);
    let e = build(&spec(), &ctx).expect_err("refused").to_string();
    assert!(e.contains("maxRefScriptSizePerTx"), "{e}");
}

#[test]
fn dijkstra_tiers_from_the_context_set_the_fee() {
    let mut ctx = dijkstra();
    ctx.pparams.ref_script_cost_stride = Some(10_000);
    let m = build(&spec(), &ctx).expect("materialises");
    // 10 000 bytes at 15, 2 000 at 18.
    assert_eq!(
        tx(&m, "r").reference_scripts_read.unwrap().fee,
        150_000 + 36_000
    );
    assert_eq!(m.pparams.ref_script_cost_stride, Some(10_000));
    // Conway fixes the tiers, whatever a context copied from a Dijkstra node says.
    ctx.era = "conway".to_string();
    let m = build(&spec(), &ctx).expect("materialises");
    assert_eq!(tx(&m, "r").reference_scripts_read.unwrap().fee, 12_000 * 15);
    assert_eq!(m.pparams.ref_script_cost_stride, None);
}

#[test]
fn reading_assets_where_the_network_charges_for_them_is_refused() {
    let yaml = format!(
        "{HEAD}policies: {{ gold: {{ after_slot: 7 }} }}\n\
         sources: {{ s: {{ owner: alice, lovelace: 20000000 }}, b: {{ owner: alice, lovelace: 5000000 }} }}\ntxs:\n  \
         m: {{ inputs: [s], mint: {{ gold.a: 1 }}, outputs: [{{ to: alice, lovelace: min, tokens: {{ gold.a: 1 }} }}, {{ to: alice, lovelace: change }}] }}\n  \
         r: {{ inputs: [b], reference_inputs: [m.0], outputs: [{{ to: alice, lovelace: change }}] }}\n"
    );
    // The demo context gives no charge.
    let mut ctx = dijkstra();
    let free = build(&yaml, &ctx).expect("no charge given");
    ctx.pparams.ref_inputs_cost_per_multi_asset_policy = Some(0);
    let zero = build(&yaml, &ctx).expect("still free");
    assert_eq!(free.run_id, zero.run_id);
    ctx.pparams.ref_inputs_cost_per_multi_asset_policy = Some(1_000);
    let e = build(&yaml, &ctx).expect_err("refused").to_string();
    assert!(e.contains("refInputsCostPerMultiAssetPolicy"), "{e}");
    // Conway has no such charge, whatever the context says.
    ctx.era = "conway".to_string();
    build(&yaml, &ctx).expect("Conway ignores the parameter");
}

#[test]
fn conway_defaults_hash_alike_given_or_not() {
    let mut given = context("demo-context.yaml");
    given.pparams.ref_script_cost_stride = Some(25_600);
    given.pparams.ref_script_cost_multiplier = Some(Rational {
        numerator: 12,
        denominator: 10,
    });
    given.pparams.max_ref_script_size_per_tx = Some(204_800);
    let a = build(&spec(), &context("demo-context.yaml")).expect("materialises");
    let b = build(&spec(), &given).expect("materialises");
    assert_eq!(a.pparams_hash, b.pparams_hash);
    assert_eq!(a.run_id, b.run_id);
}

#[test]
fn a_script_past_the_network_limit_is_refused_even_unread() {
    let yaml = format!(
        "{HEAD}sources: {{ a: {{ owner: alice, lovelace: 100000000 }} }}\ntxs:\n  \
         p: {{ inputs: [a], outputs: [{{ to: alice, lovelace: change, reference_script: {{ bytes: 12000 }} }}] }}\n"
    );
    let mut ctx = dijkstra();
    ctx.pparams.max_ref_script_size_per_tx = Some(10_000);
    let e = build(&yaml, &ctx).expect_err("refused").to_string();
    assert!(e.contains("maxRefScriptSizePerTx"), "{e}");
}

#[test]
fn a_tx_creating_more_script_than_a_tx_holds_is_refused() {
    let yaml = format!(
        "{HEAD}sources: {{ a: {{ owner: alice, lovelace: 900000000 }} }}\ntxs:\n  \
         p: {{ inputs: [a], outputs: [{{ to: alice, lovelace: change, reference_script: {{ bytes: 204800 }} }}] }}\n"
    );
    let e = build(&yaml, &context("demo-context.yaml"))
        .expect_err("refused")
        .to_string();
    assert!(e.contains("maxTxSize"), "{e}");
}

#[test]
fn an_unsweepable_script_utxo_needs_no_price() {
    // A UTxO holding a foreign asset is left where it is, so its script
    // needs no price, and the ADA-only one beside it is swept.
    let mut utxos = held(2, 0);
    utxos[1].reference_script_bytes = 1_000;
    utxos[1]
        .assets
        .entry([9; 28])
        .or_default()
        .insert(b"x".to_vec(), 1);
    let txs = sweep_with(&utxos, &pp(None)).expect("sweeps");
    assert_eq!(txs.len(), 1);
}

#[test]
fn a_large_script_does_not_hold_back_plain_utxos() {
    // One 60 000-byte script, past the sweep's two-tier chunk, then plain
    // UTxOs: they still share its tx.
    let mut utxos = held(5, 0);
    utxos[0].reference_script_bytes = 60_000;
    let txs = sweep_with(&utxos, &pp(Some(15))).expect("sweeps");
    assert_eq!(txs.len(), 1);
}

#[test]
fn a_sweep_with_falling_tier_prices_is_quick() {
    let mut pp = pp(Some(15));
    pp.ref_script_cost_stride = Some(1);
    pp.ref_script_cost_multiplier = Some(Rational {
        numerator: 4,
        denominator: 5,
    });
    let started = std::time::Instant::now();
    let txs = sweep_on(&held(3, 10_000), &pp, TxFormat::Dijkstra).expect("sweeps");
    assert_eq!(txs.len(), 1);
    assert!(started.elapsed().as_secs() < 10, "{:?}", started.elapsed());
}

#[test]
fn a_sweep_with_slowly_rising_tier_prices_is_quick() {
    let mut pp = pp(Some(15));
    pp.ref_script_cost_stride = Some(1);
    pp.ref_script_cost_multiplier = Some(Rational {
        numerator: 10_001,
        denominator: 10_000,
    });
    let started = std::time::Instant::now();
    sweep_on(&held(3, 10_000), &pp, TxFormat::Dijkstra).expect("sweeps");
    assert!(started.elapsed().as_secs() < 10, "{:?}", started.elapsed());
}

#[test]
fn a_sweep_with_senseless_pricing_is_an_error() {
    let mut pp = pp(Some(15));
    pp.ref_script_cost_stride = Some(0);
    assert!(sweep_on(&held(2, 1_000), &pp, TxFormat::Dijkstra).is_err());
    // Conway fixes the stride, so the same parameters sweep there.
    assert_eq!(sweep_with(&held(2, 1_000), &pp).expect("sweeps").len(), 1);
}

#[test]
fn a_sweep_whose_largest_chunk_would_overflow_the_fee_takes_smaller_ones() {
    let mut pp = pp(Some(15));
    pp.ref_script_cost_stride = Some(500);
    // A fee for all of these at once is past u64; one tier at a time is not.
    let txs = sweep_on(&held(10, 15_000), &pp, TxFormat::Dijkstra).expect("sweeps");
    assert!(txs.len() > 1);
}

#[test]
fn a_large_script_after_plain_utxos_still_shares_their_tx() {
    let mut utxos = held(5, 0);
    utxos[4].reference_script_bytes = 60_000;
    let txs = sweep_with(&utxos, &pp(Some(15))).expect("sweeps");
    assert_eq!(txs.len(), 1);
}
