//! Script cost: parsing, inheritance from spec to pattern to tx, and the IR.

use blake2::digest::{Update, VariableOutput};
use blake2::Blake2bVar;
use txflow_core::check::check;
use txflow_core::cost::{Cost, CostValue};
use txflow_core::expand::expand;
use txflow_core::ir::Ir;
use txflow_core::spec::ExperimentSpec;

fn ir(yaml: &str) -> Ir {
    expand(&ExperimentSpec::from_yaml(yaml).expect("spec")).expect("expands")
}

fn cost_of(ir: &Ir, tx: &str) -> Option<Cost> {
    ir.txs.get(tx).expect("tx").cost
}

const HEAD: &str =
    "version: 0\nexperiment: c\nseed: 1\nparams: { dose: 30 }\nwallets: { alice: {} }\n";

#[test]
fn values_parse_and_print_back_the_same() {
    for (text, value) in [
        ("25%", CostValue::Share(2500)),
        ("12.5%", CostValue::Share(1250)),
        ("0.05%", CostValue::Share(5)),
        ("100%", CostValue::Share(10_000)),
    ] {
        assert_eq!(CostValue::parse(text).ok(), Some(value), "{text}");
        assert_eq!(value.to_string(), text);
    }
    // Units are numbers; text is a share, spelled one way.
    assert_eq!(CostValue::Units(2_500_000_000).to_string(), "2500000000");
    for bad in [
        "0%",
        "0.0%",
        "100.01%",
        "1.234%",
        "%",
        "1.%",
        "007%",
        " 25%",
        "25 %",
        "-1",
        "0",
        "2500000000",
        "ten",
    ] {
        assert!(CostValue::parse(bad).is_err(), "{bad}");
    }
    assert_eq!(CostValue::Share(2500).units(10_000_000_000), 2_500_000_000);
    assert_eq!(CostValue::Share(1).units(16_500_000), 1650);
    assert_eq!(CostValue::Units(7).units(16_500_000), 7);
}

#[test]
fn the_spec_cost_covers_every_tx_and_a_pattern_or_tx_replaces_it() {
    let ir = ir(&format!(
        "{HEAD}cost: {{ cpu: 10% }}\n\
         patterns:\n  - {{ id: plain, use: chain, with: {{ length: 2 }} }}\n  \
         - {{ id: heavy, use: chain, with: {{ length: 2 }}, cost: {{ mem: \"{{{{ dose }}}}%\" }} }}\n\
         sources: {{ f: {{ owner: alice, lovelace: 5000000 }}, g: {{ owner: alice, lovelace: 5000000 }} }}\n\
         txs:\n  \
           own: {{ inputs: [f], outputs: [{{ to: alice, lovelace: change }}], cost: {{ cpu: 1000000 }} }}\n  \
           inherits: {{ inputs: [g], outputs: [{{ to: alice, lovelace: change }}] }}\n"
    ));
    let ten = Cost {
        cpu: Some(CostValue::Share(1000)),
        mem: None,
    };
    assert_eq!(cost_of(&ir, "plain/t1"), Some(ten));
    assert_eq!(cost_of(&ir, "inherits"), Some(ten));
    // A pattern's cost replaces the spec's whole, not field by field.
    assert_eq!(
        cost_of(&ir, "heavy/t2"),
        Some(Cost {
            cpu: None,
            mem: Some(CostValue::Share(3000)),
        })
    );
    assert_eq!(
        cost_of(&ir, "own"),
        Some(Cost {
            cpu: Some(CostValue::Units(1_000_000)),
            mem: None,
        })
    );
}

#[test]
fn select_label_limits_a_cost_to_one_side() {
    let ir = ir(&format!(
        "{HEAD}cost: {{ cpu: 25%, select: {{ label: side-a }} }}\n\
         patterns:\n  - {{ id: p, use: conflicting_chains, with: {{ length: 2 }} }}\n"
    ));
    assert!(cost_of(&ir, "p/a1").is_some());
    assert!(cost_of(&ir, "p/a2").is_some());
    assert_eq!(cost_of(&ir, "p/b1"), None);
    assert_eq!(cost_of(&ir, "p/b2"), None);
}

#[test]
fn a_cost_label_that_no_tx_has_is_an_error() {
    let pairs = "patterns:\n  - { id: p, use: conflicting_chains, with: { length: 2 } ";
    let report = check(&format!(
        "{HEAD}cost: {{ cpu: 25%, select: {{ label: side_a }} }}\n{pairs}}}\n"
    ));
    assert!(!report.ok);
    assert!(format!("{:?}", report.errors).contains("`side_a`"));
    let report = check(&format!(
        "{HEAD}{pairs}, cost: {{ cpu: 25%, select: {{ label: side_a }} }} }}\n"
    ));
    assert!(!report.ok);
    let errors = format!("{:?}", report.errors);
    assert!(
        errors.contains("pattern `p`") && errors.contains("`side_a`"),
        "{errors}"
    );
}

#[test]
fn a_cost_changes_the_ir_hash_and_rides_in_the_ir() {
    let spec = |cost: &str| {
        format!("{HEAD}{cost}patterns:\n  - {{ id: p, use: conflict_set, with: {{ k: 2 }} }}\n")
    };
    let plain = ir(&spec(""));
    let costed = ir(&spec("cost: { cpu: 25% }\n"));
    let hash = |ir: &Ir| ir.derived.as_ref().map(|d| d.hash.clone());
    assert_ne!(hash(&plain), hash(&costed));
    let json = serde_json::to_value(&costed).expect("json");
    assert_eq!(
        json["txs"]["p/x1"]["cost"],
        serde_json::json!({ "cpu": "25%" })
    );
    // The hash is over the IR's canonical JSON, cost included.
    let mut value = serde_json::to_value(&costed).expect("json");
    value.as_object_mut().expect("object").remove("derived");
    let mut hasher = <Blake2bVar as VariableOutput>::new(32).expect("blake2b");
    hasher.update(value.to_string().as_bytes());
    let mut digest = [0u8; 32];
    hasher.finalize_variable(&mut digest).expect("digest");
    assert_eq!(
        hash(&costed),
        Some(format!("blake2b256:{}", hex::encode(digest)))
    );
    // Without a cost the IR has no `cost` key, so older hashes stand.
    let json = serde_json::to_value(&plain).expect("json");
    assert!(json["txs"]["p/x1"].get("cost").is_none());
}

#[test]
fn bad_costs_are_reported_by_check() {
    for (cost, needle) in [
        ("cost: {}\n", "cpu"),
        ("cost: { cpu: 0% }\n", "above zero"),
        ("cost: { cpu: 150% }\n", "above 100%"),
        ("cost: { cpu: \"{{ nope }}%\" }\n", "nope"),
    ] {
        let report = check(&format!(
            "{HEAD}{cost}patterns:\n  - {{ id: p, use: conflict_set, with: {{ k: 2 }} }}\n"
        ));
        assert!(!report.ok, "{cost}");
        let errors = format!("{:?}", report.errors);
        assert!(errors.contains(needle), "{cost}: {errors}");
    }
}
