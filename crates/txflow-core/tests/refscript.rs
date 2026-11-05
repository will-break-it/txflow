//! Reference scripts on outputs: the spec field, its checks and the IR.

use txflow_core::check::check;
use txflow_core::expand::expand;
use txflow_core::ir::Ir;
use txflow_core::refscript::{read_by, ReferenceScript, MAX_BYTES_PER_TX};
use txflow_core::spec::ExperimentSpec;

const HEAD: &str = "version: 0\nexperiment: r\nseed: 1\nwallets: { alice: {} }\n";

fn ir(yaml: &str) -> Ir {
    expand(&ExperimentSpec::from_yaml(yaml).expect("spec")).expect("expands")
}

fn error(yaml: &str) -> String {
    expand(&ExperimentSpec::from_yaml(yaml).expect("spec"))
        .expect_err("rejected")
        .to_string()
}

fn one_output(script: &str) -> String {
    format!(
        "{HEAD}sources: {{ s: {{ owner: alice, lovelace: 90000000 }} }}\ntxs:\n  \
         t: {{ inputs: [s], outputs: [{{ to: alice, lovelace: change, reference_script: {script} }}] }}\n"
    )
}

#[test]
fn an_output_carries_a_script_into_the_ir() {
    let ir = ir(&one_output("{ bytes: 3000 }"));
    let output = &ir.txs["t"].outputs[0];
    assert_eq!(
        output.reference_script,
        Some(ReferenceScript { bytes: 3000 })
    );
}

#[test]
fn the_script_changes_the_ir_hash() {
    let with = ir(&one_output("{ bytes: 3000 }"));
    let other = ir(&one_output("{ bytes: 3001 }"));
    assert_ne!(
        with.derived.as_ref().unwrap().hash,
        other.derived.as_ref().unwrap().hash
    );
}

#[test]
fn sizes_no_script_has_are_rejected() {
    let e = error(&one_output("{ bytes: 7 }"));
    assert!(e.contains("below the smallest, 8"), "{e}");
    assert_eq!(e.matches("graph").count(), 1, "{e}");
    let e = error(&one_output("{ bytes: 25 }"));
    assert!(e.contains("use 24 or 26"), "{e}");
    let e = error(&one_output("{ bytes: 65540 }"));
    assert!(e.contains("use 65538 or 65541"), "{e}");
    let e = error(&one_output("{ bytes: 204801 }"));
    assert!(e.contains("above 204800"), "{e}");
}

#[test]
fn unknown_fields_are_rejected() {
    let e = ExperimentSpec::from_yaml(&one_output("{ bytes: 100, language: plutus_v2 }"))
        .map(|s| expand(&s).map(|_| ()))
        .err()
        .map(|e| e.to_string())
        .unwrap_or_else(|| "accepted".to_string());
    assert!(e.contains("language"), "{e}");
}

/// `n` outputs of `bytes` each, all read by `r`.
fn read_many(n: usize, bytes: u32) -> String {
    let outputs: Vec<String> = (0..n)
        .map(|_| format!("{{ to: alice, lovelace: min, reference_script: {{ bytes: {bytes} }} }}"))
        .chain(std::iter::once(
            "{ to: alice, lovelace: change }".to_string(),
        ))
        .collect();
    let refs: Vec<String> = (0..n).map(|i| format!("p.{i}")).collect();
    format!(
        "{HEAD}sources: {{ a: {{ owner: alice, lovelace: 900000000 }}, \
         b: {{ owner: alice, lovelace: 9000000 }} }}\ntxs:\n  \
         p: {{ inputs: [a], outputs: [{}] }}\n  \
         r: {{ inputs: [b, p.{n}], reference_inputs: [{}], outputs: [{{ to: alice, lovelace: change }}] }}\n",
        outputs.join(", "),
        refs.join(", ")
    )
}

#[test]
fn a_tx_counts_what_it_spends_and_reads() {
    let ir = ir(&read_many(3, 1_000));
    assert_eq!(read_by(&ir, &ir.txs["r"]).unwrap(), 3_000);
    assert_eq!(read_by(&ir, &ir.txs["p"]).unwrap(), 0);
}

#[test]
fn reading_more_than_conway_allows_is_a_warning() {
    // 14 * 15 000 = 210 000 > 204 800: a Dijkstra network may allow it.
    let report = check(&read_many(14, 15_000));
    assert!(report.ok, "{:?}", report.errors);
    assert!(
        report.warnings.iter().any(|w| w.contains(&format!(
            "reads 210000 bytes of reference scripts, more than the {MAX_BYTES_PER_TX}"
        ))),
        "{:?}",
        report.warnings
    );
    // 13 outputs of 15 000 bytes do not fit in one tx either.
    let report = check(&read_many(13, 15_000));
    assert!(
        report
            .warnings
            .iter()
            .all(|w| !w.contains("spends or reads")),
        "{:?}",
        report.warnings
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("tx `p` creates 195000 bytes of reference scripts")),
        "{:?}",
        report.warnings
    );
    assert!(check(&read_many(1, 15_000)).warnings.is_empty());
}

#[test]
fn check_describes_the_scripts() {
    let report = check(&read_many(2, 1_000));
    assert!(report.ok, "{:?}", report.errors);
    let summary = report.summary.expect("summary");
    assert!(
        summary.description.iter().any(|line| line
            == "2 outputs carry a reference script (2000 bytes in all); 1 tx pays the \
                reference-script fee for spending or reading them, `r` the most at 2000 bytes."),
        "{:?}",
        summary.description
    );
}

#[test]
fn a_script_too_large_for_its_tx_is_a_warning() {
    let report = check(&one_output("{ bytes: 16200 }"));
    assert!(report.ok, "{:?}", report.errors);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("tx `t` creates 16200 bytes")),
        "{:?}",
        report.warnings
    );
    assert!(check(&one_output("{ bytes: 15000 }")).warnings.is_empty());
}
