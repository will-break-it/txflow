//! What each pattern does and which arguments it takes, in words.
//!
//! This is the reference a person (or a language model) reads to write a
//! spec, and the source the JSON Schema is generated from. Tests check that
//! every pattern and argument listed here is one [`crate::patterns`] accepts.

use crate::patterns::{MAX_PER_TX, MAX_SIZE};

/// One pattern: its name, what it builds, and its arguments.
pub struct PatternInfo {
    pub name: &'static str,
    pub summary: &'static str,
    /// The transactions and sources it emits, by name, inside its namespace.
    pub emits: &'static str,
    pub args: &'static [ArgInfo],
    /// Example `with:` block, as YAML flow mapping text.
    pub example: &'static str,
}

/// One argument of a pattern.
pub struct ArgInfo {
    pub name: &'static str,
    pub kind: ArgKind,
    pub summary: &'static str,
}

pub enum ArgKind {
    /// A whole number in `min..=max`, with a default when left out.
    Count { min: u64, max: u64, default: u64 },
    /// An amount of lovelace; the default is described in words.
    Lovelace { default: &'static str },
    /// One of a fixed set of words; the first is the default.
    Choice { values: &'static [&'static str] },
    /// The name of a wallet declared in the spec.
    Wallet,
}

/// Arguments every pattern takes besides its own.
pub const COMMON_ARGS: &[ArgInfo] = &[ArgInfo {
    name: "wallet",
    kind: ArgKind::Wallet,
    summary: "Wallet that owns everything the pattern creates. Required when the spec \
              declares more than one wallet.",
}];

const fn count(min: u64, max: u64, default: u64) -> ArgKind {
    ArgKind::Count { min, max, default }
}

pub const PATTERNS: &[PatternInfo] = &[
    PatternInfo {
        name: "independent",
        summary: "`n` unrelated transactions, each funded by its own source. A baseline \
                  load with no dependencies or conflicts.",
        emits: "sources s1..sN; txs t1..tN (t<i> spends s<i>)",
        args: &[
            ArgInfo {
                name: "n",
                kind: count(1, MAX_SIZE, 1),
                summary: "Number of transactions.",
            },
            ArgInfo {
                name: "outs",
                kind: count(1, MAX_PER_TX, 1),
                summary: "Outputs per transaction: one change output plus `outs - 1` \
                          minimum-value outputs.",
            },
            ArgInfo {
                name: "lovelace",
                kind: ArgKind::Lovelace {
                    default: "10 ADA, or more when `outs` needs it",
                },
                summary: "Value of each source.",
            },
        ],
        example: "{ n: 100, outs: 2 }",
    },
    PatternInfo {
        name: "chain",
        summary: "A chain t1 → t2 → … → tN where each transaction spends the previous \
                  one's change output. Tests how nodes handle dependent transactions.",
        emits: "source s; txs t1..tN (t1 spends s, t<i> spends t<i-1>.0)",
        args: &[
            ArgInfo {
                name: "length",
                kind: count(1, MAX_SIZE, 2),
                summary: "Number of transactions in the chain.",
            },
            ArgInfo {
                name: "outs",
                kind: count(1, MAX_PER_TX, 1),
                summary: "Outputs per transaction: one change output plus `outs - 1` \
                          minimum-value outputs.",
            },
            ArgInfo {
                name: "lovelace",
                kind: ArgKind::Lovelace {
                    default: "10 ADA, or enough for every step's fee when the chain is long",
                },
                summary: "Value of the source that funds the chain.",
            },
        ],
        example: "{ length: 10 }",
    },
    PatternInfo {
        name: "fan_out",
        summary: "One transaction `root` with `width` outputs, each spent by its own child \
                  transaction. One parent, many children.",
        emits: "source s; txs root, c1..cW (c<i> spends root.<i-1>)",
        args: &[
            ArgInfo {
                name: "width",
                kind: count(2, MAX_PER_TX, 2),
                summary: "Number of outputs of `root`, and of children.",
            },
            ArgInfo {
                name: "lovelace",
                kind: ArgKind::Lovelace {
                    default: "3 ADA per branch",
                },
                summary: "Value of each branch; the last branch also gets the leftover.",
            },
        ],
        example: "{ width: 5 }",
    },
    PatternInfo {
        name: "fan_in",
        summary: "`width` independent leaf transactions whose change outputs are all spent \
                  by one `merge` transaction. Many parents, one child.",
        emits: "sources s1..sW; txs l1..lW, merge (merge spends every l<i>.0)",
        args: &[
            ArgInfo {
                name: "width",
                kind: count(2, MAX_PER_TX, 2),
                summary: "Number of leaf transactions merged.",
            },
            ArgInfo {
                name: "lovelace",
                kind: ArgKind::Lovelace {
                    default: "3 ADA per leaf",
                },
                summary: "Value of each leaf's source.",
            },
        ],
        example: "{ width: 4 }",
    },
    PatternInfo {
        name: "conflict_set",
        summary: "`k` transactions that all spend the same source, so at most one of them \
                  can land: a k-way double spend.",
        emits: "source s; txs x1..xK (every x<i> spends s)",
        args: &[
            ArgInfo {
                name: "k",
                kind: count(2, MAX_SIZE, 2),
                summary: "Number of competing transactions.",
            },
            ArgInfo {
                name: "lovelace",
                kind: ArgKind::Lovelace { default: "10 ADA" },
                summary: "Value of the contested source.",
            },
        ],
        example: "{ k: 3 }",
    },
    PatternInfo {
        name: "branch_merge",
        summary: "`root` branches into `width` children, which one `merge` transaction joins \
                  again. Every branch shares `root` as an ancestor, so `merge` depends on all \
                  of them: a `fan_out` closed by a merge.",
        emits: "source s; txs root, c1..cW, merge (merge spends every c<i>.0)",
        args: &[
            ArgInfo {
                name: "width",
                kind: count(2, MAX_PER_TX, 2),
                summary: "Number of branches.",
            },
            ArgInfo {
                name: "lovelace",
                kind: ArgKind::Lovelace {
                    default: "3 ADA per branch",
                },
                summary: "Value of each branch; the last branch also gets the leftover.",
            },
        ],
        example: "{ width: 2 }",
    },
    PatternInfo {
        name: "conflicting_chains",
        summary: "Two chains a1 → … → aN and b1 → … → bN that compete. With `per_step` \
                  every pair aᵢ, bᵢ spends a shared input cᵢ, so each pair conflicts. With \
                  `root` only a1 and b1 conflict and the later transactions are orphaned \
                  by whichever side loses.",
        emits: "sources wa, wb (fund each side), c1..cN (shared inputs; only c1 with \
                `root`); txs a1..aN, b1..bN, labelled side-a and side-b",
        args: &[
            ArgInfo {
                name: "length",
                kind: count(1, MAX_SIZE, 3),
                summary: "Length of each chain.",
            },
            ArgInfo {
                name: "conflict",
                kind: ArgKind::Choice {
                    values: &["per_step", "root"],
                },
                summary: "`per_step`: every pair conflicts. `root`: only the first pair \
                          conflicts.",
            },
            ArgInfo {
                name: "lovelace",
                kind: ArgKind::Lovelace {
                    default: "10 ADA, or enough for every step's fee when `root` chains are long",
                },
                summary: "Value of each side's funding source.",
            },
            ArgInfo {
                name: "conflict_lovelace",
                kind: ArgKind::Lovelace { default: "2 ADA" },
                summary: "Value of each shared input cᵢ.",
            },
        ],
        example: "{ length: 3, conflict: per_step }",
    },
    PatternInfo {
        name: "asset_shuffle",
        summary: "UTxOs full of native assets, merged and split again. `mint<i>` mints \
                  `assets` unique assets into one UTxO each; `merge<r>` spends them all into \
                  one UTxO holding every asset (fan-in), and `split<r>` pays them back out \
                  into `utxos` UTxOs with the assets shuffled (fan-out), `rounds` times. \
                  The assets are spread over `policies` time-lock policies, each its own \
                  policy id. Tests how the ledger handles large multi-asset values: the \
                  merged UTxO holds `utxos × assets` assets, and the network's \
                  maxValueSize caps it: 5000 bytes on Musashi, 844 assets of one policy \
                  or 137 policies of one asset.",
        emits: "policies p1..pP; sources s1..sU; txs mint1..mintU (mint<i> spends s<i>), \
                merge1..mergeR (merge<r> spends every UTxO of the round before), \
                split1..splitR (split<r> spends merge<r>.0 and pays `utxos` outputs), and \
                burn with `burn: all`; assets a1..aN, a<j> under p<(j-1) % P + 1>",
        args: &[
            ArgInfo {
                name: "utxos",
                kind: count(1, 100, 2),
                summary: "UTxOs minted, and split back into each round.",
            },
            ArgInfo {
                name: "assets",
                kind: count(1, 1_000, 5),
                summary: "Unique assets each UTxO holds; the merged UTxO holds \
                          `utxos × assets`.",
            },
            ArgInfo {
                name: "policies",
                kind: count(1, 1_000, 1),
                summary: "Distinct minting policies the assets are spread over, round \
                          robin. Each is a native script valid after its own slot (1, 2, 3, \
                          …), so each has its own policy id.",
            },
            ArgInfo {
                name: "rounds",
                kind: count(1, 100, 1),
                summary: "Merge-and-split rounds; each round shuffles differently.",
            },
            ArgInfo {
                name: "quantity",
                kind: count(1, 1_000_000_000_000, 1),
                summary: "How many of each asset are minted and moved.",
            },
            ArgInfo {
                name: "burn",
                kind: ArgKind::Choice {
                    values: &["none", "all"],
                },
                summary: "`all`: a last tx `burn` spends every UTxO and burns every asset, \
                          leaving ADA only. `none`: the assets stay.",
            },
            ArgInfo {
                name: "lovelace",
                kind: ArgKind::Lovelace {
                    default: "enough for the fees and every UTxO's min-UTxO with its assets",
                },
                summary: "Value of each UTxO's source.",
            },
        ],
        example: "{ utxos: 2, assets: 5, policies: 3 }",
    },
];

/// The catalogue entry for `name`.
pub fn find(name: &str) -> Option<&'static PatternInfo> {
    PATTERNS.iter().find(|p| p.name == name)
}

/// The catalogue as Markdown: one section per pattern with its arguments.
pub fn markdown() -> String {
    let mut out = format!(
        "# Patterns (spec version {})\n\n\
         Generated from `crates/txflow-core/src/catalogue.rs`; do not edit.\n\n",
        crate::spec::VERSION
    );
    for pattern in PATTERNS {
        out.push_str(&format!(
            "### `{}`\n\n{}\n\n",
            pattern.name, pattern.summary
        ));
        out.push_str(&format!("Emits: {}.\n\n", pattern.emits));
        for arg in pattern.args.iter().chain(COMMON_ARGS) {
            let kind = match &arg.kind {
                ArgKind::Count { min, max, default } => {
                    format!("whole number {min}..{max}, default {default}")
                }
                ArgKind::Lovelace { default } => format!("lovelace, default {default}"),
                ArgKind::Choice { values } => format!(
                    "one of {}, default `{}`",
                    values
                        .iter()
                        .map(|v| format!("`{v}`"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    values.first().unwrap_or(&"")
                ),
                ArgKind::Wallet => "wallet name".to_string(),
            };
            out.push_str(&format!("- `{}` ({kind}): {}\n", arg.name, arg.summary));
        }
        out.push_str(&format!(
            "\nExample: `- {{ id: {0}, use: {0}, with: {1} }}`\n\n",
            pattern.name, pattern.example
        ));
    }
    out
}
