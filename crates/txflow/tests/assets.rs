//! Native assets on chain: minting under time-lock policies, multi-asset
//! outputs, their min-UTxO, and the network's value size limit.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pallas_traverse::MultiEraTx;
use txflow::assets::{policy_script, script_hash};
use txflow::context::ChainContext;
use txflow::expand::expand;
use txflow::materialise::{materialise, Manifest, TxKind};
use txflow::spec::ExperimentSpec;

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name)
}

/// The demo context with Conway encoding, which Pallas can decode back.
fn context(file: &str) -> ChainContext {
    let mut ctx = ChainContext::load(&example(file)).expect("demo context loads");
    ctx.era = "conway".to_string();
    ctx
}

fn build(yaml: &str, ctx: &ChainContext) -> txflow::error::Result<Manifest> {
    let spec = ExperimentSpec::from_yaml(yaml).expect("spec");
    let ir = expand(&spec).expect("ir");
    materialise(&spec.experiment, &ir, spec.seed, ctx)
}

fn shuffle(with: &str) -> String {
    format!(
        "version: 0\nexperiment: a\nseed: 3\nwallets: {{ alice: {{}} }}\n\
         patterns:\n  - {{ id: x, use: asset_shuffle, with: {with} }}\n"
    )
}

/// Every asset of a decoded tx's outputs, as `(policy hex, name, quantity)`.
type Bag = BTreeMap<(String, Vec<u8>), i128>;

fn outputs_of(tx: &MultiEraTx) -> Vec<Bag> {
    tx.outputs()
        .iter()
        .map(|o| {
            let mut bag = Bag::new();
            for policy in o.value().assets() {
                for asset in policy.assets() {
                    *bag.entry((hex::encode(policy.policy()), asset.name().to_vec()))
                        .or_default() += i128::from(asset.output_coin().unwrap());
                }
            }
            bag
        })
        .collect()
}

#[test]
fn every_tx_conserves_every_asset_and_mints_under_its_policies() {
    let text = std::fs::read_to_string(example("asset-shuffle.yaml")).expect("example");
    let m = build(&text, &context("demo-context.yaml")).expect("materialises");
    assert_eq!(m.policies.len(), 3);
    let ids: BTreeMap<String, u64> = m
        .policies
        .values()
        .map(|p| (p.policy_id.clone(), p.after_slot))
        .collect();
    for slot in [1, 2, 3] {
        assert_eq!(ids[&hex::encode(script_hash(&policy_script(slot)))], slot);
    }

    // What each built output holds, by `<tx id>#<index>`.
    let mut utxos: BTreeMap<String, Bag> = BTreeMap::new();
    for entry in m.txs.iter() {
        let bytes = hex::decode(&entry.cbor_hex).unwrap();
        let tx = MultiEraTx::decode(&bytes).expect("decodes");
        let mut balance = Bag::new();
        for input in &entry.inputs {
            for (asset, quantity) in utxos.remove(&input.utxo).unwrap_or_default() {
                *balance.entry(asset).or_default() += quantity;
            }
        }
        let mut minted = 0;
        for policy in tx.mints() {
            for asset in policy.assets() {
                minted += 1;
                *balance
                    .entry((hex::encode(policy.policy()), asset.name().to_vec()))
                    .or_default() += i128::from(asset.mint_coin().unwrap());
            }
        }
        let outputs = outputs_of(&tx);
        for (i, bag) in outputs.iter().enumerate() {
            for (asset, quantity) in bag {
                *balance.entry(asset.clone()).or_default() -= quantity;
            }
            utxos.insert(format!("{}#{i}", entry.tx_id), bag.clone());
        }
        assert!(
            balance.values().all(|q| *q == 0),
            "{}: {balance:?}",
            entry.name
        );
        assert_eq!(minted, entry.mint.len(), "{}", entry.name);
        if entry.kind == TxKind::Experiment {
            let counts: Vec<usize> = outputs.iter().map(BTreeMap::len).collect();
            assert_eq!(entry.assets, counts, "{}", entry.name);
        }
        if minted > 0 {
            // The scripts of the policies used, by id, and valid from their latest slot.
            let used: Vec<String> = tx.mints().iter().map(|p| hex::encode(p.policy())).collect();
            let scripts: Vec<String> = tx
                .native_scripts()
                .iter()
                .map(|s| hex::encode(script_hash(s.raw_cbor())))
                .collect();
            let mut sorted = used.clone();
            sorted.sort();
            assert_eq!(scripts, sorted, "{}", entry.name);
            let latest = used.iter().map(|id| ids[id]).max();
            assert_eq!(tx.validity_start(), latest, "{}", entry.name);
            assert_eq!(entry.valid_from_slot, latest);
            assert!(tx.vkey_witnesses().len() == 1, "only the run key signs");
        } else {
            assert_eq!(tx.validity_start(), None, "{}", entry.name);
        }
    }
    let by = |name: &str| m.txs.iter().find(|t| t.name == name).unwrap();
    assert_eq!(by("shuffle/mint1").assets, [5]);
    assert_eq!(by("shuffle/merge1").assets, [10]);
    assert_eq!(by("shuffle/split1").assets, [5, 5]);
}

#[test]
fn outputs_with_assets_hold_their_min_utxo() {
    let ctx = context("demo-context.yaml");
    let m = build(&shuffle("{ utxos: 2, assets: 40, policies: 4 }"), &ctx).expect("builds");
    for entry in m.txs.iter().filter(|t| t.kind == TxKind::Experiment) {
        let bytes = hex::decode(&entry.cbor_hex).unwrap();
        let tx = MultiEraTx::decode(&bytes).expect("decodes");
        for (output, lovelace) in tx.outputs().iter().zip(&entry.outputs) {
            let size = pallas_codec::minicbor::to_vec(output.as_conway().unwrap())
                .unwrap()
                .len() as u64;
            let ledger_min = (160 + size) * ctx.pparams.coins_per_utxo_byte;
            assert!(
                *lovelace >= ledger_min,
                "{}: {lovelace} < {ledger_min}",
                entry.name
            );
        }
    }
    // The `min` output of the split holds no more than its minimum needs:
    // the bound only rounds the coin up to nine bytes.
    let split = m.txs.iter().find(|t| t.name == "x/split1").unwrap();
    let bytes = hex::decode(&split.cbor_hex).unwrap();
    let tx = MultiEraTx::decode(&bytes).unwrap();
    let first = tx.outputs()[0].clone();
    let size = pallas_codec::minicbor::to_vec(first.as_conway().unwrap())
        .unwrap()
        .len() as u64;
    assert!(split.outputs[0] <= (160 + size + 8) * ctx.pparams.coins_per_utxo_byte);
}

#[test]
fn materialise_is_deterministic_with_several_native_scripts() {
    let ctx = context("demo-context.yaml");
    let yaml = shuffle("{ utxos: 3, assets: 6, policies: 6, burn: all }");
    let a = build(&yaml, &ctx).expect("builds");
    for _ in 0..5 {
        assert_eq!(a, build(&yaml, &ctx).expect("builds"));
    }
}

#[test]
fn a_utxo_holds_at_most_what_max_value_size_allows() {
    let ctx = context("demo-context.yaml");
    // 844 assets of one policy (names a1..a844) fit Musashi's 5000 bytes.
    build(&shuffle("{ utxos: 1, assets: 844 }"), &ctx).expect("844 assets fit");
    let error = build(&shuffle("{ utxos: 1, assets: 845 }"), &ctx)
        .expect_err("845 do not")
        .to_string();
    assert!(
        error.contains("tx `x/mint1`: output 0 holds 845 assets in a value of 5002 bytes; the network allows at most 5000 (maxValueSize)"),
        "{error}"
    );
    // Two UTxOs of 422 merge into one of 844; of 423, into one too large.
    build(&shuffle("{ utxos: 2, assets: 422 }"), &ctx).expect("844 merged fit");
    let error = build(&shuffle("{ utxos: 2, assets: 423 }"), &ctx)
        .expect_err("846 merged do not")
        .to_string();
    assert!(error.contains("tx `x/merge1`"), "{error}");
}

#[test]
fn minting_needs_max_value_size_and_records_it_only_then() {
    let mut ctx = context("demo-context.yaml");
    let with = build(&shuffle("{}"), &ctx).expect("builds");
    assert_eq!(with.pparams.max_value_size, Some(5000));
    let plain = std::fs::read_to_string(example("conflicting-chains.yaml")).unwrap();
    let before = build(&plain, &ctx).expect("builds");
    assert_eq!(before.pparams.max_value_size, None);
    ctx.pparams.max_value_size = None;
    assert_eq!(
        before,
        build(&plain, &ctx).expect("builds"),
        "unchanged without assets"
    );
    let error = build(&shuffle("{}"), &ctx)
        .expect_err("needs it")
        .to_string();
    assert!(error.contains("maxValueSize"), "{error}");
}

#[test]
fn a_script_cost_and_a_mint_go_together() {
    let mut ctx = ChainContext::load(&example("demo-plutus-context.yaml")).expect("loads");
    ctx.era = "conway".to_string();
    let yaml = "version: 0\nexperiment: a\nseed: 3\nwallets: { alice: {} }\n\
                patterns:\n  - id: x\n    use: asset_shuffle\n\
                \x20   with: { utxos: 2, assets: 5, policies: 2 }\n    cost: { cpu: 10% }\n";
    let m = build(yaml, &ctx).expect("builds");
    for entry in m.txs.iter().filter(|t| t.kind == TxKind::Experiment) {
        let cost = entry.cost.as_ref().expect("every tx runs the burner");
        assert_eq!(cost.target.steps, Some(1_000_000_000));
    }
    assert!(m.txs.iter().any(|t| !t.mint.is_empty()));
}

#[test]
fn a_mint_is_valid_from_the_later_of_its_policies_and_its_window() {
    let mut ctx = context("demo-context.yaml");
    ctx.slots = txflow::context::RunSlots {
        start: Some(100),
        tip: Some(95),
    };
    let interval = |with: &str| {
        let yaml = format!(
            "version: 0\nexperiment: a\nseed: 3\nwallets: {{ alice: {{}} }}\n\
             patterns:\n  - {{ id: x, use: asset_shuffle, with: {{ burn: all }}, validity: {with} }}\n"
        );
        let m = build(&yaml, &ctx).expect("builds");
        let by = |name: &str| {
            let entry = m.txs.iter().find(|t| t.name == name).unwrap();
            let bytes = hex::decode(&entry.cbor_hex).unwrap();
            let tx = MultiEraTx::decode(&bytes).unwrap();
            (tx.validity_start(), tx.ttl(), entry.valid_from_slot)
        };
        (by("x/mint1"), by("x/merge1"), by("x/burn"))
    };
    // The window opens at slot 105, after the policy's slot 1.
    let (mint, merge, burn) = interval("{ from: 5, until: 50 }");
    assert_eq!(mint, (Some(105), Some(150), Some(105)));
    assert_eq!(merge, (Some(105), Some(150), None));
    assert_eq!(burn, (Some(105), Some(150), Some(105)));
    // A window without a start leaves the policy's slot as the lower bound.
    let (mint, merge, _) = interval("{ until: 50 }");
    assert_eq!(mint, (Some(1), Some(150), Some(1)));
    assert_eq!(merge, (None, Some(150), None));
}
