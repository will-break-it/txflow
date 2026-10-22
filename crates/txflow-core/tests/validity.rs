//! Validity windows: inheritance from spec to pattern to tx, `step` and
//! `select`, the IR hash, and the checks' warnings.

use txflow_core::check::{check, check_within};
use txflow_core::expand::expand;
use txflow_core::ir::Ir;
use txflow_core::limits::Limits;
use txflow_core::network::Network;
use txflow_core::spec::ExperimentSpec;
use txflow_core::validity::{Anchor, Validity};

fn ir(yaml: &str) -> Ir {
    expand(&ExperimentSpec::from_yaml(yaml).expect("spec")).expect("expands")
}

fn error(yaml: &str) -> String {
    expand(&ExperimentSpec::from_yaml(yaml).expect("spec"))
        .expect_err("rejected")
        .to_string()
}

fn window(ir: &Ir, tx: &str) -> Option<Validity> {
    ir.txs.get(tx).expect("tx").validity
}

fn until(until: u64) -> Option<Validity> {
    Some(Validity {
        from: None,
        until: Some(until),
        anchor: Anchor::Start,
    })
}

const HEAD: &str =
    "version: 0\nexperiment: v\nseed: 1\nparams: { ttl: 6 }\nwallets: { alice: {} }\n";

#[test]
fn a_pattern_window_steps_by_generation() {
    let ir = ir(&format!(
        "{HEAD}patterns:\n  - {{ id: c, use: chain, with: {{ length: 3 }}, \
         validity: {{ from: 2, until: \"{{{{ ttl }}}}\", step: 4, anchor: tip }} }}\n"
    ));
    for (tx, from, until) in [("c/t1", 2, 6), ("c/t2", 6, 10), ("c/t3", 10, 14)] {
        assert_eq!(
            window(&ir, tx),
            Some(Validity {
                from: Some(from),
                until: Some(until),
                anchor: Anchor::Tip
            }),
            "{tx}"
        );
    }
}

#[test]
fn select_depth_covers_only_that_generation() {
    let ir = ir(&format!(
        "{HEAD}patterns:\n  - {{ id: c, use: chain, with: {{ length: 3 }}, \
         validity: {{ until: 3, select: {{ depth: 0 }} }} }}\n  \
         - {{ id: f, use: fan_in, with: {{ width: 2 }}, validity: {{ until: 9, select: {{ depth: 1 }} }} }}\n"
    ));
    assert_eq!(window(&ir, "c/t1"), until(3));
    assert_eq!(window(&ir, "c/t2"), None);
    assert_eq!(window(&ir, "c/t3"), None);
    let merged: Vec<&String> = ir
        .txs
        .iter()
        .filter(|(n, t)| n.starts_with("f/") && t.validity.is_some())
        .map(|(n, _)| n)
        .collect();
    assert_eq!(merged.len(), 1, "only the tx that merges the fan-in");
}

#[test]
fn a_tx_or_pattern_window_replaces_the_spec_one_whole() {
    let ir = ir(&format!(
        "{HEAD}validity: {{ from: 1, until: 20 }}\npatterns:\n  \
         - {{ id: a, use: chain, with: {{ length: 1 }} }}\n  \
         - {{ id: b, use: chain, with: {{ length: 1 }}, validity: {{ until: 5 }} }}\n\
         txs:\n  \
         x: {{ inputs: [a/t1.0], outputs: [{{ to: alice, lovelace: change }}] }}\n  \
         y: {{ inputs: [b/t1.0], outputs: [{{ to: alice, lovelace: change }}], validity: {{ until: 30, anchor: tip }} }}\n"
    ));
    let spec = Some(Validity {
        from: Some(1),
        until: Some(20),
        anchor: Anchor::Start,
    });
    assert_eq!(window(&ir, "a/t1"), spec);
    assert_eq!(window(&ir, "x"), spec);
    assert_eq!(window(&ir, "b/t1"), until(5));
    assert_eq!(
        window(&ir, "y").map(|v| (v.until, v.anchor)),
        Some((Some(30), Anchor::Tip))
    );
}

#[test]
fn a_spec_window_can_select_by_label() {
    let ir = ir(&format!(
        "{HEAD}validity: {{ until: 4, select: {{ label: side-a }} }}\npatterns:\n  \
         - {{ id: p, use: conflicting_chains, with: {{ length: 2 }} }}\n"
    ));
    assert_eq!(window(&ir, "p/a1"), until(4));
    assert_eq!(window(&ir, "p/a2"), until(4));
    assert_eq!(window(&ir, "p/b1"), None);
}

#[test]
fn windows_that_cannot_hold_a_tx_are_errors() {
    let pattern = |validity: &str| {
        format!("{HEAD}patterns:\n  - {{ id: c, use: chain, with: {{ length: 2 }}, validity: {validity} }}\n")
    };
    assert!(error(&pattern("{ from: 5, until: 5 }")).contains("must be after"));
    assert!(error(&pattern("{ step: 2 }")).contains("needs `from`, `until` or both"));
    assert!(error(&pattern("{ until: 3, select: { label: nope } }")).contains("the label `nope`"));
    assert!(error(&pattern("{ until: 3, select: { depth: 7 } }")).contains("generation 7"));
    assert!(error(&pattern("{ until: 3, select: {} }")).contains("needs `label`, `depth`"));
    let unknown = ExperimentSpec::from_yaml(&pattern("{ until: 3, anchor: now }"))
        .expect_err("unknown anchor")
        .to_string();
    assert!(unknown.contains("expected `start` or `tip`"), "{unknown}");
    let spec = format!(
        "{HEAD}validity: {{ until: 3, select: {{ label: x }} }}\npatterns:\n  - {{ id: c, use: chain }}\n"
    );
    assert!(error(&spec).contains("takes the spec's validity"));
    let tx = format!(
        "{HEAD}sources: {{ s: {{ owner: alice, lovelace: 5000000 }} }}\ntxs:\n  \
         t: {{ inputs: [s], outputs: [{{ to: alice, lovelace: change }}], validity: {{ from: 9, until: 2 }} }}\n"
    );
    assert!(error(&tx).contains("tx `t`"));
}

#[test]
fn a_window_changes_the_hash_and_none_keeps_it() {
    let plain = format!("{HEAD}patterns:\n  - {{ id: c, use: chain, with: {{ length: 2 }} }}\n");
    let hash = |yaml: &str| ir(yaml).derived.expect("derived").hash;
    let windowed = |validity: &str| {
        hash(&format!(
            "{HEAD}patterns:\n  - {{ id: c, use: chain, with: {{ length: 2 }}, validity: {validity} }}\n"
        ))
    };
    assert_ne!(hash(&plain), windowed("{ until: 4 }"));
    assert_ne!(
        windowed("{ until: 4 }"),
        windowed("{ until: 4, anchor: tip }")
    );
    // `anchor: start` is the default, so writing it does not change the graph.
    assert_eq!(
        windowed("{ until: 4 }"),
        windowed("{ until: 4, anchor: start }")
    );
    assert_eq!(windowed("{ until: 4 }"), windowed("{ until: 4, step: 0 }"));
}

#[test]
fn the_check_warns_about_windows_that_rule_a_tx_out() {
    let text = format!(
        "{HEAD}sources: {{ s: {{ owner: alice, lovelace: 9000000 }} }}\ntxs:\n  \
         p: {{ inputs: [s], outputs: [{{ to: alice, lovelace: change }}], validity: {{ from: 10 }} }}\n  \
         c: {{ inputs: [p.0], outputs: [{{ to: alice, lovelace: change }}], validity: {{ until: 8, anchor: tip }} }}\n\
         schedule:\n  releases:\n    - {{ select: {{ names: [p] }}, target: frankfurt, at: 2s }}\n    \
         - {{ select: {{ names: [c] }}, target: frankfurt, at: 9s }}\n"
    );
    let report = check(&text);
    assert!(report.ok, "{:?}", report.errors);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("`c` can never land")),
        "{:?}",
        report.warnings
    );
    // With the network's slot clock, also the sends outside their windows.
    let network =
        Network::from_text(include_str!("../../../networks/musashi.json")).expect("network");
    let report = check_within(&text, &Limits::default(), Some(&network));
    let warnings = report.warnings.join("\n");
    assert!(
        warnings.contains("`p` goes to `frankfurt` at 2s, before its window opens"),
        "{warnings}"
    );
    assert!(
        warnings.contains("`c` goes to `frankfurt` at 9s, after its window closes"),
        "{warnings}"
    );
    assert!(report
        .summary
        .expect("summary")
        .description
        .iter()
        .any(|d| d.contains("2 transactions carry a validity window")),);
}

#[test]
fn a_network_slot_clock_is_checked() {
    let network = |slots: &str| {
        Network::from_text(&format!(
            r#"{{"id":"n","name":"N","protocol":"p","magic":1,"relays":[],"slots":{slots}}}"#
        ))
    };
    assert!(network(r#"{"start":"2026-10-18T00:00:00Z","length_ms":1000}"#).is_ok());
    assert!(network(r#"{"start":"2026-10-18","length_ms":1000}"#).is_err());
    assert!(network(r#"{"start":"2026-10-18T00:00:00Z","length_ms":0}"#).is_err());
}
