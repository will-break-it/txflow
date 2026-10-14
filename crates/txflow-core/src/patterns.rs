//! Pattern library: named, parameterised generators of IR fragments.
//!
//! Every pattern takes evaluated integer arguments plus the owning wallet and
//! emits sources and transactions with names relative to its instance prefix.
//! Arguments not listed for a pattern are rejected.

use std::collections::BTreeMap;

use crate::catalogue::{self, ArgKind};
use crate::error::{Error, Result};
use crate::ir::{Amount, AmountKeyword, OutputSpec, Source, Tx};

const ADA: u64 = 1_000_000;
/// Default value of a source that funds a chain or a single tx.
const DEFAULT_FUND: u64 = 10 * ADA;
/// Largest value a tx-count argument (`n`, `length`, `k`, `count`) may take.
pub const MAX_SIZE: u64 = 100_000;
/// Largest number of sources, txs, inputs and outputs an experiment may expand to.
pub const MAX_ELEMENTS: u64 = 5_000_000;
/// Largest number of inputs or outputs of one transaction a pattern may ask for.
pub const MAX_PER_TX: u64 = 1_000;
/// Fee budget per chained tx (a one-input, one-output tx is about 0.17 ADA
/// on mainnet-like parameters).
const STEP_FEE: u64 = ADA / 2;
/// Budget per extra minimum output of a chained tx.
const STEP_MIN_OUTPUT: u64 = 2 * ADA;
/// Default value of a shared conflict input.
const DEFAULT_CONFLICT_INPUT: u64 = 2 * ADA;

/// A pattern's output before namespacing.
#[derive(Debug, Default)]
pub struct Fragment {
    pub sources: BTreeMap<String, Source>,
    pub txs: BTreeMap<String, Tx>,
}

/// Arguments after evaluation. `get` consumes, so leftovers can be reported.
pub struct Args {
    pattern: String,
    values: BTreeMap<String, u64>,
}

impl Args {
    pub fn new(pattern: &str, values: BTreeMap<String, u64>) -> Args {
        Args {
            pattern: pattern.to_string(),
            values,
        }
    }

    /// The catalogue entry for argument `name` of this pattern. The catalogue
    /// is the one place argument names, ranges and defaults are written down;
    /// reading an argument it does not list is a bug in this file.
    fn info(&self, name: &str) -> Result<&'static ArgKind> {
        catalogue::find(&self.pattern)
            .and_then(|pattern| pattern.args.iter().find(|arg| arg.name == name))
            .map(|arg| &arg.kind)
            .ok_or_else(|| {
                Error::Spec(format!(
                    "internal: `{}` reads `{name}`, which the catalogue does not list",
                    self.pattern
                ))
            })
    }

    /// A count argument, with the range and default the catalogue gives it,
    /// so a typo cannot make expansion allocate without limit.
    fn count(&mut self, name: &str) -> Result<u64> {
        let (min, max, default) = match self.info(name)? {
            ArgKind::Count { min, max, default } => (*min, *max, *default),
            _ => {
                return Err(Error::Spec(format!(
                    "internal: `{}` argument `{name}` is not a count",
                    self.pattern
                )))
            }
        };
        let value = self.values.remove(name).unwrap_or(default);
        if value > max {
            return Err(Error::Spec(format!(
                "`{name}` is {value}, the maximum is {max}"
            )));
        }
        if value < min {
            return Err(Error::Spec(format!("`{name}` must be at least {min}")));
        }
        Ok(value)
    }

    /// An amount argument. Its default depends on other arguments, so the
    /// pattern computes it; the catalogue describes it in words.
    fn lovelace(&mut self, name: &str, default: u64) -> Result<u64> {
        match self.info(name)? {
            ArgKind::Lovelace { .. } => Ok(self.values.remove(name).unwrap_or(default)),
            _ => Err(Error::Spec(format!(
                "internal: `{}` argument `{name}` is not an amount",
                self.pattern
            ))),
        }
    }

    /// A choice argument: one of the words the catalogue lists, the first
    /// when left out.
    fn choice(&self, name: &str, given: Option<&str>) -> Result<&'static str> {
        let values = match self.info(name)? {
            ArgKind::Choice { values } => *values,
            _ => {
                return Err(Error::Spec(format!(
                    "internal: `{}` argument `{name}` is not a choice",
                    self.pattern
                )))
            }
        };
        let Some(given) = given else {
            return values
                .first()
                .copied()
                .ok_or_else(|| Error::Spec(format!("internal: `{name}` has no values")));
        };
        values.iter().copied().find(|v| *v == given).ok_or_else(|| {
            let listed: Vec<String> = values.iter().map(|v| format!("`{v}`")).collect();
            Error::Spec(format!(
                "`{name}` must be {}, got `{given}`",
                listed.join(" or ")
            ))
        })
    }

    fn finish(self) -> Result<()> {
        if let Some(name) = self.values.keys().next() {
            return Err(Error::Spec(format!(
                "`{}` has no argument `{name}`",
                self.pattern
            )));
        }
        Ok(())
    }
}

/// Non-numeric arguments (currently only `conflict`).
pub struct Flags {
    pub conflict: Option<String>,
}

/// Run pattern `name`.
pub fn generate(name: &str, mut args: Args, flags: &Flags, owner: &str) -> Result<Fragment> {
    let fragment = match name {
        "independent" => independent(&mut args, owner)?,
        "chain" => chain(&mut args, owner)?,
        "fan_out" => fan_out(&mut args, owner)?,
        "fan_in" => fan_in(&mut args, owner)?,
        "conflict_set" => conflict_set(&mut args, owner)?,
        "branch_merge" => branch_merge(&mut args, owner)?,
        "conflicting_chains" => conflicting_chains(&mut args, flags, owner)?,
        other => return Err(Error::Spec(format!("unknown pattern `{other}`"))),
    };
    if flags.conflict.is_some() && name != "conflicting_chains" {
        return Err(Error::Spec(format!("`{name}` has no argument `conflict`")));
    }
    args.finish()?;
    Ok(fragment)
}

fn change_to(owner: &str) -> OutputSpec {
    OutputSpec {
        to: owner.to_string(),
        lovelace: Amount::Keyword(AmountKeyword::Change),
    }
}

fn min_to(owner: &str) -> OutputSpec {
    OutputSpec {
        to: owner.to_string(),
        lovelace: Amount::Keyword(AmountKeyword::Min),
    }
}

/// Change first, then `outs - 1` minimum outputs.
fn outputs(owner: &str, outs: u64) -> Vec<OutputSpec> {
    let mut list = vec![change_to(owner)];
    for _ in 1..outs {
        list.push(min_to(owner));
    }
    list
}

fn source(owner: &str, lovelace: u64) -> Source {
    Source {
        owner: owner.to_string(),
        lovelace,
    }
}

/// Reject `txs` single-input txs with `outs` outputs each, plus a source
/// per tx (an upper bound on what [`independent`] and [`chain`] allocate),
/// above [`MAX_ELEMENTS`] before allocating anything.
fn require_elements(txs: u64, outs: u64) -> Result<()> {
    if txs.saturating_mul(outs.saturating_add(3)) > MAX_ELEMENTS {
        return Err(Error::Spec(format!(
            "{txs} txs with {outs} outputs each would exceed {MAX_ELEMENTS} graph elements"
        )));
    }
    Ok(())
}

/// Default funding for a chain of `length` txs (1 for a single tx) with
/// `outs` outputs each:
/// [`DEFAULT_FUND`], or enough for every step's fee and minimum outputs
/// when that is more.
fn chain_fund(length: u64, outs: u64) -> u64 {
    let per_step = STEP_FEE + outs.saturating_sub(1) * STEP_MIN_OUTPUT;
    DEFAULT_FUND.max(length.saturating_mul(per_step).saturating_add(2 * ADA))
}

/// `n` unrelated single transactions.
fn independent(args: &mut Args, owner: &str) -> Result<Fragment> {
    let n = args.count("n")?;
    let outs = args.count("outs")?;
    require_elements(n, outs)?;
    let lovelace = args.lovelace("lovelace", chain_fund(1, outs))?;
    let mut fragment = Fragment::default();
    for i in 1..=n {
        fragment
            .sources
            .insert(format!("s{i}"), source(owner, lovelace));
        fragment.txs.insert(
            format!("t{i}"),
            Tx {
                inputs: vec![format!("s{i}")],
                reference_inputs: vec![],
                outputs: outputs(owner, outs),
                labels: vec![],
                ..Tx::default()
            },
        );
    }
    Ok(fragment)
}

/// `t1 → t2 → … → tN`, each spending the previous change output.
fn chain(args: &mut Args, owner: &str) -> Result<Fragment> {
    let length = args.count("length")?;
    let outs = args.count("outs")?;
    require_elements(length, outs)?;
    let lovelace = args.lovelace("lovelace", chain_fund(length, outs))?;
    let mut fragment = Fragment::default();
    fragment
        .sources
        .insert("s".to_string(), source(owner, lovelace));
    for i in 1..=length {
        let input = if i == 1 {
            "s".to_string()
        } else {
            format!("t{}.0", i - 1)
        };
        fragment.txs.insert(
            format!("t{i}"),
            Tx {
                inputs: vec![input],
                reference_inputs: vec![],
                outputs: outputs(owner, outs),
                labels: vec![],
                ..Tx::default()
            },
        );
    }
    Ok(fragment)
}

/// One `root` tx with `width` outputs, each spent by a child `c<i>`.
fn fan_out(args: &mut Args, owner: &str) -> Result<Fragment> {
    let width = args.count("width")?;
    let per_branch = args.lovelace("lovelace", 3 * ADA)?;
    let total = per_branch
        .checked_mul(width + 1)
        .ok_or_else(|| Error::Spec("value overflow".to_string()))?;
    let mut fragment = Fragment::default();
    fragment
        .sources
        .insert("s".to_string(), source(owner, total));
    // Outputs 0..width-1 carry `per_branch`; the last branch takes the change.
    let mut root_outputs = Vec::new();
    for _ in 1..width {
        root_outputs.push(OutputSpec {
            to: owner.to_string(),
            lovelace: Amount::Fixed(per_branch),
        });
    }
    root_outputs.push(change_to(owner));
    fragment.txs.insert(
        "root".to_string(),
        Tx {
            inputs: vec!["s".to_string()],
            reference_inputs: vec![],
            outputs: root_outputs,
            labels: vec![],
            ..Tx::default()
        },
    );
    for i in 0..width {
        fragment.txs.insert(
            format!("c{}", i + 1),
            Tx {
                inputs: vec![format!("root.{i}")],
                reference_inputs: vec![],
                outputs: vec![change_to(owner)],
                labels: vec![],
                ..Tx::default()
            },
        );
    }
    Ok(fragment)
}

/// `width` leaf txs whose change outputs one `merge` tx spends.
fn fan_in(args: &mut Args, owner: &str) -> Result<Fragment> {
    let width = args.count("width")?;
    let lovelace = args.lovelace("lovelace", 3 * ADA)?;
    let mut fragment = Fragment::default();
    let mut merge_inputs = Vec::new();
    for i in 1..=width {
        fragment
            .sources
            .insert(format!("s{i}"), source(owner, lovelace));
        fragment.txs.insert(
            format!("l{i}"),
            Tx {
                inputs: vec![format!("s{i}")],
                reference_inputs: vec![],
                outputs: vec![change_to(owner)],
                labels: vec![],
                ..Tx::default()
            },
        );
        merge_inputs.push(format!("l{i}.0"));
    }
    fragment.txs.insert(
        "merge".to_string(),
        Tx {
            inputs: merge_inputs,
            reference_inputs: vec![],
            outputs: vec![change_to(owner)],
            labels: vec![],
            ..Tx::default()
        },
    );
    Ok(fragment)
}

/// `k` txs `x<i>` that all spend source `s`.
fn conflict_set(args: &mut Args, owner: &str) -> Result<Fragment> {
    let k = args.count("k")?;
    let lovelace = args.lovelace("lovelace", DEFAULT_FUND)?;
    let mut fragment = Fragment::default();
    fragment
        .sources
        .insert("s".to_string(), source(owner, lovelace));
    for i in 1..=k {
        fragment.txs.insert(
            format!("x{i}"),
            Tx {
                inputs: vec!["s".to_string()],
                reference_inputs: vec![],
                outputs: vec![change_to(owner)],
                labels: vec![],
                ..Tx::default()
            },
        );
    }
    Ok(fragment)
}

/// A `fan_out` whose children one `merge` tx joins again: every branch shares
/// `root` as an ancestor, so `merge` depends on all of them.
fn branch_merge(args: &mut Args, owner: &str) -> Result<Fragment> {
    let mut fragment = fan_out(args, owner)?;
    let children = fragment.txs.len() - 1;
    fragment.txs.insert(
        "merge".to_string(),
        Tx {
            inputs: (1..=children).map(|i| format!("c{i}.0")).collect(),
            reference_inputs: vec![],
            outputs: vec![change_to(owner)],
            labels: vec![],
            ..Tx::default()
        },
    );
    Ok(fragment)
}

/// Chains `a1 → … → aN` and `b1 → … → bN`, funded by `wa` and `wb`.
///
/// `conflict: per_step` (default) adds a shared input `c<i>` to both `a<i>`
/// and `b<i>`, so every pair conflicts. `conflict: root` adds only `c1`, so
/// later txs are orphaned by the first conflict rather than conflicting.
fn conflicting_chains(args: &mut Args, flags: &Flags, owner: &str) -> Result<Fragment> {
    let length = args.count("length")?;
    let conflict_lovelace = args.lovelace("conflict_lovelace", DEFAULT_CONFLICT_INPUT)?;
    let per_step = args.choice("conflict", flags.conflict.as_deref())? == "per_step";
    // With `per_step`, each step's conflict input pays that step's fee.
    let lovelace = args.lovelace(
        "lovelace",
        if per_step {
            DEFAULT_FUND
        } else {
            chain_fund(length, 1)
        },
    )?;
    let mut fragment = Fragment::default();
    for side in ["a", "b"] {
        fragment
            .sources
            .insert(format!("w{side}"), source(owner, lovelace));
    }
    for i in 1..=length {
        let shared = per_step || i == 1;
        if shared {
            fragment
                .sources
                .insert(format!("c{i}"), source(owner, conflict_lovelace));
        }
        for side in ["a", "b"] {
            let mut inputs = vec![if i == 1 {
                format!("w{side}")
            } else {
                format!("{side}{}.0", i - 1)
            }];
            if shared {
                inputs.push(format!("c{i}"));
            }
            fragment.txs.insert(
                format!("{side}{i}"),
                Tx {
                    inputs,
                    reference_inputs: vec![],
                    outputs: vec![change_to(owner)],
                    labels: vec![format!("side-{side}")],
                    ..Tx::default()
                },
            );
        }
    }
    Ok(fragment)
}
