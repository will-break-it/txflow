//! `txflow` command line.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use txflow::context::{signing_key, ChainContext, NetworkName};
use txflow::error::{in_file, on_file, read_file, Error, Result};
use txflow::limits::Limits;
use txflow::network::Network;
use txflow::spec::ExperimentSpec;
use txflow::{expand, materialise};

#[derive(Parser)]
#[command(name = "txflow", about = "Declarative transaction-flow experiments")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum NetworkArg {
    Testnet,
    Mainnet,
}

impl From<NetworkArg> for NetworkName {
    fn from(arg: NetworkArg) -> Self {
        match arg {
            NetworkArg::Testnet => NetworkName::Testnet,
            NetworkArg::Mainnet => NetworkName::Mainnet,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Check that a spec is runnable and describe what it builds.
    Check {
        spec: PathBuf,
        /// Also hold it to a deployment's limits file (e.g. `limits.json`).
        #[arg(long)]
        limits: Option<PathBuf>,
        /// Resolve the schedule against a network file (e.g. `networks/musashi.json`).
        #[arg(long)]
        network: Option<PathBuf>,
    },
    /// Print the JSON Schema for specs of the version this build reads.
    Schema,
    /// Print the pattern catalogue as Markdown.
    Catalogue,
    /// Print a limits file as sentences (Markdown list).
    Limits { file: PathBuf },
    /// Print a network file's relays as Markdown.
    Network { file: PathBuf },
    /// Expand an experiment spec into its graph IR (with derived relations).
    Expand {
        spec: PathBuf,
        /// Write the IR here instead of stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Build and sign every transaction offline against a chain context.
    Materialise {
        spec: PathBuf,
        #[arg(long)]
        context: PathBuf,
        /// Write the manifest here instead of stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Print the slot addresses of a pool key: fund each with the same amount
    /// so that runs can lease them. Prints addresses only, never keys.
    Slots {
        /// The pool's payment signing key (`.skey`).
        #[arg(long)]
        key: PathBuf,
        /// How many slots.
        #[arg(long, default_value_t = 10)]
        slots: u32,
        #[arg(long, value_enum, default_value_t = NetworkArg::Testnet)]
        network: NetworkArg,
    },
    /// Write the run key (owner of every source and experiment output) as a
    /// cardano-cli signing key, to recover leftover funds with other tools.
    RunKey {
        spec: PathBuf,
        #[arg(long)]
        context: PathBuf,
        /// The `.skey` file to write. Treat it as a secret.
        #[arg(short, long)]
        output: PathBuf,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("txflow: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Check {
            spec,
            limits,
            network,
        } => {
            let limits = match limits {
                Some(path) => {
                    Limits::from_text(&read_file(&path)?).map_err(|e| in_file(&path, e))?
                }
                None => Limits::default(),
            };
            let network = match network {
                Some(path) => {
                    Some(Network::from_text(&read_file(&path)?).map_err(|e| in_file(&path, e))?)
                }
                None => None,
            };
            let report = txflow::check::check_within(&read_file(&spec)?, &limits, network.as_ref());
            if !report.ok {
                return Err(Error::Parse {
                    path: spec.display().to_string(),
                    message: report.errors.join("; "),
                });
            }
            write(None, |w| {
                for line in report.summary.iter().flat_map(|s| &s.description) {
                    writeln!(w, "{line}")?;
                }
                for warning in &report.warnings {
                    writeln!(w, "warning: {warning}")?;
                }
                Ok(())
            })
        }
        Command::Network { file } => {
            let network = Network::from_text(&read_file(&file)?).map_err(|e| in_file(&file, e))?;
            write(None, |w| Ok(write!(w, "{}", network.describe())?))
        }
        Command::Limits { file } => {
            let limits = Limits::from_text(&read_file(&file)?).map_err(|e| in_file(&file, e))?;
            write(None, |w| {
                for line in limits.describe() {
                    writeln!(w, "- {line}")?;
                }
                Ok(())
            })
        }
        Command::Catalogue => write(None, |w| {
            Ok(w.write_all(txflow::catalogue::markdown().as_bytes())?)
        }),
        Command::Schema => write(None, |w| {
            serde_json::to_writer_pretty(&mut *w, &txflow::schema::schema())?;
            Ok(w.write_all(b"\n")?)
        }),
        Command::Expand { spec, output } => {
            let spec = load_spec(&spec)?;
            let ir = expand::expand(&spec)?;
            // serde_yaml ends the document with a newline.
            write(output.as_deref(), |w| Ok(serde_yaml::to_writer(w, &ir)?))
        }
        Command::Materialise {
            spec,
            context,
            output,
        } => {
            let spec = load_spec(&spec)?;
            let ir = expand::expand(&spec)?;
            let context = ChainContext::load(&context)?;
            let manifest = materialise::materialise(&spec.experiment, &ir, spec.seed, &context)?;
            write(output.as_deref(), |w| {
                serde_json::to_writer_pretty(&mut *w, &manifest)?;
                Ok(w.write_all(b"\n")?)
            })
        }
        Command::Slots {
            key,
            slots,
            network,
        } => {
            let pool = signing_key(&read_file(&key)?).map_err(|e| in_file(&key, e))?;
            let network = network.into();
            // Slots are numbered from 1, as the Worker's funding pool numbers them.
            let slots = (1..=slots)
                .map(|slot| {
                    let key = txflow::funds::slot_key(&pool, slot);
                    Ok(serde_json::json!({
                        "slot": slot,
                        "address": txflow::funds::address(&key, network)?,
                    }))
                })
                .collect::<Result<Vec<_>>>()?;
            let out = serde_json::json!({
                "pool": txflow::funds::address(&pool, network)?,
                "slots": slots,
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
            Ok(())
        }
        Command::RunKey {
            spec,
            context,
            output,
        } => {
            let spec = load_spec(&spec)?;
            let ir = expand::expand(&spec)?;
            let context = ChainContext::load(&context)?;
            let ir_hash = txflow::derive::ir_hash(&ir)?;
            let key = materialise::run_key(&context.funding_key, &ir_hash, spec.seed);
            let envelope = serde_json::json!({
                "type": "PaymentSigningKeyShelley_ed25519",
                "description": format!("txflow run key for {}", spec.experiment),
                "cborHex": format!("5820{}", hex::encode(key.0)),
            });
            write_secret(&output, &serde_json::to_string_pretty(&envelope)?)
        }
    }
}

fn load_spec(path: &Path) -> Result<ExperimentSpec> {
    ExperimentSpec::from_yaml(&read_file(path)?).map_err(|e| in_file(path, e))
}

/// Stream a document to a file or stdout. Streamed rather than built as a
/// `String` first: outputs can be hundreds of MB.
fn write(path: Option<&Path>, emit: impl FnOnce(&mut dyn Write) -> Result<()>) -> Result<()> {
    let Some(path) = path else {
        // Not `print!`, so a closed pipe is an error, not a panic.
        let mut out = std::io::BufWriter::new(std::io::stdout().lock());
        emit(&mut out)?;
        return Ok(out.flush()?);
    };
    let file = std::fs::File::create(path).map_err(on_file(path))?;
    let mut out = std::io::BufWriter::new(file);
    emit(&mut out)
        .and_then(|()| Ok(out.flush()?))
        .map_err(|e| in_output(path, e))
}

/// Name the output file an error came from. Serializers wrap I/O failures in
/// their own error types, so every kind is named.
fn in_output(path: &Path, error: Error) -> Error {
    match error {
        Error::Io(e) => on_file(path)(e),
        other => on_file(path)(std::io::Error::other(other.to_string())),
    }
}

/// Write a secret: never over an existing file, readable by the owner only.
fn write_secret(path: &Path, text: &str) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).map_err(on_file(path))?;
    let written = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all());
    if let Err(e) = written {
        // Leave no partial key behind for the next attempt to trip over.
        let _ = std::fs::remove_file(path);
        return Err(on_file(path)(e));
    }
    Ok(())
}
