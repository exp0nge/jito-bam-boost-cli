use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::bam_boost::BamBoostCommands;

#[derive(Parser)]
#[command(author, version, about = "A CLI for managing BAM Boost operations", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<ProgramCommand>,

    #[arg(long, global = true, help = "Path to the configuration file")]
    pub config_file: Option<PathBuf>,

    #[arg(
        long,
        global = true,
        default_value = "https://api.mainnet-beta.solana.com",
        help = "RPC URL to use"
    )]
    pub rpc_url: Option<String>,

    #[arg(long, global = true, help = "Commitment level")]
    pub commitment: Option<String>,

    #[arg(long, global = true, help = "BAM Boost program ID")]
    pub jito_bam_boost_program_id: Option<String>,

    #[arg(long, global = true, help = "Filepath or URL to a keypair", conflicts_with = "address")]
    pub signer: Option<String>,

    #[arg(
        long,
        global = true,
        help = "Claimant pubkey (offline mode, no signing). Mutually exclusive with --signer",
        conflicts_with = "signer"
    )]
    pub address: Option<String>,

    #[arg(long, global = true, help = "Verbose mode")]
    pub verbose: bool,

    #[arg(
        long,
        global = true,
        default_value = "false",
        help = "This will print out the raw TX instead of running it"
    )]
    pub print_tx: bool,

    #[arg(
        long,
        global = true,
        default_value = "false",
        help = "This will print out account information in JSON format"
    )]
    pub print_json: bool,

    #[arg(
        long,
        global = true,
        default_value = "false",
        help = "This will print out account information in JSON format with reserved space"
    )]
    pub print_json_with_reserves: bool,

    #[arg(long, global = true, hide = true)]
    pub markdown_help: bool,

    #[arg(
        long,
        global = true,
        help = "Assert that the BAM Boost program was deployed at the given slot (Lighthouse guard)"
    )]
    pub assert_deploy_slot: Option<u64>,

    #[arg(long, global = true, help = "Durable nonce account pubkey")]
    pub nonce: Option<String>,

    #[arg(long, global = true, help = "Nonce authority pubkey (defaults to nonce account itself if omitted)")]
    pub nonce_authority: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum ProgramCommand {
    /// BAM Boost program commands
    BamBoost {
        #[command(subcommand)]
        action: BamBoostCommands,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bam_boost::{MerkleDistributorActions, NetworkArg};

    #[test]
    fn test_rpc_default_is_mainnet() {
        let cli = Cli::parse_from(["jito-bam-boost-cli"]);
        assert_eq!(
            cli.rpc_url.as_deref(),
            Some("https://api.mainnet-beta.solana.com")
        );
    }

    #[test]
    fn test_network_default_is_mainnet() {
        let cli = Cli::parse_from([
            "jito-bam-boost-cli",
            "bam-boost",
            "merkle-distributor",
            "claim",
            "--epoch",
            "1",
        ]);
        match cli.command {
            Some(ProgramCommand::BamBoost {
                action:
                    crate::bam_boost::BamBoostCommands::MerkleDistributor {
                        action: MerkleDistributorActions::Claim { network, .. },
                    },
            }) => {
                assert!(
                    matches!(network, NetworkArg::Mainnet),
                    "Expected default network to be Mainnet, got {:?}",
                    network
                );
            }
            other => panic!("Unexpected command variant: {:?}", other),
        }
    }

    #[test]
    fn test_address_and_signer_mutually_exclusive() {
        let result = Cli::try_parse_from([
            "jito-bam-boost-cli",
            "--signer",
            "/tmp/key.json",
            "--address",
            "11111111111111111111111111111111",
        ]);
        assert!(result.is_err(), "Expected error when both --signer and --address are provided");
    }
}
