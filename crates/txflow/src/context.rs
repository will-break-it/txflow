//! Chain context: what materialise needs to know about the target network.
//!
//! The context is a YAML file. It names the network, gives the protocol
//! parameters (inline or from `cardano-cli query protocol-parameters` JSON),
//! and offers funding UTxOs with the key that spends them. Materialise uses
//! the offered UTxOs in order, taking only as many as the experiment needs.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{in_file, read_file, Error, Result};

/// The context file as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextFile {
    network: NetworkName,
    /// Network magic, recorded in the manifest (not used to build).
    magic: u32,
    /// Era of the target network, recorded in the manifest. `dijkstra` encodes
    /// the transactions in the Dijkstra format; anything else gives Conway.
    era: String,
    #[serde(default)]
    pparams: Option<ProtocolParams>,
    #[serde(default)]
    pparams_file: Option<PathBuf>,
    /// Where the txs are built, in each tx's message (`txflow/1 <origin>`);
    /// `cli` when not given.
    #[serde(default)]
    origin: Option<String>,
    funding: FundingFile,
}

// No `Debug`: it holds the signing key.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FundingFile {
    #[serde(default)]
    signing_key_file: Option<PathBuf>,
    #[serde(default)]
    signing_key_hex: Option<String>,
    /// Where funding change goes. Default: the funding key's enterprise address.
    #[serde(default)]
    change_address: Option<String>,
    utxos: Vec<OfferedUtxoFile>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct OfferedUtxoFile {
    /// `<tx id hex>#<index>`
    utxo: String,
    lovelace: u64,
}

/// A `cardano-cli` text envelope (`*.skey`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TextEnvelope {
    #[serde(rename = "type")]
    kind: String,
    cbor_hex: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkName {
    Testnet,
    Mainnet,
}

impl From<NetworkName> for pallas_addresses::Network {
    fn from(name: NetworkName) -> Self {
        match name {
            NetworkName::Testnet => pallas_addresses::Network::Testnet,
            NetworkName::Mainnet => pallas_addresses::Network::Mainnet,
        }
    }
}

/// The protocol parameters materialise uses. Field aliases accept the names
/// `cardano-cli query protocol-parameters` prints; other fields are ignored.
/// The script fields are needed only by txs with a script cost.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ProtocolParams {
    #[serde(alias = "txFeePerByte")]
    pub min_fee_a: u64,
    #[serde(alias = "txFeeFixed")]
    pub min_fee_b: u64,
    #[serde(alias = "maxTxSize")]
    pub max_tx_size: u64,
    #[serde(alias = "utxoCostPerByte")]
    pub coins_per_utxo_byte: u64,
    /// Collateral as a percentage of the fee.
    #[serde(
        default,
        alias = "collateralPercentage",
        skip_serializing_if = "Option::is_none"
    )]
    pub collateral_percentage: Option<u64>,
    /// Lovelace per memory unit and per CPU step.
    #[serde(
        default,
        alias = "executionUnitPrices",
        skip_serializing_if = "Option::is_none"
    )]
    pub execution_unit_prices: Option<ExecutionPrices>,
    /// The most execution one tx may use.
    #[serde(
        default,
        alias = "maxTxExecutionUnits",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_tx_execution_units: Option<ExecutionUnits>,
    /// Plutus cost models; only Plutus V3 is read.
    #[serde(default, alias = "costModels", skip_serializing_if = "Option::is_none")]
    pub cost_models: Option<CostModels>,
    /// Picks the builtin semantics scripts are evaluated with.
    #[serde(
        default,
        alias = "protocolVersion",
        skip_serializing_if = "Option::is_none"
    )]
    pub protocol_version: Option<ProtocolVersion>,
}

impl ProtocolParams {
    /// These parameters without the ones only script txs read.
    pub fn without_scripts(&self) -> ProtocolParams {
        ProtocolParams {
            collateral_percentage: None,
            execution_unit_prices: None,
            max_tx_execution_units: None,
            cost_models: None,
            protocol_version: None,
            ..self.clone()
        }
    }
}

/// Execution prices as exact fractions, as the ledger holds them.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ExecutionPrices {
    #[serde(alias = "priceMemory")]
    pub price_memory: Rational,
    #[serde(alias = "priceSteps")]
    pub price_steps: Rational,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct ExecutionUnits {
    pub memory: u64,
    pub steps: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CostModels {
    #[serde(default, alias = "PlutusV3", skip_serializing_if = "Option::is_none")]
    pub plutus_v3: Option<Vec<i64>>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct ProtocolVersion {
    pub major: u16,
    #[serde(default)]
    pub minor: u16,
}

/// A non-negative fraction. Read from a decimal number (`0.0577`, `7.21e-05`,
/// taken exactly as written) or `{ numerator, denominator }`; written as the latter.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rational {
    pub numerator: u64,
    pub denominator: u64,
}

impl<'de> Deserialize<'de> for Rational {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Number(f64),
            Fraction { numerator: u64, denominator: u64 },
        }
        match Raw::deserialize(d)? {
            Raw::Fraction {
                numerator,
                denominator,
            } if denominator > 0 => Ok(Rational {
                numerator,
                denominator,
            }),
            Raw::Fraction { .. } => Err(serde::de::Error::custom("denominator is zero")),
            Raw::Number(value) => Rational::from_decimal(value).map_err(serde::de::Error::custom),
        }
    }
}

impl Rational {
    /// The fraction a decimal number stands for: `7.21e-05` is 721/10000000.
    /// Rust prints the shortest decimal that reads back as the same `f64`,
    /// which is the number as `cardano-cli` wrote it.
    pub fn from_decimal(value: f64) -> std::result::Result<Rational, String> {
        if !value.is_finite() || value < 0.0 {
            return Err(format!("{value} is not a price"));
        }
        let text = format!("{value}");
        let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
        let digits = format!("{whole}{fraction}");
        let numerator: u64 = digits
            .parse()
            .map_err(|_| format!("{value} has too many digits"))?;
        let denominator = 10u64
            .checked_pow(fraction.len() as u32)
            .ok_or_else(|| format!("{value} has too many decimals"))?;
        let divisor = gcd(numerator, denominator);
        Ok(Rational {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        })
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a.max(1)
    } else {
        gcd(b, a % b)
    }
}

/// A UTxO offered for funding the experiment.
#[derive(Clone, Debug, PartialEq)]
pub struct OfferedUtxo {
    pub tx_id: [u8; 32],
    pub index: u64,
    pub lovelace: u64,
}

/// A 32-byte Ed25519 secret key. `Debug` never prints it.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretKeyBytes(pub [u8; 32]);

impl std::fmt::Debug for SecretKeyBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretKeyBytes(<redacted>)")
    }
}

/// The loaded chain context.
#[derive(Clone, Debug)]
pub struct ChainContext {
    pub network: NetworkName,
    pub magic: u32,
    pub era: String,
    pub pparams: ProtocolParams,
    /// Key that spends the offered UTxOs.
    pub funding_key: SecretKeyBytes,
    /// Bech32 address for funding change, if not the funding key's enterprise address.
    pub change_address: Option<String>,
    pub offered: Vec<OfferedUtxo>,
    /// Where the txs are built, e.g. `cli` or `txflow.fun`: the second word of
    /// each tx's watermark ([`crate::materialise::watermark`]).
    pub origin: String,
}

/// `origin` when it fits in a tx message: 1-48 of `a-z0-9.-`.
pub fn check_origin(origin: &str) -> Result<String> {
    let ok = !origin.is_empty()
        && origin.len() <= 48
        && origin
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-');
    if ok {
        Ok(origin.to_string())
    } else {
        Err(Error::Context(format!(
            "origin `{origin}` must be 1-48 of a-z, 0-9, `.` and `-`"
        )))
    }
}

impl ChainContext {
    /// Load a context file. Relative paths inside it resolve against its directory.
    pub fn load(path: &Path) -> Result<ChainContext> {
        let text = read_file(path)?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        ChainContext::from_yaml(&text, base).map_err(|e| in_file(path, e))
    }

    /// Parse a context. Relative paths resolve against `base`.
    pub fn from_yaml(text: &str, base: &Path) -> Result<ChainContext> {
        let file: ContextFile = crate::spec::from_yaml_strict(text)?;
        // Mainnet is the only network with this magic; anything else is a testnet.
        const MAINNET_MAGIC: u32 = 764_824_073;
        if (file.network == NetworkName::Mainnet) != (file.magic == MAINNET_MAGIC) {
            return Err(Error::Context(format!(
                "network `{:?}` does not match magic {}",
                file.network, file.magic
            )));
        }
        let pparams = match (file.pparams, file.pparams_file) {
            (Some(inline), None) => inline,
            (None, Some(p)) => {
                let path = base.join(p);
                let json = read_file(&path)?;
                serde_json::from_str(&json).map_err(|e| in_file(&path, Error::Json(e)))?
            }
            _ => {
                return Err(Error::Context(
                    "give exactly one of `pparams` and `pparams_file`".to_string(),
                ))
            }
        };
        let key_hex = match (file.funding.signing_key_hex, file.funding.signing_key_file) {
            (Some(hex), None) => hex,
            (None, Some(p)) => {
                let path = base.join(p);
                let key = signing_key(&read_file(&path)?).map_err(|e| in_file(&path, e))?;
                hex::encode(key.0)
            }
            _ => {
                return Err(Error::Context(
                    "give exactly one of `signing_key_file` and `signing_key_hex`".to_string(),
                ))
            }
        };
        let funding_key = SecretKeyBytes(decode_32(&key_hex, "signing key")?);
        let mut offered = Vec::new();
        let mut seen = BTreeSet::new();
        for utxo in &file.funding.utxos {
            let (id, index) = utxo.utxo.split_once('#').ok_or_else(|| {
                Error::Context(format!("`{}` is not `<tx id>#<index>`", utxo.utxo))
            })?;
            let index: u64 = index
                .parse()
                .map_err(|_| Error::Context(format!("bad index in `{}`", utxo.utxo)))?;
            let entry = OfferedUtxo {
                tx_id: decode_32(id, "tx id")?,
                index,
                lovelace: utxo.lovelace,
            };
            if !seen.insert((entry.tx_id, entry.index)) {
                return Err(Error::Context(format!("`{}` is offered twice", utxo.utxo)));
            }
            offered.push(entry);
        }
        Ok(ChainContext {
            network: file.network,
            magic: file.magic,
            era: file.era,
            pparams,
            funding_key,
            change_address: file.funding.change_address,
            offered,
            origin: check_origin(file.origin.as_deref().unwrap_or("cli"))?,
        })
    }
}

/// A payment signing key: a `cardano-cli` text envelope (`*.skey`) or 64 hex
/// characters.
pub fn signing_key(text: &str) -> Result<SecretKeyBytes> {
    let text = text.trim();
    if !text.starts_with('{') {
        return Ok(SecretKeyBytes(decode_32(text, "signing key")?));
    }
    let envelope: TextEnvelope = serde_json::from_str(text).map_err(Error::Json)?;
    // A `.vkey` (and stake or pool keys) also start with `5820`;
    // only accept payment signing keys.
    match envelope.kind.as_str() {
        "PaymentSigningKeyShelley_ed25519" | "GenesisUTxOSigningKey_ed25519" => {}
        kind if kind.contains("Extended") => {
            return Err(Error::Context(format!(
                "signing key file has type `{kind}`: extended keys are not supported \
                 yet, use a `PaymentSigningKeyShelley_ed25519`"
            )))
        }
        kind => {
            return Err(Error::Context(format!(
                "signing key file has type `{kind}`, expected \
                 `PaymentSigningKeyShelley_ed25519`"
            )))
        }
    }
    // A cardano-cli payment skey is CBOR bytes(32): `5820` then the key.
    let hex = envelope.cbor_hex.strip_prefix("5820").ok_or_else(|| {
        Error::Context("signing key file is not a 32-byte Ed25519 key".to_string())
    })?;
    Ok(SecretKeyBytes(decode_32(hex, "signing key")?))
}

fn decode_32(text: &str, what: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(text).map_err(|e| Error::Context(format!("{what}: {e}")))?;
    bytes
        .try_into()
        .map_err(|_| Error::Context(format!("{what} must be 32 bytes")))
}
