use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};

use clap::{Parser, Subcommand};
use outrider::{config::ConfigPatch, label::RoutingRequest};

#[derive(Parser)]
#[command(name = "outrider", version, about = "Multi-cloud GARM runner provider")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Validate {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, default_value = "default")]
        profile: String,
    },
    Label {
        #[arg(long)]
        repository_id: Option<u64>,
        #[arg(long)]
        run_id: Option<u64>,
        #[arg(long)]
        attempt: Option<u32>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long = "override", value_name = "KEY=VALUE")]
        overrides: Vec<String>,
        #[arg(long)]
        token: Option<String>,
    },
    Explain {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, conflicts_with = "snapshot")]
        profile: Option<String>,
        #[arg(long, conflicts_with = "profile")]
        snapshot: Option<String>,
    },
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    Inventory {
        #[arg(long)]
        config: PathBuf,
    },
    Cleanup {
        #[command(subcommand)]
        command: CleanupCommand,
    },
}

#[derive(Subcommand)]
enum CleanupCommand {
    Inspect {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        controller: String,
        #[arg(long)]
        allocation: String,
    },
    Resolve {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        attestation: PathBuf,
    },
}

fn main() -> ExitCode {
    if std::env::args_os().nth(1).is_none() && outrider::garm::is_provider_invocation() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build();
        return match runtime {
            Ok(runtime) => runtime.block_on(outrider::garm::run_provider()),
            Err(_) => {
                eprintln!("outrider provider: cannot initialize runtime");
                ExitCode::FAILURE
            }
        };
    }

    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("outrider: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Command::Validate { config, profile } => {
            let patch = ConfigPatch::from_yaml(&fs::read_to_string(config)?)?;
            patch.normalize_profile(None, &profile, None)?;
            println!("valid: profile {profile}");
        }

        Command::Explain {
            config,
            profile,
            snapshot,
        } => {
            let patch = ConfigPatch::from_yaml(&fs::read_to_string(config)?)?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let report = runtime.block_on(outrider::daemon::explain_remote(
                &patch,
                outrider::allocation::ExplainRequest {
                    profile,
                    snapshot_id: snapshot,
                },
            ))?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }

        Command::Inventory { config } => {
            let patch = ConfigPatch::from_yaml(&fs::read_to_string(config)?)?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let report = runtime.block_on(outrider::daemon::inventory_remote(&patch))?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }

        Command::Label {
            repository_id,
            run_id,
            attempt,
            profile,
            overrides,
            token,
        } => {
            if let Some(token) = token {
                let request = RoutingRequest::parse(&token)?;
                println!("canonical: {}", request.format()?);
            } else {
                let repository_id =
                    repository_id.ok_or("--repository-id is required without --token")?;
                let run_id = run_id.ok_or("--run-id is required without --token")?;
                let attempt = attempt.ok_or("--attempt is required without --token")?;
                let profile = profile.ok_or("--profile is required without --token")?;
                let mut values = BTreeMap::new();
                for raw in overrides {
                    let (key, value) = raw.split_once('=').ok_or("override must be KEY=VALUE")?;
                    if values.insert(key.to_owned(), value.to_owned()).is_some() {
                        return Err(format!("duplicate override {key}").into());
                    }
                }

                let request = RoutingRequest {
                    repository_id,
                    run_id,
                    run_attempt: attempt,
                    profile,
                    overrides: values,
                };

                println!("{}", request.format()?);
            }
        }

        Command::Serve { config } => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(outrider::daemon::serve(&config))?;
        }

        Command::Cleanup { command } => match command {
            CleanupCommand::Inspect {
                database,
                controller,
                allocation,
            } => {
                outrider::cleanup::validate_host()?;
                let journal = open_existing_journal(&database)?;
                let inspection = outrider::cleanup::inspect(&journal, &controller, &allocation)?;
                println!("{}", serde_json::to_string_pretty(&inspection)?);
            }
            CleanupCommand::Resolve {
                database,
                attestation,
            } => {
                outrider::cleanup::validate_host()?;
                let journal = open_existing_journal(&database)?;
                let attestation: outrider::cleanup::CleanupAttestation =
                    serde_json::from_slice(&fs::read(attestation)?)?;
                let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                let changed = outrider::cleanup::resolve(&journal, &attestation, now)?;
                println!(
                    "{}",
                    if changed {
                        "resolved"
                    } else {
                        "already resolved"
                    }
                );
            }
        },
    }

    Ok(())
}

fn open_existing_journal(
    path: &std::path::Path,
) -> Result<outrider::journal::Journal, Box<dyn std::error::Error>> {
    if !path.is_file() {
        return Err("database path must name an existing journal file".into());
    }
    Ok(outrider::journal::Journal::open(path)?)
}
