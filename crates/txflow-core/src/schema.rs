//! JSON Schema for experiment specs, generated from the [`crate::catalogue`].
//!
//! The schema checks shape only: field names, types, pattern names, argument
//! ranges and name spelling. Whether the graph makes sense (references that
//! resolve, no double spend of an ancestor's output, size limits) is checked by
//! [`crate::expand::expand`]. A spec that passes both is runnable.

use serde_json::{json, Map, Value};

use crate::catalogue::{ArgKind, PatternInfo, COMMON_ARGS, PATTERNS};
use crate::metadata::{COLOR_LABEL, MAX_STRING_BYTES, MESSAGE_LABEL};
use crate::spec::VERSION;

/// Spelling of wallet, source, tx, pattern and param names.
pub const NAME: &str = "^[a-z][a-z0-9_]*$";
/// A reference to a source or to output N of a tx, possibly inside a
/// pattern's namespace (`chains/a1.0`, `p_2/s`).
pub const REF: &str = "^[a-z][a-z0-9_]*(/[a-z][a-z0-9_]*)*(\\.(0|[1-9][0-9]*))?$";
/// A time offset: `0ms`, `250ms`, `2s`.
pub const DURATION: &str = "^(0|[0-9]+(ms|s))$";
/// An argument written as a number in a string, or as `{{ expression }}`.
const ARG_TEXT: &str = "^\\s*([0-9]+|\\{\\{[^{}]*\\}\\})\\s*$";

/// Where the schema for [`VERSION`] is committed, relative to the repository root.
pub fn committed_path() -> String {
    format!("schema/v{VERSION}/txflow.schema.json")
}

/// Where the pattern catalogue for [`VERSION`] is committed as Markdown.
pub fn committed_catalogue_path() -> String {
    format!("schema/v{VERSION}/catalogue.md")
}

/// The schema as a JSON value (draft 2020-12).
pub fn schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": format!("https://txflow.fun/schema/v{VERSION}.json"),
        "title": "txflow experiment",
        "description": "A declarative transaction-flow experiment: the UTxO graph to create, \
            with no transaction ids. Write the shape with `patterns`; use `sources` and `txs` \
            only for shapes no pattern covers.",
        "type": "object",
        "additionalProperties": false,
        "required": ["version", "experiment", "seed", "wallets"],
        "properties": {
            "version": {
                "const": VERSION,
                "description": format!("Format version. Always {VERSION}.")
            },
            "experiment": {
                "type": "string",
                "minLength": 1,
                "description": "Short name of the experiment."
            },
            "seed": {
                "type": "integer",
                "minimum": 0,
                "description": "Any number. Rerunning with the same seed gives the same transactions."
            },
            "params": {
                "type": "object",
                "description": "Named integers that arguments can use as `{{ name }}`, so one \
                    number scales the experiment.",
                "propertyNames": { "pattern": NAME },
                "additionalProperties": { "type": "integer" }
            },
            "wallets": {
                "type": "object",
                "description": "Symbolic owners of outputs. Usually one, e.g. `{ alice: {} }`.",
                "minProperties": 1,
                "propertyNames": { "pattern": NAME },
                "additionalProperties": {
                    "type": "object",
                    "additionalProperties": false
                }
            },
            "patterns": {
                "type": "array",
                "description": "Pattern instances. Each emits named sources and txs under \
                    `<id>/`, or `<id>_<k>/` for each copy when `count` is written.",
                "items": { "$ref": "#/$defs/pattern" }
            },
            "sources": {
                "type": "object",
                "description": "Hand-written funding outputs, by name.",
                "propertyNames": { "pattern": NAME },
                "additionalProperties": { "$ref": "#/$defs/source" }
            },
            "txs": {
                "type": "object",
                "description": "Hand-written transactions, by name. They may spend pattern \
                    outputs, e.g. `chains/a1.0`.",
                "propertyNames": { "pattern": NAME },
                "additionalProperties": { "$ref": "#/$defs/tx" }
            },
            "metadata": {
                "$ref": "#/$defs/metadata",
                "description": "Transaction metadata on every tx, by label. A pattern's or a \
                    tx's own `metadata` replaces it label by label."
            },
            "color": {
                "$ref": "#/$defs/color",
                "description": "Colour tag on every tx, unless a pattern or tx sets its own."
            },
            "cost": {
                "$ref": "#/$defs/cost_covering",
                "description": "Script cost of every tx (or of those with `select.label`), \
                    unless a pattern or tx sets its own."
            },
            "schedule": { "$ref": "#/$defs/schedule" },
            "expect": {
                "description": "Expected outcomes. Carried, not yet interpreted."
            }
        },
        "$defs": defs()
    })
}

fn defs() -> Value {
    let mut defs = Map::new();
    defs.insert(
        "pattern".to_string(),
        json!({
            "oneOf": PATTERNS.iter().map(|p| json!({ "$ref": format!("#/$defs/pattern_{}", p.name) }))
                .collect::<Vec<_>>()
        }),
    );
    for pattern in PATTERNS {
        defs.insert(format!("pattern_{}", pattern.name), pattern_schema(pattern));
    }
    defs.insert(
        "arg_text".to_string(),
        json!({
            "type": "string",
            "pattern": ARG_TEXT,
            "description": "A number, or `{{ expression }}` over `params` with + - * / and parentheses."
        }),
    );
    defs.insert(
        "source".to_string(),
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["owner", "lovelace"],
            "properties": {
                "owner": { "type": "string", "pattern": NAME },
                "lovelace": { "type": "integer", "minimum": 1 }
            }
        }),
    );
    defs.insert(
        "tx".to_string(),
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["inputs", "outputs"],
            "properties": {
                "inputs": {
                    "type": "array",
                    "minItems": 1,
                    "uniqueItems": true,
                    "description": "Outputs spent: a source name, or `tx.N` for output N of a tx.",
                    "items": { "type": "string", "pattern": REF }
                },
                "reference_inputs": {
                    "type": "array",
                    "uniqueItems": true,
                    "description": "Outputs read but not spent.",
                    "items": { "type": "string", "pattern": REF }
                },
                "outputs": {
                    "type": "array",
                    "minItems": 1,
                    "description": "Exactly one output must be `change`.",
                    "items": { "$ref": "#/$defs/output" }
                },
                "labels": {
                    "type": "array",
                    "description": "Free tags a schedule can select on.",
                    "items": { "type": "string" }
                },
                "metadata": {
                    "$ref": "#/$defs/metadata",
                    "description": "Transaction metadata on this tx, over the spec's."
                },
                "color": {
                    "$ref": "#/$defs/color",
                    "description": "Colour tag on this tx, over the spec's."
                },
                "cost": {
                    "$ref": "#/$defs/cost",
                    "description": "Script cost of this tx, instead of the spec's."
                }
            }
        }),
    );
    defs.insert(
        "metadata".to_string(),
        json!({
            "type": "object",
            "description": format!(
                "Transaction metadata by label (an integer), as in `cardano-cli \
                 --json-metadata-no-schema`: integers, text, lists and maps with text keys; \
                 text starting with `0x` is a byte string. Text and byte strings hold at most \
                 {MAX_STRING_BYTES} bytes, so split longer text into a list. Labels \
                 {MESSAGE_LABEL} and {COLOR_LABEL} belong to txflow; use `color` for the \
                 colour tag."
            ),
            "propertyNames": {
                "pattern": "^(0|[1-9][0-9]*)$",
                "not": { "enum": [MESSAGE_LABEL.to_string(), COLOR_LABEL.to_string()] }
            },
            "additionalProperties": { "$ref": "#/$defs/metadatum" }
        }),
    );
    defs.insert(
        "metadatum".to_string(),
        json!({
            "anyOf": [
                { "type": "integer" },
                {
                    "type": "string",
                    "pattern": format!("^0x([0-9a-fA-F]{{2}}){{0,{MAX_STRING_BYTES}}}$"),
                    "description": "A byte string, in hex."
                },
                {
                    "type": "string",
                    "maxLength": MAX_STRING_BYTES,
                    "not": { "pattern": "^0x" },
                    "description": format!("Text, at most {MAX_STRING_BYTES} bytes.")
                },
                { "type": "array", "items": { "$ref": "#/$defs/metadatum" } },
                {
                    "type": "object",
                    "propertyNames": { "maxLength": MAX_STRING_BYTES },
                    "additionalProperties": { "$ref": "#/$defs/metadatum" }
                }
            ]
        }),
    );
    defs.insert(
        "color".to_string(),
        json!({
            "type": "string",
            "pattern": "^#?[0-9a-fA-F]{6}$",
            "description": format!(
                "RGB, e.g. `ff0000`. Written as metadata label {COLOR_LABEL}, three bytes, \
                 as `tx-firehose --color` does, so `mempool-monitor` shows the txs in it."
            )
        }),
    );
    defs.insert(
        "output".to_string(),
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["to", "lovelace"],
            "properties": {
                "to": { "type": "string", "pattern": NAME, "description": "Owning wallet." },
                "lovelace": {
                    "oneOf": [
                        { "type": "integer", "minimum": 1 },
                        { "enum": ["min", "change"] }
                    ],
                    "description": "An amount, `min` (the ledger minimum) or `change` (what is left)."
                }
            }
        }),
    );
    cost_defs(&mut defs);
    defs.insert("schedule".to_string(), schedule_schema());
    Value::Object(defs)
}

fn schedule_schema() -> Value {
    let one = |field: &str, value: Value, description: &str| {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": [field],
            "properties": { field: value },
            "description": description
        })
    };
    json!({
        "type": "object",
        "description": "Where and when each transaction is sent. Without a schedule nothing is \
            submitted; the graph can still be built and checked.",
        "additionalProperties": false,
        "required": ["releases"],
        "properties": {
            "releases": {
                "type": "array",
                "minItems": 1,
                "description": "Each release sends the transactions it selects to one target. \
                    A transaction selected by several releases is sent by each, which is how \
                    one transaction reaches several relays.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["select", "target"],
                    "properties": {
                        "select": {
                            "description": "Which transactions to send: exactly one of \
                                `label`, `names` or `pattern`.",
                            "oneOf": [
                                one("label", json!({ "type": "string" }),
                                    "Every transaction with this label, e.g. `side-a`."),
                                one("names", json!({
                                    "type": "array",
                                    "minItems": 1,
                                    "uniqueItems": true,
                                    "items": { "type": "string", "pattern": REF }
                                }), "These transactions by full name, e.g. `ds/x1`."),
                                one("pattern", json!({ "type": "string", "pattern": NAME }),
                                    "Every transaction a pattern instance emits, all copies included."),
                            ]
                        },
                        "target": {
                            "type": "string",
                            "pattern": NAME,
                            "description": "A relay of the site's network, or a name in `targets`."
                        },
                        "via": {
                            "enum": ["n2c", "n2n"],
                            "default": "n2c",
                            "description": "`n2c`: submitted as a local client, and the node \
                                says whether it accepted each transaction. `n2n`: offered as a \
                                peer would, with no answer beyond the peer asking for it."
                        },
                        "at": {
                            "type": "string",
                            "pattern": DURATION,
                            "default": "0ms",
                            "description": "Offset from the start of the run."
                        },
                        "spacing": {
                            "type": "string",
                            "pattern": DURATION,
                            "default": "0ms",
                            "description": "Gap between consecutive selected transactions, in \
                                dependency order."
                        }
                    }
                }
            },
            "targets": {
                "type": "object",
                "description": "Nodes outside the site's network, for local runs.",
                "propertyNames": { "pattern": NAME },
                "additionalProperties": {
                    "type": "object",
                    "additionalProperties": false,
                    "minProperties": 1,
                    "properties": {
                        "host": { "type": "string", "description": "`host:port` for n2n." },
                        "socket": { "type": "string", "description": "Node socket path for n2c." }
                    }
                }
            },
            "options": {
                "type": "object",
                "description": "How the run behaves once sending starts.",
                "additionalProperties": false,
                "properties": {
                    "skip_descendants_of_rejected": {
                        "type": "boolean",
                        "description": "Stop sending a transaction's descendants once a relay \
                            rejects it (`n2c` only)."
                    },
                    "observe_for": {
                        "type": "string",
                        "pattern": DURATION,
                        "description": "How long to watch for inclusion after the last send."
                    }
                }
            }
        }
    })
}

/// A share of the per-tx limit with at most two decimals, `25%` or `12.5%`:
/// above 0, at most 100, no leading zeros or spaces, as [`crate::cost::CostValue::parse`].
const SHARE: &str = "^(?!0(\\.00?)?%$)(100(\\.00?)?|[1-9]?[0-9](\\.[0-9]{1,2})?)%$";
/// A share or a number of units written with params: `{{ dose }}%`.
const COST_TEXT: &str = "^\\s*\\{\\{[^{}]*\\}\\}\\s*%?\\s*$";

fn cost_defs(defs: &mut Map<String, Value>) {
    let dimensions = |value: &str| {
        json!({
            "cpu": {
                "$ref": format!("#/$defs/{value}"),
                "description": "CPU steps: a share of the network's per-tx step limit \
                    (maxTxExecutionUnits.steps), or a number of steps."
            },
            "mem": {
                "$ref": format!("#/$defs/{value}"),
                "description": "Memory units: a share of the network's per-tx memory limit \
                    (maxTxExecutionUnits.memory), or a number of units."
            }
        })
    };
    defs.insert(
        "cost_value".to_string(),
        json!({
            "oneOf": [
                {
                    "type": "string",
                    "pattern": SHARE,
                    "description": "A share of the per-tx limit, `25%` or `12.5%`, above 0."
                },
                {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Execution units."
                }
            ]
        }),
    );
    defs.insert(
        "cost_value_text".to_string(),
        json!({
            "oneOf": [
                { "$ref": "#/$defs/cost_value" },
                {
                    "type": "string",
                    "pattern": COST_TEXT,
                    "description": "`{{ expression }}%` for a share in whole percent, or \
                        `{{ expression }}` for units, over `params`."
                }
            ]
        }),
    );
    defs.insert(
        "cost".to_string(),
        json!({
            "type": "object",
            "description": "How much Plutus execution the tx carries, not which script: \
                materialise adds a run of txflow's burner script that uses as much as \
                possible without going over. A dimension left out is kept as low as the \
                script allows. A share keeps the spec network-free.",
            "additionalProperties": false,
            "minProperties": 1,
            "properties": dimensions("cost_value")
        }),
    );
    let mut covering = dimensions("cost_value_text");
    if let Some(properties) = covering.as_object_mut() {
        properties.insert(
            "select".to_string(),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["label"],
                "properties": {
                    "label": {
                        "type": "string",
                        "description": "Only txs with this label carry the cost."
                    }
                }
            }),
        );
    }
    defs.insert(
        "cost_covering".to_string(),
        json!({
            "type": "object",
            "description": "A script cost (see `cost`) for many txs. Values may use params.",
            "additionalProperties": false,
            "anyOf": [{ "required": ["cpu"] }, { "required": ["mem"] }],
            "properties": covering
        }),
    );
}

fn pattern_schema(pattern: &PatternInfo) -> Value {
    let mut args = Map::new();
    for arg in pattern.args.iter().chain(COMMON_ARGS) {
        let value = match &arg.kind {
            ArgKind::Count { min, max, default } => json!({
                "oneOf": [
                    { "type": "integer", "minimum": min, "maximum": max },
                    { "$ref": "#/$defs/arg_text" }
                ],
                "default": default
            }),
            ArgKind::Lovelace { default } => json!({
                "oneOf": [
                    { "type": "integer", "minimum": 1 },
                    { "$ref": "#/$defs/arg_text" }
                ],
                "description": format!("Default: {default}.")
            }),
            ArgKind::Choice { values } => json!({
                "enum": values,
                "default": values.first()
            }),
            ArgKind::Wallet => json!({ "type": "string", "pattern": NAME }),
        };
        let mut value = value;
        if let Some(object) = value.as_object_mut() {
            let summary = match object.get("description").and_then(Value::as_str) {
                Some(extra) => format!("{} {extra}", arg.summary),
                None => arg.summary.to_string(),
            };
            object.insert("description".to_string(), Value::String(summary));
        }
        args.insert(arg.name.to_string(), value);
    }
    json!({
        "type": "object",
        "title": pattern.name,
        "description": format!("{} Emits: {}.", pattern.summary, pattern.emits),
        "additionalProperties": false,
        "required": ["id", "use"],
        "properties": {
            "id": {
                "type": "string",
                "pattern": NAME,
                "description": "Instance name; prefixes everything it emits."
            },
            "use": { "const": pattern.name },
            "count": {
                "oneOf": [
                    { "type": "integer", "minimum": 1, "maximum": crate::patterns::MAX_SIZE },
                    { "$ref": "#/$defs/arg_text" }
                ],
                "description": "Independent copies, named `<id>_0/`, `<id>_1/`, …"
            },
            "with": {
                "type": "object",
                "additionalProperties": false,
                "properties": args
            },
            "metadata": {
                "$ref": "#/$defs/metadata",
                "description": "Transaction metadata on every tx this instance emits, over \
                    the spec's."
            },
            "color": {
                "$ref": "#/$defs/color",
                "description": "Colour tag on every tx this instance emits, over the spec's."
            },
            "cost": {
                "$ref": "#/$defs/cost_covering",
                "description": "Script cost of every tx this instance emits (or of those \
                    with `select.label`), instead of the spec's."
            }
        }
    })
}
