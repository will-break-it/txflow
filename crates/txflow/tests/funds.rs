//! Pool split into slots, and sweeps back.

use pallas_traverse::MultiEraTx;
use txflow::assets::{policy_script, script_hash, Assets, Policies};
use txflow::context::{NetworkName, OfferedUtxo, ProtocolParams, SecretKeyBytes};
use txflow::funds::{address, slot_key, split, sweep, HeldUtxo, RunTag, SWEEP_INPUTS};
use txflow::materialise::TxFormat;

const POOL: SecretKeyBytes = SecretKeyBytes([7; 32]);

fn pp() -> ProtocolParams {
    ProtocolParams {
        min_fee_a: 44,
        min_fee_b: 155_381,
        max_tx_size: 16_384,
        coins_per_utxo_byte: 4_310,
        ..ProtocolParams::default()
    }
}

fn utxos(n: usize, lovelace: u64) -> Vec<OfferedUtxo> {
    (0..n)
        .map(|i| OfferedUtxo {
            tx_id: [i as u8; 32],
            index: i as u64,
            lovelace,
        })
        .collect()
}

fn decode(cbor_hex: &str) -> Vec<u8> {
    hex::decode(cbor_hex).expect("hex")
}

#[test]
fn slot_keys_are_distinct_and_stable() {
    let a = slot_key(&POOL, 0);
    assert_eq!(a, slot_key(&POOL, 0));
    assert_ne!(a, slot_key(&POOL, 1));
    assert_ne!(a, slot_key(&SecretKeyBytes([8; 32]), 0));
    let addr = address(&a, NetworkName::Testnet).expect("address");
    assert!(addr.starts_with("addr_test1v"), "{addr}");
}

#[test]
fn split_pays_each_slot_and_returns_the_rest() {
    let ada = 1_000_000;
    let tx = split(
        &POOL,
        &utxos(1, 10_000 * ada),
        1..=10,
        900 * ada,
        NetworkName::Testnet,
        &pp(),
        TxFormat::Conway,
    )
    .expect("split");
    assert_eq!(tx.outputs.len(), 11);
    assert!(tx.outputs[..10].iter().all(|l| *l == 900 * ada));
    assert_eq!(tx.outputs.iter().sum::<u64>() + tx.fee, 10_000 * ada);

    let bytes = decode(&tx.cbor_hex);
    let decoded = MultiEraTx::decode(&bytes).expect("decodes");
    assert_eq!(hex::encode(decoded.hash()), tx.tx_id);
    // Slot 1 first: the Worker numbers slots from 1.
    let slot1 = address(&slot_key(&POOL, 1), NetworkName::Testnet).unwrap();
    let pool = address(&POOL, NetworkName::Testnet).unwrap();
    let outs: Vec<String> = decoded
        .outputs()
        .iter()
        .map(|o| o.address().unwrap().to_bech32().unwrap())
        .collect();
    assert_eq!(outs[0], slot1);
    let slot10 = address(&slot_key(&POOL, 10), NetworkName::Testnet).unwrap();
    assert_eq!(outs[9], slot10);
    assert_eq!(outs[10], pool);
}

#[test]
fn split_needs_enough_in_the_pool() {
    let err = split(
        &POOL,
        &utxos(1, 5_000_000),
        1..=10,
        1_000_000,
        NetworkName::Testnet,
        &pp(),
        TxFormat::Conway,
    )
    .unwrap_err();
    assert!(err.to_string().contains("inputs hold"), "{err}");
}

#[test]
fn sweep_returns_everything_in_chunks() {
    let key = slot_key(&POOL, 3);
    let to = address(&slot_key(&POOL, 0), NetworkName::Testnet).unwrap();
    let held: Vec<HeldUtxo> = utxos(SWEEP_INPUTS + 5, 2_000_000)
        .into_iter()
        .map(HeldUtxo::from)
        .collect();
    let txs = sweep(
        &key,
        &held,
        &Policies::default(),
        &to,
        &RunTag {
            id: "0123456789abcdef",
            origin: "txflow.fun",
        },
        NetworkName::Testnet,
        &pp(),
        TxFormat::Dijkstra,
    )
    .expect("sweep");
    assert_eq!(txs.len(), 2);
    let total: u64 = txs.iter().map(|t| t.outputs[0] + t.fee).sum();
    assert_eq!(total, held.iter().map(|u| u.utxo.lovelace).sum::<u64>());
    for tx in &txs {
        assert_eq!(tx.outputs.len(), 1);
        assert_eq!(decode(&tx.cbor_hex)[0], 0x83, "Dijkstra encoding");
        // The message names the tool, the site and the run.
        let bytes = decode(&tx.cbor_hex);
        for part in ["txflow/1 txflow.fun", "sweep", "run:0123456789abcdef"] {
            assert!(
                bytes.windows(part.len()).any(|w| w == part.as_bytes()),
                "{part}"
            );
        }
    }
    let nothing = sweep(
        &key,
        &[],
        &Policies::default(),
        &to,
        &RunTag {
            id: "r",
            origin: "cli",
        },
        NetworkName::Testnet,
        &pp(),
        TxFormat::Conway,
    );
    assert_eq!(nothing.expect("empty sweep"), vec![]);
}

#[test]
fn sweep_burns_the_runs_assets_and_leaves_foreign_ones() {
    let spec = txflow::spec::ExperimentSpec::from_yaml(
        "version: 0\nexperiment: s\nseed: 0\nwallets: { alice: {} }\n\
         patterns:\n  - { id: x, use: asset_shuffle, with: { utxos: 2, assets: 3, policies: 2 } }\n",
    )
    .expect("spec");
    let ir = txflow::expand::expand(&spec).expect("expands");
    let policies = Policies::of(&ir);
    let p1 = script_hash(&policy_script(1));
    let p2 = script_hash(&policy_script(2));
    let foreign = script_hash(&policy_script(99));
    let holding = |id: [u8; 28], names: &[&str]| -> Assets {
        let mut assets = Assets::new();
        assets.insert(
            id,
            names.iter().map(|n| (n.as_bytes().to_vec(), 1)).collect(),
        );
        assets
    };
    let mut both = holding(p1, &["a1", "a3"]);
    both.extend(holding(p2, &["a2"]));
    let mut held: Vec<HeldUtxo> = utxos(3, 5_000_000)
        .into_iter()
        .map(HeldUtxo::from)
        .collect();
    held[0].assets = both;
    held[2].assets = holding(foreign, &["x"]);
    let key = slot_key(&POOL, 3);
    let to = address(&slot_key(&POOL, 0), NetworkName::Testnet).unwrap();
    let txs = sweep(
        &key,
        &held,
        &policies,
        &to,
        &RunTag {
            id: "r",
            origin: "cli",
        },
        NetworkName::Testnet,
        &pp(),
        TxFormat::Conway,
    )
    .expect("sweep");
    assert_eq!(txs.len(), 1);
    let bytes = decode(&txs[0].cbor_hex);
    let tx = MultiEraTx::decode(&bytes).expect("decodes");
    // Two inputs: the one with a foreign asset stays.
    assert_eq!(tx.inputs().len(), 2);
    let mut burned: Vec<(String, i64)> = tx
        .mints()
        .iter()
        .flat_map(|p| {
            p.assets()
                .iter()
                .map(|a| {
                    (
                        String::from_utf8(a.name().to_vec()).unwrap(),
                        a.mint_coin().unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect();
    burned.sort();
    assert_eq!(
        burned,
        vec![
            ("a1".to_string(), -1),
            ("a2".to_string(), -1),
            ("a3".to_string(), -1)
        ]
    );
    // Valid from the latest policy's slot, and only ADA goes out.
    assert_eq!(tx.validity_start(), Some(2));
    assert!(tx.outputs()[0].value().assets().is_empty());
    assert_eq!(txs[0].outputs[0] + txs[0].fee, 10_000_000);
}
