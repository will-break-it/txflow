//! Metadata and colour: where they apply, the ledger's rules and the limits.

use txflow_core::check::check_within;
use txflow_core::expand::expand;
use txflow_core::ir::Ir;
use txflow_core::limits::Limits;
use txflow_core::metadata::{auxiliary_data, encoded_size, Label, Metadata};
use txflow_core::spec::ExperimentSpec;

const HEAD: &str = "version: 0\nexperiment: t\nseed: 1\nwallets: { alice: {} }\n";

fn expand_yaml(body: &str) -> Result<Ir, String> {
    let spec = ExperimentSpec::from_yaml(&format!("{HEAD}{body}")).map_err(|e| e.to_string())?;
    expand(&spec).map_err(|e| e.to_string())
}

fn error(body: &str) -> String {
    expand_yaml(body).expect_err("rejected")
}

const TWO_CHAINS: &str = "\
patterns:
  - { id: a, use: chain, with: { length: 2 } }
  - id: b
    use: chain
    with: { length: 2 }
    color: '#FF0000'
    metadata: { 1: pattern, 3: [1, 2] }
sources:
  s: { owner: alice, lovelace: 5000000 }
txs:
  t:
    inputs: [s]
    outputs: [{ to: alice, lovelace: change }]
    color: 00ff88
    metadata: { 1: tx }
";

#[test]
fn the_most_specific_metadata_wins_label_by_label() {
    let ir = expand_yaml(&format!(
        "color: 7f3fbf\nmetadata: {{ 1: spec, 2: {{ study: mempool }} }}\n{TWO_CHAINS}"
    ))
    .expect("expands");
    let get = |tx: &str, label: u64| ir.txs[tx].metadata.get(&Label(label)).cloned();

    assert_eq!(get("a/t1", 1), Some("spec".into()));
    assert_eq!(
        get("a/t2", 2),
        Some(serde_json::json!({ "study": "mempool" }))
    );
    assert_eq!(ir.txs["a/t1"].color.as_deref(), Some("7f3fbf"));

    assert_eq!(get("b/t1", 1), Some("pattern".into()));
    assert_eq!(
        get("b/t1", 2),
        Some(serde_json::json!({ "study": "mempool" }))
    );
    assert_eq!(get("b/t2", 3), Some(serde_json::json!([1, 2])));
    assert_eq!(ir.txs["b/t2"].color.as_deref(), Some("ff0000"));

    assert_eq!(get("t", 1), Some("tx".into()));
    assert_eq!(get("t", 3), None);
    assert_eq!(ir.txs["t"].color.as_deref(), Some("00ff88"));
}

#[test]
fn metadata_changes_the_hash_and_its_absence_does_not() {
    let hash = |ir: Ir| ir.derived.expect("derived").hash;
    let plain = hash(expand_yaml("patterns: [{ id: a, use: chain }]\n").expect("plain"));
    let empty =
        hash(expand_yaml("metadata: {}\npatterns: [{ id: a, use: chain }]\n").expect("empty"));
    let tagged =
        hash(expand_yaml("color: ff0000\npatterns: [{ id: a, use: chain }]\n").expect("c"));
    let meta =
        hash(expand_yaml("metadata: { 1: x }\npatterns: [{ id: a, use: chain }]\n").expect("m"));
    assert_eq!(plain, empty);
    assert_ne!(plain, tagged);
    assert_ne!(plain, meta);
    assert_ne!(tagged, meta);
}

#[test]
fn labels_may_be_written_as_strings_in_json() {
    let spec = r#"{"version": 0, "experiment": "t", "seed": 1, "wallets": {"alice": {}},
        "metadata": {"1337": {"k": "0xdeadbeef"}}, "patterns": [{"id": "a", "use": "chain"}]}"#;
    let ir = expand(&ExperimentSpec::from_yaml(spec).expect("parses")).expect("expands");
    assert!(ir.txs["a/t1"].metadata.contains_key(&Label(1337)));
    let bad = r#"{"version": 0, "experiment": "t", "seed": 1, "wallets": {"alice": {}},
        "metadata": {"x1": 1}}"#;
    assert!(ExperimentSpec::from_yaml(bad).is_err());
}

#[test]
fn txflow_labels_are_reserved() {
    assert!(error("metadata: { 674: x }\n").contains("674 is reserved"));
    assert!(error("metadata: { 1022: '0xff0000' }\n").contains("write `color: rrggbb`"));
}

#[test]
fn the_ledger_rules_are_checked_up_front() {
    let long = "x".repeat(65);
    assert!(error(&format!("metadata: {{ 1: {long} }}\n")).contains("text of 65 bytes"));
    // 22 three-byte characters: 22 characters but 66 bytes.
    let wide = "€".repeat(22);
    assert!(error(&format!("metadata: {{ 1: {wide} }}\n")).contains("66 bytes"));
    let bytes = format!("0x{}", "ab".repeat(65));
    assert!(error(&format!("metadata: {{ 1: '{bytes}' }}\n")).contains("byte string of 65"));
    assert!(error("metadata: { 1: '0xabc' }\n").contains("not whole bytes"));
    assert!(error("metadata: { 1: 1.5 }\n").contains("not an integer"));
    assert!(error("metadata: { 1: true }\n").contains("no booleans"));
    assert!(error("metadata: { 1: null }\n").contains("no null"));
    assert!(error(&format!("metadata: {{ 1: {{ {long}: 1 }} }}\n")).contains("65 bytes"));
    assert!(error("metadata: { 1: [[[[[[[[1]]]]]]]] }\n").contains("deeper than 8"));
    assert!(expand_yaml(
        "metadata: { 1: [[[[[[[1]]]]]]], 2: 18446744073709551615, 3: -9223372036854775808 }\n\
         patterns: [{ id: a, use: chain }]\n"
    )
    .is_ok());
}

#[test]
fn errors_name_the_pattern_or_tx() {
    assert!(
        error("patterns: [{ id: p, use: chain, metadata: { 674: x } }]\n")
            .contains("pattern `p`: metadata label 674")
    );
    assert!(error("patterns: [{ id: p, use: chain, color: red }]\n")
        .contains("pattern `p`: color `red` must be six hex digits"));
    assert!(error(
        "sources: { s: { owner: alice, lovelace: 5000000 } }\n\
         txs: { t: { inputs: [s], outputs: [{ to: alice, lovelace: change }], color: '#12345' } }\n"
    )
    .contains("tx `t`: color `#12345`"));
}

#[test]
fn one_tx_asks_for_at_most_8_kb() {
    let item = format!("'{}'", "x".repeat(64));
    let list = vec![item; 130].join(", ");
    let message = error(&format!(
        "metadata: {{ 1: [{list}] }}\npatterns: [{{ id: a, use: chain }}]\n"
    ));
    assert!(
        message.contains("bytes of metadata; the most is 8192"),
        "{message}"
    );
}

#[test]
fn the_site_limit_counts_metadata_and_colour() {
    let text = format!(
        "{HEAD}color: ff0000\nmetadata: {{ 1: '{}' }}\npatterns: [{{ id: a, use: chain }}]\n",
        "x".repeat(64)
    );
    let limits = |max| Limits {
        max_metadata_bytes: Some(max),
        ..Limits::default()
    };
    // Label 1 (1) + text (2 + 64) + label 1022 (3) + three bytes (1 + 3).
    assert!(check_within(&text, &limits(74), None).ok);
    let report = check_within(&text, &limits(73), None);
    assert_eq!(
        report.errors,
        ["tx `a/t1` has 74 bytes of metadata; this site allows at most 73"]
    );
}

#[test]
fn auxiliary_data_is_one_map_in_label_order() {
    let mut metadata = Metadata::new();
    metadata.insert(Label(2000), serde_json::json!({ "b": -1, "a": "0x0aff" }));
    metadata.insert(Label(1), serde_json::json!(24));
    let aux = auxiliary_data(&["m"], &metadata, Some("ff0000"));
    let expected: Vec<u8> = [
        &[0xa4][..],                     // map(4)
        &[0x01, 0x18, 0x18],             // 1: 24
        &[0x19, 0x02, 0xa2, 0xa1, 0x63], // 674: {"msg" ...
        b"msg",
        &[0x81, 0x61, b'm'],                         // ["m"]}
        &[0x19, 0x03, 0xfe, 0x43, 0xff, 0x00, 0x00], // 1022: h'ff0000'
        &[0x19, 0x07, 0xd0, 0xa2],                   // 2000: map(2)
        &[0x61, b'a', 0x42, 0x0a, 0xff],             // "a": h'0aff'
        &[0x61, b'b', 0x20],                         // "b": -1
    ]
    .concat();
    assert_eq!(hex(&aux), hex(&expected));
    assert_eq!(
        encoded_size(&metadata, Some("ff0000")),
        expected.len() - 1 - 11 // without the map header and the 674 entry
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn the_tagged_example_checks_and_says_what_it_tags() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let text =
        std::fs::read_to_string(root.join("examples/tagged-double-spend.yaml")).expect("example");
    let limits = Limits::from_text(&std::fs::read_to_string(root.join("limits.json")).unwrap())
        .expect("limits");
    let report = check_within(&text, &limits, None);
    assert!(report.ok, "{:?}", report.errors);
    let description = report.summary.expect("summary").description;
    assert!(description.contains(&"Colour tag: 00ff88 on 5 txs, ff0000 on 3 txs.".to_string()));
    assert!(description.contains(&"Metadata label 1337 on 8 txs.".to_string()));
}
