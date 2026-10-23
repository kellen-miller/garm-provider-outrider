use clap::{Parser, Subcommand};
use outrider_xtask::{acceptance, bundle, contract_tools, linux_contracts};
use std::path::PathBuf;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Operation,
}

#[derive(Subcommand)]
enum Operation {
    CheckAcceptance {
        #[arg(long, conflicts_with = "all", required_unless_present = "all")]
        milestone: Option<String>,
        #[arg(long)]
        all: bool,
    },
    CheckLinuxContracts,
    BuildProviderBundle(bundle::Options),
    InstallContractTools {
        output: PathBuf,
    },
    BuildGarmWrapperProbe {
        output: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Operation::CheckAcceptance { milestone, all: _ } => acceptance::run(milestone.as_deref()),
        Operation::CheckLinuxContracts => linux_contracts::run(),
        Operation::BuildProviderBundle(options) => bundle::build(&options),
        Operation::InstallContractTools { output } => contract_tools::install(&output),
        Operation::BuildGarmWrapperProbe { output } => contract_tools::build_wrapper(&output),
    }
}
