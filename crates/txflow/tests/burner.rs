//! The burner script: its encoding and how precisely it meets a cost.

use txflow::burner::{Calibration, Knobs, BURNER_CBOR_HEX, BURNER_UPLC};
use uplc::ast::{DeBruijn, Name, Program};

#[test]
fn committed_bytes_are_the_source_encoded() {
    let program: Program<Name> = uplc::parser::program(BURNER_UPLC).expect("parses");
    let program: Program<DeBruijn> = program.try_into().expect("closed");
    let encoded = hex::encode(program.to_cbor().expect("encodes"));
    if std::env::var("UPDATE_BURNER").is_ok() {
        println!("BURNER_CBOR_HEX = {encoded}");
    }
    assert_eq!(
        encoded, BURNER_CBOR_HEX,
        "re-run with UPDATE_BURNER=1 -- --nocapture"
    );
}

fn musashi() -> Calibration {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/musashi-pparams.json"
    ))
    .expect("example pparams");
    let json: serde_json::Value = serde_json::from_str(&text).expect("json");
    let costs: Vec<i64> = json["costModels"]["PlutusV3"]
        .as_array()
        .expect("V3 cost model")
        .iter()
        .map(|v| v.as_i64().expect("integer"))
        .collect();
    Calibration::new(&costs, 12).expect("calibrates")
}

#[test]
fn prediction_matches_a_run() {
    let calibration = musashi();
    for knobs in [
        Knobs {
            hashes: 0,
            bytes: 1,
            loops: 0,
        },
        Knobs {
            hashes: 3,
            bytes: 1000,
            loops: 7,
        },
        Knobs {
            hashes: 40,
            bytes: 65_536,
            loops: 0,
        },
        Knobs {
            hashes: 0,
            bytes: 1,
            loops: 2000,
        },
    ] {
        assert_eq!(
            calibration.run(knobs).expect("runs"),
            calibration.predict(knobs),
            "{knobs:?}"
        );
    }
}

#[test]
fn solved_costs_land_just_under_the_target() {
    let calibration = musashi();
    // Musashi's per-tx limit: 10,000,000,000 steps and 16,500,000 memory units.
    for (steps, mem) in [
        (Some(2_500_000_000), Some(330_000)),     // 25% CPU, 2% memory
        (Some(10_000_000_000), Some(16_500_000)), // the whole limit
        (Some(300_000_000), Some(1_000_000)),     // memory-heavy
        (Some(5_000_000_000), None),
        (None, Some(8_250_000)),
        (Some(20_000_000), None),
    ] {
        let knobs = calibration.solve(steps, mem).expect("solves");
        let used = calibration.run(knobs).expect("runs");
        if let Some(steps) = steps {
            assert!(used.steps <= steps, "{used:?} over {steps}");
            assert!(
                steps - used.steps <= steps / 1000 + 1_000_000,
                "{used:?} far below {steps}"
            );
        }
        if let Some(mem) = mem {
            assert!(used.mem <= mem, "{used:?} over {mem}");
            assert!(mem - used.mem <= 3_500, "{used:?} far below {mem}");
        }
    }
}

#[test]
fn unreachable_costs_say_what_is_reachable() {
    let calibration = musashi();
    let floor = calibration.floor();
    let error = calibration
        .solve(Some(floor.steps - 1), None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("below the burner's floor"), "{error}");
    // Far more CPU than hashing can use with this little memory.
    let error = calibration
        .solve(Some(10_000_000_000), Some(100_000))
        .unwrap_err()
        .to_string();
    assert!(error.contains("between"), "{error}");
}
