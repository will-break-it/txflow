//! A network the site submits to, and its relays.
//!
//! The network file is deployment configuration, like [`crate::limits`]: it
//! names the relays a schedule may target. Relay addresses are public; keys
//! never go in this file.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::schedule::Via;

/// One network, read from `networks/<id>.json`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Network {
    /// Short id, e.g. `musashi`.
    pub id: String,
    /// Display name, e.g. `Musashi`.
    pub name: String,
    /// The protocol it runs, e.g. `Ouroboros Leios`.
    pub protocol: String,
    /// Network magic.
    pub magic: u32,
    #[serde(default)]
    pub description: String,
    pub relays: Vec<Relay>,
    /// Where the page links a transaction id, when the network has an explorer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explorer: Option<Explorer>,
}

/// A block explorer for the network.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Explorer {
    /// Display name, e.g. `KleioScan`.
    pub name: String,
    /// A transaction's page, with `{tx}` where the id goes.
    pub tx: String,
    /// An address's page, with `{address}` where the address goes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// A block's page, with `{block}` where its number goes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<String>,
}

/// A node that accepts submissions.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Relay {
    /// What a schedule's `target` names, e.g. `frankfurt`.
    pub id: String,
    /// Display name, e.g. `Frankfurt`.
    pub label: String,
    /// Earlier ids, still accepted as a `target` so older specs keep working.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Public address (IP or name).
    pub host: String,
    /// How it takes submissions. `n2c` reaches the node's local socket
    /// through a bridge on the relay.
    pub via: Vec<Via>,
    /// Node-to-node port.
    #[serde(default = "default_n2n_port")]
    pub n2n_port: u16,
    /// TCP port of the node-to-client bridge, when it has a fixed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n2c_port: Option<u16>,
    /// When a temporary relay goes away, as UTC `YYYY-MM-DDTHH:MM:SSZ`. From
    /// then on the site leaves it out of the network it serves, so neither
    /// the model nor a run sees it; nothing here compares it with a clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
}

/// `YYYY-MM-DDTHH:MM:SSZ`, the one form `until` takes, so that it sorts and
/// parses the same everywhere (JavaScript's `Date.parse` included).
fn is_utc_instant(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 20
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            10 => *c == b'T',
            13 | 16 => *c == b':',
            19 => *c == b'Z',
            _ => c.is_ascii_digit(),
        })
}

fn default_n2n_port() -> u16 {
    3001
}

impl Network {
    pub fn from_text(text: &str) -> Result<Network> {
        let network: Network =
            serde_json::from_str(text).map_err(|e| Error::Spec(format!("network file: {e}")))?;
        let mut seen = std::collections::BTreeSet::new();
        for relay in &network.relays {
            for name in std::iter::once(&relay.id).chain(&relay.aliases) {
                if !seen.insert(name.as_str()) {
                    return Err(Error::Spec(format!(
                        "network file: relay `{name}` is listed twice"
                    )));
                }
            }
            if relay.via.is_empty() {
                return Err(Error::Spec(format!(
                    "network file: relay `{}` takes neither `n2n` nor `n2c`",
                    relay.id
                )));
            }
            if let Some(until) = &relay.until {
                if !is_utc_instant(until) {
                    return Err(Error::Spec(format!(
                        "network file: relay `{}` has `until` {until:?}; write it as UTC YYYY-MM-DDTHH:MM:SSZ",
                        relay.id
                    )));
                }
            }
        }
        if let Some(explorer) = &network.explorer {
            let fields = [
                ("tx", Some(&explorer.tx)),
                ("address", explorer.address.as_ref()),
                ("block", explorer.block.as_ref()),
            ];
            for (field, template) in fields {
                let Some(template) = template else { continue };
                if !template.starts_with("https://") || !template.contains(&format!("{{{field}}}"))
                {
                    return Err(Error::Spec(format!(
                        "network file: `explorer.{field}` must be an https URL with `{{{field}}}` in it"
                    )));
                }
            }
        }
        Ok(network)
    }

    /// The relay with this id, or with this as an earlier id.
    pub fn relay(&self, id: &str) -> Option<&Relay> {
        self.relays
            .iter()
            .find(|r| r.id == id || r.aliases.iter().any(|a| a == id))
    }

    /// The relays as Markdown lines, for the model's prompt.
    pub fn describe(&self) -> String {
        let mut out = format!(
            "Network: {} ({}), magic {}.\n\nRelays a release can target:\n\n",
            self.name, self.protocol, self.magic
        );
        for relay in &self.relays {
            let via = relay
                .via
                .iter()
                .map(|v| format!("`{}`", v.as_str()))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("- `{}`: {} (via {via})\n", relay.id, relay.label));
        }
        out
    }
}
