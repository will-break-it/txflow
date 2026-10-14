//! Transaction metadata a spec asks for, and the colour tag.
//!
//! A spec can put metadata on its transactions (`metadata:`, keyed by label)
//! and a colour (`color:`), on the whole experiment, on one pattern instance
//! or on one hand-written tx. Values use the plain JSON mapping of
//! `cardano-cli --json-metadata-no-schema`: integers, text, lists and maps
//! with text keys; text starting with `0x` is a byte string.
//!
//! The ledger's own rules (text and byte strings of at most 64 bytes, integers
//! in range) are checked here, so a spec fails before anything is funded
//! rather than at submit. Two labels belong to txflow: 674 carries its
//! attribution message, and 1022 is written by `color`, in the form
//! `tx-firehose --color` uses (three bytes of RGB), which `mempool-monitor`
//! paints by.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::error::{Error, Result};

/// Label of txflow's own attribution message (CIP-20).
pub const MESSAGE_LABEL: u64 = 674;
/// Label of the colour tag `tx-firehose --color` writes and `mempool-monitor` reads.
pub const COLOR_LABEL: u64 = 1022;
/// Longest text or byte string the ledger accepts in metadata, in bytes.
pub const MAX_STRING_BYTES: usize = 64;
/// Deepest nesting of lists and maps (a label's value is depth 1).
pub const MAX_DEPTH: usize = 8;
/// Most encoded bytes of metadata and colour one tx may ask for. Half of
/// mainnet's `max_tx_size`, so a tx always has room for its inputs, outputs
/// and witness.
pub const MAX_BYTES: usize = 8 * 1024;

/// A metadata label: an unsigned 64-bit integer. Written as a number in YAML,
/// or as a string of digits where keys must be strings (JSON).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Label(pub u64);

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for Label {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for Label {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = Label;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a metadata label, an integer from 0 to 18446744073709551615")
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Label, E> {
                Ok(Label(v))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Label, E> {
                u64::try_from(v)
                    .map(Label)
                    .map_err(|_| E::custom(format!("metadata label {v} is negative")))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<Label, E> {
                let digits = !v.is_empty() && v.chars().all(|c| c.is_ascii_digit());
                if !digits || (v.len() > 1 && v.starts_with('0')) {
                    return Err(E::custom(format!(
                        "metadata label `{v}` is not an integer from 0 to 18446744073709551615"
                    )));
                }
                v.parse()
                    .map(Label)
                    .map_err(|_| E::custom(format!("metadata label `{v}` is too large")))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// Metadata by label.
pub type Metadata = BTreeMap<Label, Value>;

/// `base` with each label of `over` replacing the same label of `base`.
pub fn merge(base: &Metadata, over: &Metadata) -> Metadata {
    let mut merged = base.clone();
    merged.extend(over.iter().map(|(k, v)| (*k, v.clone())));
    merged
}

/// Check labels and values against the ledger's rules and txflow's own.
pub fn validate(metadata: &Metadata) -> Result<()> {
    for (label, value) in metadata {
        if label.0 == MESSAGE_LABEL {
            return Err(Error::Spec(format!(
                "metadata label {MESSAGE_LABEL} is reserved for txflow's own message"
            )));
        }
        if label.0 == COLOR_LABEL {
            return Err(Error::Spec(format!(
                "metadata label {COLOR_LABEL} is the colour tag; write `color: rrggbb` instead"
            )));
        }
        value_ok(value, 1).map_err(|e| Error::Spec(format!("metadata label {label}: {e}")))?;
    }
    Ok(())
}

fn value_ok(value: &Value, depth: usize) -> std::result::Result<(), String> {
    match value {
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                Ok(())
            } else {
                Err(format!("{n} is not an integer; metadata has no fractions"))
            }
        }
        Value::String(s) => string_ok(s),
        Value::Array(items) => {
            nested(depth)?;
            items.iter().try_for_each(|item| value_ok(item, depth + 1))
        }
        Value::Object(map) => {
            nested(depth)?;
            for (key, item) in map {
                string_ok(key)?;
                value_ok(item, depth + 1)?;
            }
            Ok(())
        }
        Value::Bool(b) => Err(format!("{b} is not allowed; metadata has no booleans")),
        Value::Null => Err("null is not allowed; metadata has no null".to_string()),
    }
}

fn nested(depth: usize) -> std::result::Result<(), String> {
    if depth >= MAX_DEPTH {
        return Err(format!("nested deeper than {MAX_DEPTH} levels"));
    }
    Ok(())
}

fn string_ok(s: &str) -> std::result::Result<(), String> {
    if let Some(hex_digits) = s.strip_prefix("0x") {
        let bytes = hex::decode(hex_digits)
            .map_err(|_| format!("`{s}` starts with 0x but is not whole bytes of hex"))?;
        if bytes.len() > MAX_STRING_BYTES {
            return Err(format!(
                "a byte string of {} bytes; the ledger allows at most {MAX_STRING_BYTES}",
                bytes.len()
            ));
        }
    } else if s.len() > MAX_STRING_BYTES {
        return Err(format!(
            "text of {} bytes (`{}…`); the ledger allows at most {MAX_STRING_BYTES}, \
             so split it into a list",
            s.len(),
            s.chars().take(16).collect::<String>()
        ));
    }
    Ok(())
}

/// A colour written `rrggbb` or `#rrggbb`, as lowercase `rrggbb`.
pub fn parse_color(text: &str) -> Result<String> {
    let digits = text.strip_prefix('#').unwrap_or(text);
    if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::Spec(format!(
            "color `{text}` must be six hex digits of RGB, e.g. ff0000"
        )));
    }
    Ok(digits.to_ascii_lowercase())
}

/// The three bytes of a colour [`parse_color`] accepted.
pub fn color_bytes(color: &str) -> [u8; 3] {
    let mut rgb = [0u8; 3];
    if let Ok(bytes) = hex::decode(color) {
        if bytes.len() == 3 {
            rgb.copy_from_slice(&bytes);
        }
    }
    rgb
}

/// Encoded size of what a tx asks for: its metadata entries and its colour,
/// each as a label and a value, without the enclosing map.
pub fn encoded_size(metadata: &Metadata, color: Option<&str>) -> usize {
    let mut out = Vec::new();
    for (label, value) in metadata {
        cbor_head(&mut out, 0, label.0);
        encode_value(&mut out, value);
    }
    if let Some(color) = color {
        cbor_head(&mut out, 0, COLOR_LABEL);
        cbor_bytes(&mut out, &color_bytes(color));
    }
    out.len()
}

/// Shelley-format auxiliary data: one map holding txflow's message under
/// [`MESSAGE_LABEL`] (`{"msg": [parts...]}`), the colour under
/// [`COLOR_LABEL`] and `metadata`'s labels, in ascending order of label.
pub fn auxiliary_data(message: &[&str], metadata: &Metadata, color: Option<&str>) -> Vec<u8> {
    let mut entries: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let mut msg = Vec::new();
    cbor_head(&mut msg, 5, 1);
    cbor_text(&mut msg, "msg");
    cbor_head(&mut msg, 4, message.len() as u64);
    for part in message {
        cbor_text(&mut msg, part);
    }
    entries.insert(MESSAGE_LABEL, msg);
    if let Some(color) = color {
        let mut rgb = Vec::new();
        cbor_bytes(&mut rgb, &color_bytes(color));
        entries.insert(COLOR_LABEL, rgb);
    }
    for (label, value) in metadata {
        let mut encoded = Vec::new();
        encode_value(&mut encoded, value);
        entries.entry(label.0).or_insert(encoded);
    }
    let mut out = Vec::new();
    cbor_head(&mut out, 5, entries.len() as u64);
    for (label, value) in entries {
        cbor_head(&mut out, 0, label);
        out.extend_from_slice(&value);
    }
    out
}

/// One metadata value as CBOR. Maps are written in their key order.
fn encode_value(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                cbor_head(out, 0, u);
            } else if let Some(i) = n.as_i64() {
                // -1 - i, for negative i.
                cbor_head(out, 1, (-1 - i) as u64);
            }
        }
        Value::String(s) => match s.strip_prefix("0x").map(hex::decode) {
            Some(Ok(bytes)) => cbor_bytes(out, &bytes),
            _ => cbor_text(out, s),
        },
        Value::Array(items) => {
            cbor_head(out, 4, items.len() as u64);
            for item in items {
                encode_value(out, item);
            }
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            cbor_head(out, 5, keys.len() as u64);
            for key in keys {
                encode_value(out, &Value::String(key.clone()));
                encode_value(out, &map[key]);
            }
        }
        // Rejected by `validate`.
        Value::Bool(_) | Value::Null => {}
    }
}

fn cbor_text(out: &mut Vec<u8>, text: &str) {
    cbor_head(out, 3, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
}

fn cbor_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    cbor_head(out, 2, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// CBOR major type and argument, shortest encoding.
fn cbor_head(out: &mut Vec<u8>, major: u8, value: u64) {
    let m = major << 5;
    if value < 24 {
        out.push(m | value as u8);
    } else if value <= u64::from(u8::MAX) {
        out.push(m | 24);
        out.push(value as u8);
    } else if value <= u64::from(u16::MAX) {
        out.push(m | 25);
        out.extend_from_slice(&(value as u16).to_be_bytes());
    } else if value <= u64::from(u32::MAX) {
        out.push(m | 26);
        out.extend_from_slice(&(value as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend_from_slice(&value.to_be_bytes());
    }
}
