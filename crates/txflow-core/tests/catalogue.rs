//! The catalogue, the schema generated from it and `check` agree with the
//! patterns as implemented.

use std::path::Path;

use txflow_core::catalogue::{ArgKind, PATTERNS};
use txflow_core::check::check;

fn spec_with(pattern: &str, with: &str) -> String {
    format!(
        "version: 0\nexperiment: t\nseed: 1\nwallets: {{ alice: {{}} }}\n\
         patterns:\n  - {{ id: p, use: {pattern}, with: {with} }}\n"
    )
}

#[test]
fn every_catalogue_argument_is_accepted_by_its_pattern() {
    for pattern in PATTERNS {
        let report = check(&spec_with(pattern.name, pattern.example));
        assert!(report.ok, "{} example: {:?}", pattern.name, report.errors);
        for arg in pattern.args {
            let values: Vec<String> = match &arg.kind {
                ArgKind::Count { min, default, .. } => {
                    vec![min.to_string(), default.to_string()]
                }
                ArgKind::Lovelace { .. } => vec!["50000000".to_string()],
                ArgKind::Choice { values } => values.iter().map(|v| v.to_string()).collect(),
                ArgKind::Wallet => vec!["alice".to_string()],
            };
            for value in values {
                let with = format!("{{ {}: {value} }}", arg.name);
                let report = check(&spec_with(pattern.name, &with));
                assert!(
                    report.ok,
                    "{} with {with}: {:?}",
                    pattern.name, report.errors
                );
            }
        }
    }
}

#[test]
fn counts_below_the_catalogue_minimum_are_rejected() {
    for pattern in PATTERNS {
        for arg in pattern.args {
            if let ArgKind::Count { min, .. } = arg.kind {
                if min == 0 {
                    continue;
                }
                let with = format!("{{ {}: {} }}", arg.name, min - 1);
                assert!(
                    !check(&spec_with(pattern.name, &with)).ok,
                    "{} accepted {with}",
                    pattern.name
                );
            }
        }
    }
}

#[test]
fn counts_above_the_catalogue_maximum_are_rejected() {
    for pattern in PATTERNS {
        for arg in pattern.args {
            if let ArgKind::Count { max, .. } = arg.kind {
                let with = format!("{{ {}: {} }}", arg.name, max + 1);
                let report = check(&spec_with(pattern.name, &with));
                assert!(!report.ok, "{} accepted {with}", pattern.name);
                assert!(
                    report.errors[0].contains("maximum"),
                    "{} with {with}: {:?}",
                    pattern.name,
                    report.errors
                );
            }
        }
    }
}

#[test]
fn a_choice_outside_the_catalogue_is_rejected() {
    let report = check(&spec_with("conflicting_chains", "{ conflict: sideways }"));
    assert!(!report.ok);
    assert!(
        report.errors[0].contains("`per_step` or `root`"),
        "{:?}",
        report.errors
    );
}

#[test]
fn a_pattern_missing_from_the_catalogue_is_unknown() {
    let report = check(&spec_with("zigzag", "{}"));
    assert!(!report.ok);
    assert!(
        report.errors[0].contains("unknown pattern"),
        "{:?}",
        report.errors
    );
}

#[test]
fn check_describes_the_worked_example() {
    let text = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/conflicting-chains.yaml"),
    )
    .expect("example");
    let report = check(&text);
    assert!(report.ok, "{:?}", report.errors);
    let summary = report.summary.expect("summary");
    assert_eq!(summary.txs, 6);
    assert_eq!(summary.conflict_sets, 3);
    assert_eq!(summary.orphanable, 4);
    assert!(summary.description[0].starts_with("6 transactions funded by 5 sources"));
}

#[test]
fn check_reports_the_error_for_a_broken_spec() {
    let text = "version: 0\nexperiment: t\nseed: 1\nwallets: { alice: {} }\n\
                txs:\n  t: { inputs: [nowhere], outputs: [{ to: alice, lovelace: change }] }\n";
    let report = check(text);
    assert!(!report.ok);
    assert!(report.errors[0].contains("nowhere"), "{:?}", report.errors);
}

#[test]
fn json_specs_are_accepted() {
    let text = r#"{"version":0,"experiment":"j","seed":1,"wallets":{"alice":{}},
                  "patterns":[{"id":"c","use":"chain","with":{"length":3}}]}"#;
    let report = check(text);
    assert!(report.ok, "{:?}", report.errors);
}

/// The schema and catalogue for the current version are generated; rerun
/// with UPDATE_SCHEMA=1 after changing the catalogue.
#[test]
fn committed_schema_is_up_to_date() {
    let schema = serde_json::to_string_pretty(&txflow_core::schema::schema()).expect("schema");
    assert_committed(
        &txflow_core::schema::committed_path(),
        &format!("{schema}\n"),
    );
}

#[test]
fn committed_catalogue_is_up_to_date() {
    assert_committed(
        &txflow_core::schema::committed_catalogue_path(),
        &txflow_core::catalogue::markdown(),
    );
}

fn assert_committed(relative: &str, generated: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative);
    if std::env::var_os("UPDATE_SCHEMA").is_some() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("schema dir");
        }
        std::fs::write(&path, generated).expect("write");
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == generated,
        "{relative} is stale; run UPDATE_SCHEMA=1 cargo test"
    );
}

#[test]
fn limits_name_the_number_and_the_limit() {
    use txflow_core::check::check_within;
    use txflow_core::limits::Limits;

    let text = spec_with("chain", "{ length: 12 }");
    let roomy = Limits::from_text(r#"{ "max_txs": 12, "max_chain_length": 12 }"#).expect("limits");
    assert!(check_within(&text, &roomy, None).ok);

    let tight = Limits {
        max_txs: Some(10),
        max_chain_length: Some(5),
        max_outputs_per_tx: Some(1),
        max_funding_ada: Some(5),
    };
    let report = check_within(&text, &tight, None);
    assert!(!report.ok);
    assert_eq!(
        report.errors,
        vec![
            "12 transactions; this site allows at most 10",
            "a path of 12 dependent transactions; this site allows at most 5",
            "10 ADA of funding; this site allows at most 5 ADA",
        ]
    );
    let wide = check_within(&spec_with("fan_out", "{ width: 3 }"), &tight, None);
    assert!(
        wide.errors[0].contains("has 3 outputs"),
        "{:?}",
        wide.errors
    );
}

#[test]
fn the_committed_limits_file_parses() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../limits.json");
    let text = std::fs::read_to_string(path).expect("limits.json");
    let limits = txflow_core::limits::Limits::from_text(&text).expect("valid limits");
    assert_eq!(limits.describe().len(), 4);
}
