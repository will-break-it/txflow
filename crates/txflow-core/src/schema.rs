//! JSON Schema for experiment specs, generated from the [`crate::catalogue`].
//!
//! The schema checks shape only: field names, types, pattern names, argument
//! ranges and name spelling. Whether the graph makes sense (references that
//! resolve, no double spend of an ancestor's output, size limits) is checked by
//! [`crate::expand::expand`]. A spec that passes both is runnable.

use serde_json::{json, Map, Value};

use crate::catalogue::{ArgKind, PatternInfo, COMMON_ARGS, PATTERNS};
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
                }
            }
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
            }
        }
    })
}
