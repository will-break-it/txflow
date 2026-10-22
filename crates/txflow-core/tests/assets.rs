//! Native assets: policies, mint and burn, and the tokens each output holds.

use txflow_core::assets::flow;
use txflow_core::expand::expand;
use txflow_core::ir::Ir;
use txflow_core::spec::ExperimentSpec;

fn expand_yaml(yaml: &str) -> txflow_core::error::Result<Ir> {
    expand(&ExperimentSpec::from_yaml(yaml)?)
}

/// A spec with one source `s`, the policies `gold` (slot 7) and `silver`
/// (slot 8), and `txs`.
fn with_txs(txs: &str) -> String {
    format!(
        "version: 0\nexperiment: a\nseed: 0\nwallets: {{ alice: {{}} }}\n\
         policies: {{ gold: {{ after_slot: 7 }}, silver: {{ after_slot: 8 }} }}\n\
         sources: {{ s: {{ owner: alice, lovelace: 10000000 }} }}\n\
         txs:\n{txs}"
    )
}

fn shuffle(with: &str) -> String {
    format!(
        "version: 0\nexperiment: a\nseed: 0\nwallets: {{ alice: {{}} }}\n\
         patterns:\n  - {{ id: x, use: asset_shuffle, with: {with} }}\n"
    )
}

#[test]
fn change_takes_the_tokens_left_and_moves_follow_the_graph() {
    let ir = expand_yaml(&with_txs(
        "  m: { inputs: [s], mint: { gold.a: 5, silver.b: 2 }, outputs: [{ to: alice, lovelace: change }] }\n  \
         p: { inputs: [m.0], outputs: [{ to: alice, lovelace: min, tokens: { gold.a: 3 } }, { to: alice, lovelace: change }] }\n  \
         b: { inputs: [p.1], mint: { gold.a: -2 }, outputs: [{ to: alice, lovelace: change }] }\n",
    ))
    .expect("expands");
    let held = flow(&ir, &ir.derived.as_ref().unwrap().topo_order).expect("flows");
    let tokens = |output: &str| held.get(output).cloned().unwrap_or_default();
    assert_eq!(tokens("m.0").get("gold.a"), Some(&5));
    assert_eq!(tokens("m.0").get("silver.b"), Some(&2));
    assert_eq!(tokens("p.0").len(), 1);
    assert_eq!(tokens("p.1").get("gold.a"), Some(&2));
    assert_eq!(tokens("p.1").get("silver.b"), Some(&2));
    // Burning the last gold leaves only silver.
    assert_eq!(tokens("b.0").get("gold.a"), None);
    assert_eq!(tokens("b.0").get("silver.b"), Some(&2));
}

#[test]
fn paying_or_burning_more_than_held_names_the_tx() {
    for (txs, says) in [
        (
            "  m: { inputs: [s], mint: { gold.a: 1 }, outputs: [{ to: alice, lovelace: min, tokens: { gold.a: 2 } }, { to: alice, lovelace: change }] }\n",
            "tx `m` pays out more `gold.a`",
        ),
        (
            "  m: { inputs: [s], mint: { gold.a: -1 }, outputs: [{ to: alice, lovelace: change }] }\n",
            "tx `m` burns 1 of `gold.a`",
        ),
        (
            "  m: { inputs: [s], outputs: [{ to: alice, lovelace: min, tokens: { gold.a: 1 } }, { to: alice, lovelace: change }] }\n",
            "tx `m` pays out more `gold.a`",
        ),
        (
            "  m: { inputs: [s], mint: { copper.a: 1 }, outputs: [{ to: alice, lovelace: change }] }\n",
            "unknown policy `copper`",
        ),
        (
            "  m: { inputs: [s], mint: { gold.a: 1 }, outputs: [{ to: alice, lovelace: change, tokens: { gold.a: 1 } }] }\n",
            "the `change` output takes every token left",
        ),
        (
            "  m: { inputs: [s], mint: { gold.a: 0 }, outputs: [{ to: alice, lovelace: change }] }\n",
            "mints 0 of `gold.a`",
        ),
        (
            "  m: { inputs: [s], mint: { \"gold.has space\": 1 }, outputs: [{ to: alice, lovelace: change }] }\n",
            "an asset name is 1 to 32",
        ),
    ] {
        let error = expand_yaml(&with_txs(txs)).expect_err(says).to_string();
        assert!(error.contains(says), "{error}");
    }
}

#[test]
fn two_policies_on_one_slot_are_one_policy_id() {
    let error = expand_yaml(
        "version: 0\nexperiment: a\nseed: 0\nwallets: { alice: {} }\n\
         policies: { gold: { after_slot: 7 }, silver: { after_slot: 7 } }\n",
    )
    .expect_err("one script")
    .to_string();
    assert!(error.contains("both `after_slot: 7`"), "{error}");
}

#[test]
fn pattern_policies_take_free_slots_in_order() {
    let ir = expand_yaml(
        "version: 0\nexperiment: a\nseed: 0\nwallets: { alice: {} }\n\
         policies: { mine: { after_slot: 2 } }\n\
         patterns:\n  - { id: x, use: asset_shuffle, count: 2, with: { policies: 2 } }\n",
    )
    .expect("expands");
    let slots: Vec<(&str, u64)> = ir
        .policies
        .iter()
        .map(|(name, p)| (name.as_str(), p.after_slot))
        .collect();
    assert_eq!(
        slots,
        [
            ("mine", 2),
            ("x_0/p1", 1),
            ("x_0/p2", 3),
            ("x_1/p1", 4),
            ("x_1/p2", 5)
        ]
    );
    // Each copy's txs use its own policies.
    assert!(ir.txs["x_1/mint1"]
        .mint
        .keys()
        .all(|k| k.starts_with("x_1/p")));
}

#[test]
fn shuffle_merges_every_asset_and_splits_them_shuffled() {
    let ir = expand_yaml(&shuffle(
        "{ utxos: 3, assets: 4, policies: 3, rounds: 2, burn: all }",
    ))
    .expect("expands");
    let order = &ir.derived.as_ref().unwrap().topo_order;
    assert_eq!(
        order,
        &[
            "x/mint1", "x/mint2", "x/mint3", "x/merge1", "x/split1", "x/merge2", "x/split2",
            "x/burn"
        ]
    );
    let held = flow(&ir, order).expect("flows");
    assert_eq!(held["x/merge1.0"].len(), 12);
    for k in 0..3 {
        assert_eq!(held[&format!("x/split1.{k}")].len(), 4);
    }
    // Shuffled: a split output does not hold what one mint made, and the
    // two rounds split differently.
    let minted: Vec<_> = (1..=3)
        .map(|i| held[&format!("x/mint{i}.0")].clone())
        .collect();
    assert!((0..3).all(|k| !minted.contains(&held[&format!("x/split1.{k}")])));
    assert_ne!(held["x/split1.0"], held["x/split2.0"]);
    // Assets round robin over the policies; the burn leaves ADA only.
    assert_eq!(ir.policies.len(), 3);
    assert!(ir.txs["x/mint1"].mint.contains_key("x/p2.a2"));
    assert_eq!(ir.txs["x/burn"].mint.len(), 12);
    assert!(ir.txs["x/burn"].mint.values().all(|q| *q == -1));
    assert!(!held.contains_key("x/burn.0"));
}

#[test]
fn shuffle_arguments_are_checked() {
    for (with, says) in [
        ("{ assets: 1001 }", "the maximum is 1000"),
        (
            "{ utxos: 1, assets: 2, policies: 3 }",
            "each policy needs at least one asset",
        ),
        ("{ burn: some }", "`burn` must be `none` or `all`"),
    ] {
        let error = expand_yaml(&shuffle(with)).expect_err(says).to_string();
        assert!(error.contains(says), "{error}");
    }
    let error = expand_yaml(
        "version: 0\nexperiment: a\nseed: 0\nwallets: { alice: {} }\n\
         patterns:\n  - { id: x, use: chain, with: { burn: all } }\n",
    )
    .expect_err("no burn on chain")
    .to_string();
    assert!(error.contains("has no argument `burn`"), "{error}");
}

#[test]
fn assets_change_the_hash_and_their_absence_does_not() {
    let plain = expand_yaml(&with_txs(
        "  m: { inputs: [s], outputs: [{ to: alice, lovelace: change }] }\n",
    ))
    .expect("expands");
    let minting = expand_yaml(&with_txs(
        "  m: { inputs: [s], mint: { gold.a: 1 }, outputs: [{ to: alice, lovelace: change }] }\n",
    ))
    .expect("expands");
    let hash = |ir: &Ir| ir.derived.as_ref().unwrap().hash.clone();
    assert_ne!(hash(&plain), hash(&minting));
    // Without policies a graph hashes as it did before they existed.
    let without = expand_yaml(
        "version: 0\nexperiment: a\nseed: 0\nwallets: { alice: {} }\n\
         sources: { s: { owner: alice, lovelace: 10000000 } }\n\
         txs:\n  m: { inputs: [s], outputs: [{ to: alice, lovelace: change }] }\n",
    )
    .expect("expands");
    assert_ne!(hash(&plain), hash(&without), "unused policies still count");
    let mut stripped = plain.clone();
    stripped.policies.clear();
    assert_eq!(
        txflow_core::derive::ir_hash(&stripped).unwrap(),
        hash(&without)
    );
}

#[test]
fn the_check_describes_the_assets() {
    let report = txflow_core::check::check(&shuffle("{ utxos: 2, assets: 5, policies: 3 }"));
    assert!(report.ok, "{:?}", report.errors);
    let text = report.summary.unwrap().description.join("\n");
    assert!(
        text.contains("Native assets under 3 policy ids: 2 txs mint 10 assets."),
        "{text}"
    );
    assert!(
        text.contains("The most any output holds is 10 distinct assets (x/merge1.0)."),
        "{text}"
    );
}
