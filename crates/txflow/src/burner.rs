//! The burner: one fixed Plutus V3 script that uses a chosen amount of CPU
//! and memory, so a spec can ask for script load by cost alone.
//!
//! The script reads three numbers from its redeemer: `hashes`, `bytes` and
//! `loops`. It hashes the first `bytes` bytes of a 64 KiB buffer `hashes`
//! times with blake2b-256, then counts down `loops` times, and returns unit.
//! It never fails, so anyone can spend an output locked by it, and its cost
//! depends only on those three numbers, not on the transaction around it.
//!
//! Every part of the cost is affine in the three numbers, so [`Calibration`]
//! measures the parts once per cost model by running the script, and
//! [`Calibration::solve`] picks the numbers for a target. Hashing is CPU-heavy
//! (up to about 25,000 steps per memory unit); counting is memory-heavy
//! (about 240 steps per unit), so mixing the two reaches any ratio between.

use pallas_crypto::hash::Hasher;
use uplc::ast::{DeBruijn, NamedDeBruijn, Program};
use uplc::machine::cost_model::{initialize_cost_model_with_protocol, ExBudget};
use uplc::machine::Machine;
use uplc::{Language, PlutusData};

use crate::error::{Error, Result};

/// The burner in textual UPLC. `BURNER_CBOR_HEX` is this, encoded; a test
/// checks they agree.
pub const BURNER_UPLC: &str = r#"
(program 1.1.0
 (lam ctx
  [ (lam redeemer_fields
    [ (lam fix
      [ (lam hashes
        [ (lam bytes
          [ (lam loops
            [ (lam buffer
              [ (lam slice
                [ (lam hash_loop
                  [ (lam count_loop
                    [ (lam done [ count_loop loops ]) [ hash_loop hashes ] ])
                    [ fix (lam self (lam n
                      (force [ (force (builtin ifThenElse))
                        [ (builtin equalsInteger) n (con integer 0) ]
                        (delay (con unit ()))
                        (delay [ self [ (builtin subtractInteger) n (con integer 1) ] ]) ]))) ] ])
                  [ fix (lam self (lam n
                    (force [ (force (builtin ifThenElse))
                      [ (builtin equalsInteger) n (con integer 0) ]
                      (delay (con unit ()))
                      (delay [ (lam digest [ self [ (builtin subtractInteger) n (con integer 1) ] ])
                               [ (builtin blake2b_256) slice ] ]) ]))) ] ])
                [ (builtin sliceByteString) (con integer 0) bytes buffer ] ])
              [ (lam double
                  [ double [ double [ double [ double [ double [ double [ double [ double [ double [ double [ double
                    (con bytestring #0000000000000000000000000000000000000000000000000000000000000000)
                  ] ] ] ] ] ] ] ] ] ] ])
                (lam b [ (builtin appendByteString) b b ]) ] ])
            [ (builtin unIData) [ (force (builtin headList))
              [ (force (builtin tailList)) [ (force (builtin tailList)) redeemer_fields ] ] ] ] ])
          [ (builtin unIData) [ (force (builtin headList)) [ (force (builtin tailList)) redeemer_fields ] ] ] ])
        [ (builtin unIData) [ (force (builtin headList)) redeemer_fields ] ] ])
      (lam f [ (lam x [ f (lam v [ x x v ]) ]) (lam x [ f (lam v [ x x v ]) ]) ]) ])
    [ (force (force (builtin sndPair))) [ (builtin unConstrData)
      [ (force (builtin headList)) [ (force (builtin tailList))
        [ (force (force (builtin sndPair))) [ (builtin unConstrData) ctx ] ] ] ] ] ] ])
)
"#;

/// The burner as it goes into a transaction: CBOR bytes wrapping the flat
/// encoding of [`BURNER_UPLC`].
pub const BURNER_CBOR_HEX: &str = "58cc010100232323232323232323232300200630020073007225333573466e1c005200014984c008cdc0800a4004600c44a666ae68cdc3800a4000293099180199b8100248008dca001999b8c4800000c004c8c004c004c004c004c004c004c004c004c004c004c0052201200000000000000000000000000000000000000000000000000000000000000000002337140020026eb4d5d09aba2357440086eb4d5d09aba2003375a6ae840088c8c0088cc0080080048c0088cc008008004d55cf1baa357426ae88d55cf1baa00101";

/// Size of the buffer the burner hashes from: 32 bytes doubled 11 times.
pub const BUFFER_BYTES: u64 = 65_536;

/// The burner's script bytes.
pub fn script() -> Vec<u8> {
    hex::decode(BURNER_CBOR_HEX).expect("BURNER_CBOR_HEX is valid hex")
}

/// blake2b-224 over the language tag (3, Plutus V3) and the script bytes.
pub fn script_hash() -> [u8; 28] {
    let mut bytes = vec![0x03];
    bytes.extend_from_slice(&script());
    *Hasher::<224>::hash(&bytes)
}

/// What the burner is told to do. `bytes` is at least 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Knobs {
    pub hashes: u64,
    pub bytes: u64,
    pub loops: u64,
}

impl Knobs {
    /// The redeemer: `Constr 0 [hashes, bytes, loops]` as Plutus data CBOR.
    pub fn redeemer(&self) -> Vec<u8> {
        let mut out = vec![0xd8, 0x79, 0x83];
        for value in [self.hashes, self.bytes, self.loops] {
            cbor_uint(&mut out, 0, value);
        }
        out
    }

    fn data(&self) -> PlutusData {
        uplc::plutus_data(&self.redeemer()).expect("a well-formed redeemer")
    }
}

/// Execution units: CPU steps and memory units.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Units {
    pub steps: u64,
    pub mem: u64,
}

/// The burner's costs under one cost model, measured by running it.
#[derive(Clone, Debug)]
pub struct Calibration {
    cost_model: Vec<i64>,
    protocol_major: u16,
    /// With no hashes and no loops.
    base: Units,
    /// One hash of one word (8 bytes), beyond `base`.
    hash: Units,
    /// Each further word hashed, per hash: CPU only.
    hash_word_steps: u64,
    /// One countdown step, beyond `base`.
    count: Units,
}

impl Calibration {
    /// Measure the burner under a Plutus V3 `cost_model` and the protocol
    /// version whose builtin semantics apply.
    pub fn new(cost_model: &[i64], protocol_major: u16) -> Result<Calibration> {
        let mut calibration = Calibration {
            cost_model: cost_model.to_vec(),
            protocol_major,
            base: Units { steps: 0, mem: 0 },
            hash: Units { steps: 0, mem: 0 },
            hash_word_steps: 0,
            count: Units { steps: 0, mem: 0 },
        };
        let knobs = |hashes, bytes, loops| Knobs {
            hashes,
            bytes,
            loops,
        };
        let base = calibration.run(knobs(0, 1, 0))?;
        let one_hash = calibration.run(knobs(1, 8, 0))?;
        let one_hash_two_words = calibration.run(knobs(1, 16, 0))?;
        let one_count = calibration.run(knobs(0, 1, 1))?;
        let less = |a: Units, b: Units| -> Result<Units> {
            Ok(Units {
                steps: a.steps.checked_sub(b.steps).ok_or_else(not_affine)?,
                mem: a.mem.checked_sub(b.mem).ok_or_else(not_affine)?,
            })
        };
        calibration.base = base;
        calibration.hash = less(one_hash, base)?;
        calibration.hash_word_steps = less(one_hash_two_words, one_hash)?.steps;
        calibration.count = less(one_count, base)?;
        if calibration.hash.mem == 0 || calibration.count.mem == 0 || calibration.count.steps == 0 {
            return Err(not_affine());
        }
        Ok(calibration)
    }

    /// The least the burner costs.
    pub fn floor(&self) -> Units {
        self.base
    }

    /// Predicted cost of `knobs`, from the calibration.
    pub fn predict(&self, knobs: Knobs) -> Units {
        let words = words(knobs.bytes);
        let hash_steps = self.hash.steps + self.hash_word_steps * (words - 1);
        Units {
            steps: self.base.steps + knobs.hashes * hash_steps + knobs.loops * self.count.steps,
            mem: self.base.mem + knobs.hashes * self.hash.mem + knobs.loops * self.count.mem,
        }
    }

    /// Run the burner with `knobs` and return what it used.
    pub fn run(&self, knobs: Knobs) -> Result<Units> {
        let bytes = script();
        let mut buffer = Vec::new();
        let program = Program::<DeBruijn>::from_cbor(&bytes, &mut buffer)
            .map_err(|e| Error::Materialise(format!("burner script: {e}")))?;
        let program: Program<NamedDeBruijn> = program.into();
        let context = PlutusData::Constr(uplc::Constr {
            tag: 121,
            any_constructor: None,
            fields: uplc::MaybeIndefArray::Indef(vec![
                PlutusData::BigInt(uplc::BigInt::Int(0.into())),
                knobs.data(),
                PlutusData::BigInt(uplc::BigInt::Int(0.into())),
            ]),
        });
        let program = program.apply_data(context);
        let budget = ExBudget {
            mem: i64::MAX / 2,
            cpu: i64::MAX / 2,
        };
        let cost_model = initialize_cost_model_with_protocol(
            &Language::PlutusV3,
            self.protocol_major,
            &self.cost_model,
        );
        let mut machine = Machine::new_with_protocol(
            Language::PlutusV3,
            self.protocol_major,
            cost_model,
            budget,
            200,
        );
        machine
            .run(program.term)
            .map_err(|e| Error::Materialise(format!("burner failed: {e}")))?;
        let used = budget - machine.ex_budget;
        Ok(Units {
            steps: u64::try_from(used.cpu).map_err(|_| not_affine())?,
            mem: u64::try_from(used.mem).map_err(|_| not_affine())?,
        })
    }

    /// Knobs whose cost is as close to `target` as the burner gets without
    /// going over it. A dimension that is `None` is kept as low as possible.
    /// Fails, naming what is reachable, when no knobs fit.
    pub fn solve(&self, steps: Option<u64>, mem: Option<u64>) -> Result<Knobs> {
        let max_words = BUFFER_BYTES / 8;
        let hash_max = self.hash.steps + self.hash_word_steps * (max_words - 1);
        let at_least = |what: &str, target: u64, floor: u64| -> Result<()> {
            if target < floor {
                return Err(Error::Materialise(format!(
                    "a script cost of {target} {what} is below the burner's floor of {floor}"
                )));
            }
            Ok(())
        };
        match (steps, mem) {
            (None, None) => Err(Error::Materialise("a cost needs cpu or mem".to_string())),
            (None, Some(mem)) => {
                at_least("memory units", mem, self.base.mem)?;
                let loops = (mem - self.base.mem) / self.count.mem;
                Ok(Knobs {
                    hashes: 0,
                    bytes: 1,
                    loops,
                })
            }
            (Some(steps), None) => {
                at_least("CPU steps", steps, self.base.steps)?;
                let extra = steps - self.base.steps;
                if extra < self.hash.steps {
                    return Ok(Knobs {
                        hashes: 0,
                        bytes: 1,
                        loops: extra / self.count.steps,
                    });
                }
                // The fewest hashes, each as long as needed.
                let hashes = extra.div_ceil(hash_max);
                let words = self.words_for(extra, hashes, max_words);
                Ok(Knobs {
                    hashes,
                    bytes: words * 8,
                    loops: 0,
                })
            }
            (Some(steps), Some(mem)) => {
                at_least("CPU steps", steps, self.base.steps)?;
                at_least("memory units", mem, self.base.mem)?;
                let extra_steps = steps - self.base.steps;
                let extra_mem = mem - self.base.mem;
                // More hashes leave less memory for counting; take the
                // fewest hashes whose length can carry the CPU left over.
                let mut hashes = 0;
                while hashes * self.hash.mem <= extra_mem {
                    let loops = (extra_mem - hashes * self.hash.mem) / self.count.mem;
                    let counted = loops * self.count.steps;
                    if counted <= extra_steps {
                        let left = extra_steps - counted;
                        if hashes == 0 {
                            if left < self.hash.steps {
                                return Ok(Knobs {
                                    hashes: 0,
                                    bytes: 1,
                                    loops,
                                });
                            }
                        } else if left >= hashes * self.hash.steps && left <= hashes * hash_max {
                            let words = self.words_for(left, hashes, max_words);
                            return Ok(Knobs {
                                hashes,
                                bytes: words * 8,
                                loops,
                            });
                        } else if left < hashes * self.hash.steps {
                            break;
                        }
                    }
                    hashes += 1;
                }
                let (low, high) = self.steps_range(extra_mem, hash_max);
                Err(Error::Materialise(format!(
                    "the burner cannot use {steps} CPU steps with {mem} memory units: with \
                     that much memory it uses between {} and {} steps",
                    self.base.steps + low,
                    self.base.steps + high
                )))
            }
        }
    }

    /// Words per hash so that `hashes` hashes use at most `steps` steps.
    fn words_for(&self, steps: u64, hashes: u64, max_words: u64) -> u64 {
        let per_hash = steps / hashes;
        let words = 1 + per_hash.saturating_sub(self.hash.steps) / self.hash_word_steps.max(1);
        words.clamp(1, max_words)
    }

    /// Steps the burner can use beyond its base with `extra_mem` memory:
    /// all of it counting, or as much as possible hashing the whole buffer.
    fn steps_range(&self, extra_mem: u64, hash_max: u64) -> (u64, u64) {
        let low = (extra_mem / self.count.mem) * self.count.steps;
        let hashes = extra_mem / self.hash.mem;
        let loops = (extra_mem - hashes * self.hash.mem) / self.count.mem;
        let high = hashes * hash_max + loops * self.count.steps;
        (low.min(high), high.max(low))
    }
}

/// Words a bytestring of `bytes` bytes counts as in the cost model.
fn words(bytes: u64) -> u64 {
    bytes.max(1).div_ceil(8)
}

fn not_affine() -> Error {
    Error::Materialise("the burner's cost under this cost model is not as expected".to_string())
}

/// CBOR head with major type `major` and argument `value`.
pub(crate) fn cbor_uint(out: &mut Vec<u8>, major: u8, value: u64) {
    let major = major << 5;
    match value {
        0..=23 => out.push(major | value as u8),
        24..=0xff => out.extend_from_slice(&[major | 24, value as u8]),
        0x100..=0xffff => {
            out.push(major | 25);
            out.extend_from_slice(&(value as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(major | 26);
            out.extend_from_slice(&(value as u32).to_be_bytes());
        }
        _ => {
            out.push(major | 27);
            out.extend_from_slice(&value.to_be_bytes());
        }
    }
}
