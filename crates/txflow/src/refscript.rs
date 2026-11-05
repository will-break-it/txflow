//! Reference scripts (CIP-33) that outputs carry, and the fee for reading them.
//!
//! A spec asks for a reference script by size only (`reference_script: { bytes:
//! N }`). Materialise makes it a Plutus V3 script of exactly N bytes, as the
//! ledger counts a script's size for the reference-script fee: the program
//! `(program 1.1.0 (delay ... (con bytestring #…)))`, with the bytestring's
//! length and the number of `delay`s chosen so the script's bytes come to N,
//! and its bytes starting with a tag that tells outputs' scripts apart. It is well
//! formed, so the ledger accepts it in an output; nothing ever runs it.
//!
//! A tx that spends or reads outputs with reference scripts pays the Conway
//! reference-script fee on top of the size fee: `minFeeRefScriptCostPerByte`
//! per byte for the first 25 KiB of scripts, 1.2 times that for the next
//! 25 KiB, and so on, rounded down once at the end.

use std::collections::BTreeMap;
use std::rc::Rc;

use num_bigint::BigUint;
use uplc::ast::{Constant, DeBruijn, Program, Term};

use crate::context::{ProtocolParams, Rational};
use crate::error::{Error, Result};
use txflow_core::refscript::MAX_BYTES_PER_TX;

/// Conway's bytes of reference scripts per fee tier.
const STRIDE: u64 = 25_600;
/// Conway's factor from one tier's price per byte to the next's.
const MULTIPLIER: Rational = Rational {
    numerator: 6,
    denominator: 5,
};
/// The most `delay`s the generator wraps the constant in. Four bits each, so
/// eight cover any two bytes the constant's length cannot.
const MAX_DELAYS: usize = 8;

/// A Plutus V3 script of exactly `bytes` bytes (the bytes that go in a
/// `[3, bytes]` script reference, CBOR-wrapped flat), its bytestring starting
/// with as much of `tag` as fits. The same size and tag always give the same
/// script; outputs given different tags get different scripts (hashes), as
/// they would on a real network. Scripts under about 40 bytes hold only part
/// of the tag, so different tags can give the same one; at 8 and 9 bytes
/// they all do.
pub fn script_of_size(bytes: u32, tag: &[u8]) -> Result<Vec<u8>> {
    Scripts::default().make(bytes, tag)
}

/// Makes scripts as [`script_of_size`] does, searching for each size's
/// shape only once.
#[derive(Default)]
pub struct Scripts {
    shapes: BTreeMap<u32, (usize, usize)>,
}

impl Scripts {
    pub fn make(&mut self, bytes: u32, tag: &[u8]) -> Result<Vec<u8>> {
        let (payload, delays) = match self.shapes.get(&bytes) {
            Some(shape) => *shape,
            None => {
                let shape = shape_of(bytes)?;
                self.shapes.insert(bytes, shape);
                shape
            }
        };
        let mut constant = vec![0; payload];
        let n = tag.len().min(payload);
        constant[..n].copy_from_slice(&tag[..n]);
        // The encoding's size depends on the bytestring's length, not its bytes.
        let script = encode(constant, delays)?;
        debug_assert_eq!(script.len(), bytes as usize);
        Ok(script)
    }
}

/// The bytestring length and `delay`s that make a script of `bytes` bytes.
fn shape_of(bytes: u32) -> Result<(usize, usize)> {
    txflow_core::refscript::ReferenceScript { bytes }
        .validate()
        .map_err(Error::Materialise)?;
    let target = bytes as usize;
    // The script is the payload plus a few bytes of framing: the program
    // version (3), the term's tags and the final padding (at most 3), one
    // length byte per 255-byte chunk and the end byte, and the CBOR header (at
    // most 5). So the payload lies within 32 bytes above the target less the
    // most framing. Search down from there, so the payload, which holds the
    // tag, is as long as it can be and the `delay`s make up the rest.
    let start = target.saturating_sub(target / 255 + 16);
    for payload in (start..=target.min(start + 32)).rev() {
        for delays in 0..=MAX_DELAYS {
            let size = encode(vec![0; payload], delays)?.len();
            if size == target {
                return Ok((payload, delays));
            }
            if size > target {
                break;
            }
        }
    }
    Err(no_script(bytes))
}

fn no_script(bytes: u32) -> Error {
    Error::Materialise(format!("no reference script of exactly {bytes} bytes"))
}

fn encode(constant: Vec<u8>, delays: usize) -> Result<Vec<u8>> {
    let mut term: Term<DeBruijn> = Term::Constant(Rc::new(Constant::ByteString(constant)));
    for _ in 0..delays {
        term = Term::Delay(Rc::new(term));
    }
    Program {
        version: (1, 1, 0),
        term,
    }
    .to_cbor()
    .map_err(|e| Error::Materialise(format!("encoding a reference script: {e}")))
}

/// How the network prices reference scripts: the price per byte of the first
/// tier, the bytes per tier, how much dearer each tier is than the one
/// before, and the most one tx may spend or read. Conway fixes the last three
/// (25 600 bytes, 1.2 and 204 800 bytes); Dijkstra makes them protocol
/// parameters, read when the context gives them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pricing {
    pub price: Rational,
    pub stride: u64,
    pub multiplier: Rational,
    pub max_per_tx: u64,
}

impl Pricing {
    pub fn of(pp: &ProtocolParams) -> Result<Pricing> {
        let price = pp
            .min_fee_ref_script_cost_per_byte
            .ok_or_else(missing_price)?;
        let stride = pp.ref_script_cost_stride.unwrap_or(STRIDE);
        let multiplier = pp.ref_script_cost_multiplier.unwrap_or(MULTIPLIER);
        if price.denominator == 0
            || multiplier.denominator == 0
            || multiplier.numerator == 0
            || stride == 0
        {
            return Err(Error::Context(
                "the reference-script fee parameters need a stride and a multiplier above 0, \
                 and fractions with non-zero denominators"
                    .to_string(),
            ));
        }
        Ok(Pricing {
            price,
            stride,
            multiplier,
            max_per_tx: pp.max_ref_script_size_per_tx.unwrap_or(MAX_BYTES_PER_TX),
        })
    }

    /// The fee for spending or reading `bytes` bytes of reference scripts,
    /// as the ledger computes it (`tierRefScriptFee`): exact fractions,
    /// rounded down once at the end.
    pub fn fee(&self, bytes: u64) -> Result<u64> {
        if bytes > self.max_per_tx {
            return Err(Error::Materialise(format!(
                "reads {bytes} bytes of reference scripts; a transaction may read at most {}",
                self.max_per_tx
            )));
        }
        // Closed form, so a small stride costs no more than a large one. With
        // the multiplier a/b, `full` whole tiers and `rem` bytes after them:
        //   fee = price · (stride · Σ_{i<full} (a/b)^i + rem · (a/b)^full)
        //       = price · (stride · b · G + rem · a^full) / b^full,
        // where G = Σ_{i<full} a^i b^(full-1-i) = (a^full - b^full) / (a - b).
        let big = |v: u64| BigUint::from(v);
        let (a, b) = (
            big(self.multiplier.numerator),
            big(self.multiplier.denominator),
        );
        let full = u32::try_from(bytes / self.stride)
            .map_err(|_| Error::Materialise("too many fee tiers".to_string()))?;
        let rem = big(bytes % self.stride);
        let (a_full, b_full) = (a.pow(full), b.pow(full));
        let geometric = if a == b {
            big(u64::from(full)) * b.pow(full.saturating_sub(1))
        } else if a > b {
            (&a_full - &b_full) / (&a - &b)
        } else {
            (&b_full - &a_full) / (&b - &a)
        };
        let acc = big(self.price.numerator) * (big(self.stride) * &b * geometric + rem * a_full);
        let den = big(self.price.denominator) * b_full;
        u64::try_from(acc / den)
            .map_err(|_| Error::Materialise("reference-script fee overflows".to_string()))
    }
}

/// The reference-script fee a tx pays for spending or reading `bytes` bytes
/// of scripts under `pp`; 0 for none, without needing the price.
pub fn fee_for(bytes: u64, pp: &ProtocolParams) -> Result<u64> {
    if bytes == 0 {
        return Ok(0);
    }
    Pricing::of(pp)?.fee(bytes)
}

/// The error for reference scripts without their price.
pub fn missing_price() -> Error {
    Error::Context(
        "reference scripts need `minFeeRefScriptCostPerByte` in the protocol parameters \
         (`cardano-cli query protocol-parameters` prints it)"
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use txflow_core::refscript::{MIN_BYTES, UNREACHABLE};

    #[test]
    fn every_size_from_the_smallest_is_exact() {
        let sizes = (MIN_BYTES..1_200).chain([4_096, 12_000, 16_384, 65_535, 65_538, 65_541]);
        for bytes in sizes.filter(|b| !UNREACHABLE.contains(b)) {
            let script = script_of_size(bytes, b"tag").expect("a script");
            assert_eq!(script.len(), bytes as usize);
        }
    }

    #[test]
    fn below_the_smallest_and_the_gaps_are_refused() {
        assert!(script_of_size(MIN_BYTES - 1, b"").is_err());
        for bytes in UNREACHABLE {
            assert!(script_of_size(bytes, b"").is_err(), "{bytes}");
        }
    }

    #[test]
    fn the_script_decodes_as_a_v3_program() {
        let script = script_of_size(3_000, b"tag").expect("a script");
        let mut buffer = Vec::new();
        let program = Program::<DeBruijn>::from_cbor(&script, &mut buffer).expect("decodes");
        assert_eq!(program.version, (1, 1, 0));
    }

    #[test]
    fn different_tags_give_different_scripts_of_the_same_size() {
        let tag = [0xff; 32];
        for bytes in [MIN_BYTES, 9, 40, 300, 15_000] {
            let a = script_of_size(bytes, b"a").expect("a script");
            let b = script_of_size(bytes, b"b").expect("a script");
            let long = script_of_size(bytes, &tag).expect("a script");
            assert_eq!((a.len(), long.len()), (bytes as usize, bytes as usize));
            if bytes > 9 {
                assert_ne!(a, b, "{bytes}");
            }
        }
    }

    fn conway() -> ProtocolParams {
        ProtocolParams {
            min_fee_ref_script_cost_per_byte: Some(Rational {
                numerator: 15,
                denominator: 1,
            }),
            ..ProtocolParams::default()
        }
    }

    #[test]
    fn the_fee_is_tiered_as_the_ledger_has_it() {
        let pp = conway();
        let fee = |bytes| fee_for(bytes, &pp).unwrap();
        assert_eq!(fee(0), 0);
        assert_eq!(fee(1_000), 15_000);
        assert_eq!(fee(25_600), 384_000);
        // 25 600 at 15, then 1 000 at 18.
        assert_eq!(fee(26_600), 402_000);
        // 25 600 at 15, 25 600 at 18, then 100 at 21.6.
        assert_eq!(fee(51_300), 384_000 + 460_800 + 2_160);
        assert!(fee_for(MAX_BYTES_PER_TX + 1, &pp).is_err());
        assert!(fee_for(1, &ProtocolParams::default()).is_err());
        assert_eq!(fee_for(0, &ProtocolParams::default()).unwrap(), 0);
    }

    #[test]
    fn dijkstra_parameters_replace_the_conway_constants() {
        let pp = ProtocolParams {
            ref_script_cost_stride: Some(1_000),
            ref_script_cost_multiplier: Some(Rational {
                numerator: 2,
                denominator: 1,
            }),
            max_ref_script_size_per_tx: Some(3_000),
            ..conway()
        };
        // 1 000 at 15, 1 000 at 30, 500 at 60.
        assert_eq!(fee_for(2_500, &pp).unwrap(), 15_000 + 30_000 + 30_000);
        assert!(fee_for(3_001, &pp).is_err());
    }

    /// The ledger's loop, `tierRefScriptFee`, in exact fractions.
    fn ledger(bytes: u64, price: Rational, stride: u64, m: Rational) -> u64 {
        let big = |v: u64| BigUint::from(v);
        let (mut num, mut den) = (big(price.numerator), big(price.denominator));
        let (mut acc, mut left) = (BigUint::ZERO, bytes);
        while left >= stride {
            acc += big(stride) * &num;
            left -= stride;
            num *= big(m.numerator);
            den *= big(m.denominator);
            acc *= big(m.denominator);
        }
        acc += big(left) * &num;
        u64::try_from(acc / den).unwrap()
    }

    #[test]
    fn the_closed_form_is_the_ledger_loop() {
        let r = |n, d| Rational {
            numerator: n,
            denominator: d,
        };
        for (price, stride, m) in [
            (r(15, 1), 25_600, r(6, 5)),
            (r(15, 1), 1_000, r(2, 1)),
            (r(44, 3), 777, r(1, 1)),
            (r(15, 1), 300, r(4, 5)),
            (r(7, 2), 1, r(11, 10)),
        ] {
            let pp = ProtocolParams {
                min_fee_ref_script_cost_per_byte: Some(price),
                ref_script_cost_stride: Some(stride),
                ref_script_cost_multiplier: Some(m),
                ..ProtocolParams::default()
            };
            for bytes in [
                0,
                1,
                stride - 1,
                stride,
                stride + 1,
                3 * stride,
                3 * stride + 5,
                4_321,
            ] {
                if bytes > 400 && stride == 1 {
                    continue;
                }
                assert_eq!(
                    fee_for(bytes, &pp).unwrap(),
                    ledger(bytes, price, stride, m),
                    "{bytes} bytes, stride {stride}"
                );
            }
        }
    }

    #[test]
    fn a_zero_multiplier_is_refused() {
        let pp = ProtocolParams {
            ref_script_cost_multiplier: Some(Rational {
                numerator: 0,
                denominator: 1,
            }),
            ..conway()
        };
        assert!(fee_for(1, &pp).is_err());
    }

    #[test]
    fn a_stride_of_one_is_quick() {
        let pp = ProtocolParams {
            ref_script_cost_stride: Some(1),
            ref_script_cost_multiplier: Some(Rational {
                numerator: 1,
                denominator: 1,
            }),
            ..conway()
        };
        assert_eq!(
            fee_for(MAX_BYTES_PER_TX, &pp).unwrap(),
            15 * MAX_BYTES_PER_TX
        );
    }

    #[test]
    fn many_tiers_do_not_overflow() {
        // 52 tiers of 4 000 bytes at 1.2 each: 6^51 is past u128.
        let pp = ProtocolParams {
            ref_script_cost_stride: Some(4_000),
            ..conway()
        };
        let fee = fee_for(MAX_BYTES_PER_TX, &pp).expect("a fee");
        // Each tier costs 4 000 * 15 * 1.2^k; the sum is about 2.5e9.
        let exact: f64 = (0..51).map(|k| 60_000.0 * 1.2f64.powi(k)).sum::<f64>()
            + 800.0 * 15.0 * 1.2f64.powi(51);
        assert!((fee as f64 - exact).abs() < 2.0, "{fee} vs {exact}");
    }
}
