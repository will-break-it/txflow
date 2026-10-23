//! Schedules resolved against the graph and the site's network.

use std::path::Path;

use txflow_core::check::{check_within, Report};
use txflow_core::limits::Limits;
use txflow_core::network::Network;
use txflow_core::schedule::Via;

fn musashi() -> Network {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../networks/musashi.json");
    Network::from_text(&std::fs::read_to_string(path).expect("network file")).expect("valid")
}

/// Three relays that take both, and one that takes only node-to-node.
fn fixture() -> Network {
    Network::from_text(
        r#"{ "id": "test", "name": "Test", "protocol": "Ouroboros Leios", "magic": 1,
             "relays": [
               { "id": "europe", "label": "Europe", "host": "10.0.0.1", "via": ["n2c", "n2n"] },
               { "id": "americas", "label": "Americas", "host": "10.0.0.2", "via": ["n2c", "n2n"] },
               { "id": "asia", "label": "Asia", "host": "10.0.0.3", "via": ["n2c", "n2n"], "n2c_port": 3100 },
               { "id": "legacy", "label": "Legacy", "host": "10.0.0.4", "via": ["n2n"] }
             ] }"#,
    )
    .expect("fixture")
}

fn run(schedule: &str) -> Report {
    let text = format!(
        "version: 0\nexperiment: race\nseed: 1\nwallets: {{ alice: {{}} }}\n\
         patterns:\n  - {{ id: ds, use: conflict_set, with: {{ k: 3 }} }}\n\
         schedule:\n{schedule}"
    );
    check_within(&text, &Limits::default(), Some(&fixture()))
}

#[test]
fn a_three_way_race_sends_one_copy_to_each_relay() {
    let report = run("  releases:\n\
         \x20   - { select: { names: [ds/x1] }, target: europe }\n\
         \x20   - { select: { names: [ds/x2] }, target: americas }\n\
         \x20   - { select: { names: [ds/x3] }, target: asia }\n");
    assert!(report.ok, "{:?}", report.errors);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let plan = report.plan.expect("plan");
    let sends: Vec<(&str, &str, u64)> = plan
        .sends
        .iter()
        .map(|s| (s.tx.as_str(), s.target.as_str(), s.at_ms))
        .collect();
    assert_eq!(
        sends,
        vec![
            ("ds/x1", "europe", 0),
            ("ds/x2", "americas", 0),
            ("ds/x3", "asia", 0)
        ]
    );
    // Node-to-client unless the release says otherwise.
    assert!(plan.sends.iter().all(|s| s.via == Via::N2c));
}

#[test]
fn spacing_and_offsets_order_the_sends() {
    let report = run("  releases:\n\
         \x20   - { select: { pattern: ds }, target: europe, at: 100ms, spacing: 10ms }\n\
         \x20   - { select: { names: [ds/x2] }, target: asia, at: 0ms }\n");
    assert!(report.ok, "{:?}", report.errors);
    let plan = report.plan.expect("plan");
    let times: Vec<(String, u64)> = plan
        .sends
        .iter()
        .map(|s| (format!("{}@{}", s.tx, s.target), s.at_ms))
        .collect();
    assert_eq!(times[0], ("ds/x2@asia".to_string(), 0));
    assert_eq!(
        times[1..].iter().map(|t| t.1).collect::<Vec<_>>(),
        vec![100, 110, 120]
    );
}

#[test]
fn an_unknown_relay_lists_the_real_ones() {
    let report = run("  releases:\n    - { select: { pattern: ds }, target: tokyo }\n");
    assert!(!report.ok);
    assert!(
        report.errors[0].contains("unknown target `tokyo`")
            && report.errors[0].contains("`europe`, `americas`, `asia`, `legacy`"),
        "{:?}",
        report.errors
    );
}

#[test]
fn a_relay_without_n2c_rejects_it() {
    let report = run("  releases:\n    - { select: { pattern: ds }, target: legacy }\n");
    assert!(!report.ok);
    assert!(
        report.errors[0].contains("relay `legacy` does not accept `n2c`; it takes `n2n`"),
        "{:?}",
        report.errors
    );
    let report = run("  releases:\n    - { select: { pattern: ds }, target: legacy, via: n2n }\n");
    assert!(report.ok, "{:?}", report.errors);
}

#[test]
fn selections_must_match_something() {
    for (select, message) in [
        ("{ label: side-a }", "no transaction has the label `side-a`"),
        ("{ names: [ds/x9] }", "no transaction is named `ds/x9`"),
        ("{ pattern: chains }", "no pattern has the id `chains`"),
        ("{ label: a, pattern: ds }", "exactly one of"),
    ] {
        let report = run(&format!(
            "  releases:\n    - {{ select: {select}, target: europe }}\n"
        ));
        assert!(!report.ok, "{select}");
        assert!(
            report.errors[0].contains(message),
            "{select}: {:?}",
            report.errors
        );
    }
}

#[test]
fn transactions_no_release_sends_are_a_warning() {
    let report = run("  releases:\n    - { select: { names: [ds/x1] }, target: europe }\n");
    assert!(report.ok, "{:?}", report.errors);
    assert_eq!(
        report.warnings,
        vec!["no release sends `ds/x2` and `ds/x3`"]
    );
}

#[test]
fn durations_are_bounded() {
    let report = run("  releases:\n    - { select: { pattern: ds }, target: europe, at: 2h }\n");
    assert!(!report.ok);
    assert!(
        report.errors[0].contains("longer than one hour"),
        "{:?}",
        report.errors
    );
    let report = run("  releases:\n    - { select: { pattern: ds }, target: europe, at: 4000s }\n");
    assert!(
        report.errors[0].contains("longer than one hour"),
        "{:?}",
        report.errors
    );
}

#[test]
fn a_relay_needs_an_address() {
    let error = Network::from_text(
        r#"{ "id": "t", "name": "T", "protocol": "p", "magic": 1,
             "relays": [{ "id": "x", "label": "X", "host": "10.0.0.9", "via": [] }] }"#,
    )
    .expect_err("no address");
    assert!(error.to_string().contains("takes neither `n2n` nor `n2c`"));
}

#[test]
fn an_explorer_link_needs_https_and_the_tx_id() {
    let file = |tx: &str| {
        format!(
            r#"{{ "id": "t", "name": "T", "protocol": "p", "magic": 1,
                 "relays": [{{ "id": "x", "label": "X", "host": "10.0.0.9", "via": ["n2n"] }}],
                 "explorer": {{ "name": "Scan", "tx": "{tx}" }} }}"#
        )
    };
    assert!(Network::from_text(&file("https://scan.example/tx/{tx}")).is_ok());
    for bad in ["http://scan.example/tx/{tx}", "https://scan.example/"] {
        let error = Network::from_text(&file(bad)).expect_err(bad);
        assert!(error.to_string().contains("`explorer.tx`"), "{error}");
    }
    let error = Network::from_text(&file("https://scan.example/tx/{tx}").replace(
        r#""tx": "#,
        r#""address": "https://scan.example/a/", "tx": "#,
    ))
    .expect_err("address without placeholder");
    assert!(error.to_string().contains("`explorer.address`"), "{error}");
    let with_block = |template: &str| {
        file("https://scan.example/tx/{tx}")
            .replace(r#""tx": "#, &format!(r#""block": "{template}", "tx": "#))
    };
    assert!(Network::from_text(&with_block("https://scan.example/block/{block}")).is_ok());
    let error = Network::from_text(&with_block("https://scan.example/")).expect_err("block");
    assert!(error.to_string().contains("`explorer.block`"), "{error}");
    assert!(musashi()
        .explorer
        .is_some_and(|e| e.address.is_some() && e.block.is_some()));
}

#[test]
fn the_relay_example_sends_every_transaction() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/relay-race.yaml");
    let text = std::fs::read_to_string(path).expect("example");
    let report = check_within(&text, &Limits::default(), Some(&musashi()));
    assert!(report.ok, "{:?}", report.errors);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(report.plan.expect("plan").sends.len(), 9);
}

fn branch_merge(releases: &str) -> Report {
    let text = format!(
        "version: 0\nexperiment: d\nseed: 1\nwallets: {{ alice: {{}} }}\n\
         patterns:\n  - {{ id: diamond, use: branch_merge, with: {{}} }}\n\
         schedule:\n  releases:\n{releases}"
    );
    check_within(&text, &Limits::default(), Some(&fixture()))
}

#[test]
fn a_child_sent_before_its_parent_is_flagged() {
    // The merge reaches Europe before `c2` exists anywhere, and `c2`
    // goes to Asia although its parent went only to Europe.
    let report = branch_merge(
        "    - { select: { names: [diamond/root] }, target: europe, at: 0ms }\n\
         \x20   - { select: { names: [diamond/c1] }, target: europe, at: 10ms }\n\
         \x20   - { select: { names: [diamond/merge] }, target: europe, at: 50ms }\n\
         \x20   - { select: { names: [diamond/c2] }, target: asia, at: 250ms }\n",
    );
    assert!(report.ok, "{:?}", report.errors);
    assert_eq!(
        report.warnings,
        vec![
            "`diamond/merge` goes to `europe` at 50ms before its parent `diamond/c2` \
             (it goes to `asia` at 250ms) is sent anywhere, so the relay will reject it",
            "`diamond/c2` goes to `asia` at 250ms, but its parent `diamond/root` \
             (sent to `europe` at 0ms) is not sent there first; it is accepted only if \
             `asia` has received the parent by propagation in time",
        ]
    );
}

#[test]
fn parents_sent_first_to_the_same_relay_are_fine() {
    // One release, in dependency order, all at once: each parent goes first.
    let report = branch_merge("    - { select: { pattern: diamond }, target: europe }\n");
    assert!(report.ok, "{:?}", report.errors);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    // A parent sent to both relays covers a child on either.
    let report = branch_merge(
        "    - { select: { names: [diamond/root] }, target: europe }\n\
         \x20   - { select: { names: [diamond/root] }, target: asia }\n\
         \x20   - { select: { names: [diamond/c1] }, target: europe, at: 10ms }\n\
         \x20   - { select: { names: [diamond/c2] }, target: asia, at: 10ms }\n\
         \x20   - { select: { names: [diamond/c1, diamond/c2] }, target: americas, at: 20ms }\n\
         \x20   - { select: { names: [diamond/root] }, target: americas }\n\
         \x20   - { select: { names: [diamond/merge] }, target: americas, at: 30ms }\n",
    );
    assert!(report.ok, "{:?}", report.errors);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

#[test]
fn a_relay_renamed_after_its_city_still_answers_to_its_old_id() {
    let text = "version: 0\nexperiment: race\nseed: 1\nwallets: { alice: {} }\n\
         patterns:\n  - { id: ds, use: conflict_set, with: { k: 3 } }\n\
         schedule:\n  releases:\n\
         \x20   - { select: { names: [ds/x1] }, target: europe }\n\
         \x20   - { select: { names: [ds/x2] }, target: virginia }\n\
         \x20   - { select: { names: [ds/x3] }, target: asia }\n";
    let report = check_within(text, &Limits::default(), Some(&musashi()));
    assert!(report.ok, "{:?}", report.errors);
    let targets: Vec<String> = report
        .plan
        .expect("plan")
        .sends
        .into_iter()
        .map(|s| s.target)
        .collect();
    assert_eq!(targets, ["frankfurt", "virginia", "tokyo"]);
    assert_eq!(
        report.warnings,
        [
            "target `europe` is now called `frankfurt`; it still works, but use the new name",
            "target `asia` is now called `tokyo`; it still works, but use the new name",
        ]
    );
    // An id is taken once, earlier ids included.
    let twice = r#"{ "id": "t", "name": "T", "protocol": "p", "magic": 1, "relays": [
        { "id": "a", "label": "A", "host": "h", "via": ["n2c"], "aliases": ["b"] },
        { "id": "b", "label": "B", "host": "h", "via": ["n2c"] } ] }"#;
    assert!(Network::from_text(twice).is_err());
}

#[test]
fn a_temporary_relay_says_when_it_goes_in_utc() {
    let file = |until: &str| {
        format!(
            r#"{{ "id": "t", "name": "T", "protocol": "Ouroboros Leios", "magic": 1,
                 "relays": [ {{ "id": "oregon", "label": "Oregon", "host": "10.0.0.1", "via": ["n2c"], "until": "{until}" }} ] }}"#
        )
    };
    let network = Network::from_text(&file("2026-10-16T12:13:00Z")).expect("until");
    assert_eq!(
        network.relays[0].until.as_deref(),
        Some("2026-10-16T12:13:00Z")
    );
    for bad in [
        "2026-10-16",
        "2026-10-16T12:13:00+02:00",
        "2026-10-16 12:13:00Z",
        "tomorrow",
    ] {
        let error = Network::from_text(&file(bad)).expect_err(bad);
        assert!(
            error.to_string().contains("YYYY-MM-DDTHH:MM:SSZ"),
            "{error}"
        );
    }
}

#[test]
fn a_release_repeats_its_selection_each_wave() {
    let report = run("  releases:\n    - { select: { pattern: ds }, target: europe, every: 20ms, until: 50ms }\n");
    assert!(report.ok, "{:?}", report.errors);
    let plan = report.plan.expect("plan");
    let times: Vec<u64> = plan.sends.iter().map(|s| s.at_ms).collect();
    // conflict_set k:3 sends three txs per wave; waves fire at 0, 20 and 40 ms
    // (a 60 ms wave is not before `until: 50ms`).
    assert_eq!(times, vec![0, 0, 0, 20, 20, 20, 40, 40, 40]);
    assert!(plan.sends.iter().all(|s| s.via == Via::N2c));
    let report = run("  releases:\n    - { select: { pattern: ds }, target: europe, every: 20ms, until: 60ms }\n");
    assert!(report.ok, "{:?}", report.errors);
    assert_eq!(report.plan.expect("plan").sends.len(), 9);
}

#[test]
fn repetition_needs_both_bounds_and_a_future_until() {
    for (release, message) in [
        (
            "{ select: { pattern: ds }, target: europe, every: 20ms }",
            "`every` needs `until`",
        ),
        (
            "{ select: { pattern: ds }, target: europe, until: 50ms }",
            "`until` needs `every`",
        ),
        (
            "{ select: { pattern: ds }, target: europe, every: 20ms, until: 0ms }",
            "never fires",
        ),
    ] {
        let report = run(&format!("  releases:\n    - {release}\n"));
        assert!(!report.ok, "{release}");
        assert!(
            report.errors.iter().any(|e| e.contains(message)),
            "{release}: {:?}",
            report.errors
        );
    }
}

#[test]
fn offsets_take_units_and_bare_milliseconds() {
    use txflow_core::schedule::duration;
    assert_eq!(duration("1m").unwrap(), 60_000);
    assert_eq!(duration("1h").unwrap(), 3_600_000);
    assert_eq!(duration("45s").unwrap(), 45_000);
    assert!(duration("bogus").is_err());
    let report = run("  releases:\n    - { select: { names: [ds/x1] }, target: europe, at: 20000, spacing: 500, every: 5000, until: 90s }\n");
    assert!(report.ok, "{:?}", report.errors);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("`at` is a bare integer")),
        "{:?}",
        report.warnings
    );
    let at: Vec<u64> = report
        .plan
        .expect("plan")
        .sends
        .iter()
        .map(|s| s.at_ms)
        .collect();
    // Waves start at 20000, 25000, … every 5000 until 90000, one send each.
    assert_eq!(at.len(), (90000 - 20000) / 5000);
    assert_eq!(at[0], 20000);
    assert_eq!(at[1], 25000);
}

fn run_with_params(schedule: &str, params: &str) -> Report {
    let text = format!(
        "version: 0\nexperiment: race\nseed: 1\nparams: {{ {params} }}\n\
         wallets: {{ alice: {{}} }}\n\
         patterns:\n  - {{ id: ds, use: conflict_set, with: {{ k: 3 }} }}\n\
         schedule:\n{schedule}"
    );
    check_within(&text, &Limits::default(), Some(&fixture()))
}

#[test]
fn params_substitute_into_offsets_as_milliseconds() {
    let report = run_with_params(
        "  releases:\n    - { select: { names: [ds/x1] }, target: europe, every: \"{{ wave }}\", until: 90s }\n",
        "wave: 20000",
    );
    assert!(report.ok, "{:?}", report.errors);
    let at: Vec<u64> = report
        .plan
        .expect("plan")
        .sends
        .iter()
        .map(|s| s.at_ms)
        .collect();
    assert_eq!(at, vec![0, 20000, 40000, 60000, 80000]);
}

#[test]
fn a_unit_next_to_a_param_is_rejected_clearly() {
    let report = run_with_params(
        "  releases:\n    - { select: { names: [ds/x1] }, target: europe, every: \"{{ wave }}s\", until: 90s }\n",
        "wave: 20000",
    );
    assert!(!report.ok);
    assert!(
        report.errors[0].contains("must be the whole offset"),
        "{:?}",
        report.errors
    );
}

#[test]
fn negative_offsets_get_a_clear_message() {
    let report = run("  releases:\n    - { select: { names: [ds/x1] }, target: europe, at: -5 }\n");
    assert!(!report.ok);
    assert!(
        report.errors[0].contains("is not an offset"),
        "{:?}",
        report.errors
    );
}

#[test]
fn a_spec_survives_a_serialise_round_trip() {
    // The manual Offset deserializer must not break the derived Serialize:
    // formatting a spec (the site's to_yaml) has to parse back unchanged.
    let text = String::from(
        "version: 0\nexperiment: race\nseed: 1\nwallets: { alice: {} }\n\
         patterns:\n  - { id: ds, use: conflict_set, with: { k: 3 } }\n\
         schedule:\n  releases:\n    - { select: { pattern: ds }, target: europe, at: 2s, spacing: 1s }\n",
    );
    let first = txflow_core::spec::ExperimentSpec::from_yaml(&text).expect("parse");
    let written = serde_yaml::to_string(&first).expect("serialise");
    assert!(
        !written.contains("!Text") && !written.contains("!Ms"),
        "{written}"
    );
    txflow_core::spec::ExperimentSpec::from_yaml(&written).expect("reparse");
    let a = check_within(&text, &Limits::default(), Some(&fixture()));
    let b = check_within(&written, &Limits::default(), Some(&fixture()));
    assert!(a.ok && b.ok, "{:?} {:?}", a.errors, b.errors);
    let as_: Vec<u64> = a.plan.unwrap().sends.iter().map(|s| s.at_ms).collect();
    let bs: Vec<u64> = b.plan.unwrap().sends.iter().map(|s| s.at_ms).collect();
    assert_eq!(as_, bs);
}

#[test]
fn offsets_sharing_the_one_hour_cap_in_every_spelling() {
    for spelling in [
        "  releases:\n    - { select: { names: [ds/x1] }, target: europe, spacing: 7200000 }\n",
        "  releases:\n    - { select: { names: [ds/x1] }, target: europe, spacing: 7200000ms }\n",
    ] {
        let report = run(spelling);
        assert!(!report.ok, "{:?}", report.errors);
        assert!(
            report.errors[0].contains("longer than one hour"),
            "{:?}",
            report.errors
        );
    }
    let report = run_with_params(
        "  releases:\n    - { select: { names: [ds/x1] }, target: europe, spacing: \"{{ big }}\" }\n",
        "big: 3600001",
    );
    assert!(
        !report.ok && report.errors[0].contains("longer than one hour"),
        "{:?}",
        report.errors
    );
}

#[test]
fn a_zero_every_is_rejected() {
    let report = run(
        "  releases:\n    - { select: { names: [ds/x1] }, target: europe, every: 0, until: 10s }\n",
    );
    assert!(
        !report.ok && report.errors[0].contains("`every` resolves to 0"),
        "{:?}",
        report.errors
    );
    let report = run_with_params(
        "  releases:\n    - { select: { names: [ds/x1] }, target: europe, every: \"{{ wave }}\", until: 10s }\n",
        "wave: 0",
    );
    assert!(
        !report.ok && report.errors[0].contains("`every` resolves to 0"),
        "{:?}",
        report.errors
    );
}

#[test]
fn schedules_cap_their_total_sends() {
    let report = run(
        "  releases:\n    - { select: { pattern: ds }, target: europe, every: 5, until: 1h }\n",
    );
    assert!(
        !report.ok && report.errors[0].contains("plans at most"),
        "{:?}",
        report.errors
    );
}

#[test]
fn signed_offsets_no_longer_slip_past_the_schema() {
    // YAML's own reader turns unquoted `+500` into a number, so `check` and
    // the schema both accept a leading plus on bare integers.
    let report =
        run("  releases:\n    - { select: { names: [ds/x1] }, target: europe, at: +500 }\n");
    assert!(report.ok, "{:?}", report.errors);
    let report =
        run("  releases:\n    - { select: { names: [ds/x1] }, target: europe, at: ' 500 ' }\n");
    assert!(report.ok, "{:?}", report.errors);
}
