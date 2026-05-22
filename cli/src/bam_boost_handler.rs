use anyhow::anyhow;
use borsh::BorshDeserialize;
use jito_bam_boost_client::{accounts::ClaimStatus, instructions::ClaimBuilder};
use jito_bam_boost_merkle_tree::bam_boost_merkle_tree::BamBoostMerkleTree;
use solana_hash::Hash;
use solana_keypair::Signer;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_transaction::{Instruction, Message, Transaction};
use spl_associated_token_account_interface::{
    address::get_associated_token_address_with_program_id,
    instruction::create_associated_token_account_idempotent,
};

use crate::{
    bam_boost::{BamBoostCommands, ClaimStatusActions, MerkleDistributorActions, NetworkArg},
    cli_config::CliConfig,
    lighthouse::build_assert_deploy_slot_ix,
    JITOSOL_MINT,
};

#[allow(dead_code)]
pub struct BamBoostCliHandler {
    /// The configuration of CLI
    cli_config: CliConfig,

    /// The Pubkey of the Jito BAM Boost Program
    bam_boost_program_id: Pubkey,

    /// This will print out the raw TX instead of running it
    print_tx: bool,

    /// This will print out the account information in JSON format
    print_json: bool,

    /// This will print out the account information in JSON format with reserved space
    print_json_with_reserves: bool,

    /// When set, prepend a Lighthouse AssertUpgradeableLoaderAccount instruction
    assert_deploy_slot: Option<u64>,

    /// Durable nonce account
    nonce: Option<Pubkey>,

    /// Nonce authority (defaults to the nonce account pubkey if not specified)
    nonce_authority: Option<Pubkey>,
}

impl BamBoostCliHandler {
    pub fn new(
        cli_config: CliConfig,
        bam_boost_program_id: Pubkey,
        print_tx: bool,
        print_json: bool,
        print_json_with_reserves: bool,
        assert_deploy_slot: Option<u64>,
        nonce: Option<Pubkey>,
        nonce_authority: Option<Pubkey>,
    ) -> Self {
        Self {
            cli_config,
            bam_boost_program_id,
            print_tx,
            print_json,
            print_json_with_reserves,
            assert_deploy_slot,
            nonce,
            nonce_authority,
        }
    }

    /// Resolve the claimant pubkey: either from a signer keypair or from --address.
    fn resolve_claimant(&self) -> anyhow::Result<Pubkey> {
        if let Some(signer) = &self.cli_config.signer {
            Ok(signer.pubkey())
        } else if let Some(address) = self.cli_config.address {
            Ok(address)
        } else {
            Err(anyhow!("Either --signer or --address is required"))
        }
    }

    /// Whether we are in unsigned/offline mode (--address without --signer).
    fn is_offline(&self) -> bool {
        self.cli_config.signer.is_none() && self.cli_config.address.is_some()
    }

    /// Whether we should print the transaction instead of signing/sending.
    fn should_print_tx(&self) -> bool {
        self.print_tx || self.is_offline()
    }

    pub async fn handle(&self, action: BamBoostCommands) -> anyhow::Result<()> {
        match action {
            BamBoostCommands::MerkleDistributor {
                action: MerkleDistributorActions::Claim { network, epoch },
            } => {
                let network = match network {
                    NetworkArg::Mainnet => "mainnet",
                    NetworkArg::Testnet => "testnet",
                };

                self.claim(network, epoch).await
            }
            BamBoostCommands::ClaimStatus {
                action: ClaimStatusActions::Get { epoch, claimant },
            } => self.get_claim_status(epoch, claimant),
        }
    }

    fn merkle_distributor_address(&self, jitosol_mint: Pubkey, epoch: u64) -> Pubkey {
        Pubkey::find_program_address(
            &[
                b"merkle_distributor",
                jitosol_mint.to_bytes().as_slice(),
                epoch.to_le_bytes().as_slice(),
            ],
            &self.bam_boost_program_id,
        )
        .0
    }

    fn claim_status_address(&self, claimant: Pubkey, distributor_pda: Pubkey) -> Pubkey {
        Pubkey::find_program_address(
            &[
                b"claim_status",
                claimant.to_bytes().as_slice(),
                distributor_pda.to_bytes().as_slice(),
            ],
            &self.bam_boost_program_id,
        )
        .0
    }

    /// Parse the stored nonce hash from a nonce account's raw data.
    /// Layout: 4 bytes version (u32 LE) + 4 bytes state (u32 LE) + 32 bytes authority + 32 bytes blockhash + ...
    /// The nonce hash is at offset 40.
    fn parse_nonce_hash(data: &[u8]) -> anyhow::Result<Hash> {
        if data.len() < 72 {
            return Err(anyhow!("Nonce account data too short: {} bytes", data.len()));
        }
        let hash_bytes: [u8; 32] = data[40..72]
            .try_into()
            .map_err(|_| anyhow!("Failed to extract nonce hash bytes"))?;
        Ok(Hash::new_from_array(hash_bytes))
    }

    async fn claim(&self, cluster: &str, epoch: u64) -> anyhow::Result<()> {
        let rpc_client = self.get_rpc_client();
        let claimant = self.resolve_claimant()?;

        let distributor_pda = self.merkle_distributor_address(JITOSOL_MINT, epoch);
        let distributor_token_address = get_associated_token_address_with_program_id(
            &Pubkey::new_from_array(distributor_pda.to_bytes()),
            &JITOSOL_MINT,
            &spl_token_interface::id(),
        );

        let claim_status_pda = self.claim_status_address(claimant, distributor_pda);
        let claimant_token_address = get_associated_token_address_with_program_id(
            &claimant,
            &JITOSOL_MINT,
            &spl_token_interface::id(),
        );

        let url = format!(
            "https://storage.googleapis.com/jito-bam-boost/{cluster}/{epoch}/merkle_tree.json",
        );

        log::info!("Fetching merkle tree from: {}", url);

        // Download the merkle tree JSON from GCS
        let response = match reqwest::get(&url).await {
            Ok(resp) => resp,
            Err(e) => {
                log::error!("Failed to fetch merkle tree: {}", e);
                return Err(anyhow!("Failed to fetch merkle tree: {e}"));
            }
        };

        let response_json = match response.json().await {
            Ok(json) => json,
            Err(e) => {
                log::error!("Failed to parse merkle tree JSON response: {e}");
                return Err(anyhow!("Failed to parse merkle tree JSON response: {e}"));
            }
        };

        // Parse the merkle tree JSON (amounts are already in lamports, no conversion needed)
        let merkle_tree: BamBoostMerkleTree =
            match BamBoostMerkleTree::new_from_entries(response_json) {
                Ok(tree) => tree,
                Err(e) => {
                    log::error!("Failed to parse merkle tree: {e}");
                    return Err(anyhow!("Failed to parse merkle tree: {e}"));
                }
            };

        let node = merkle_tree.get_node(&claimant);

        let claim_status_pda = Pubkey::new_from_array(claim_status_pda.to_bytes());

        if rpc_client.get_account(&claim_status_pda).is_ok() {
            return Err(anyhow!("Claim status account already exists — subsidy for this epoch has already been claimed."));
        }

        let mut ix_builder = ClaimBuilder::new();
        ix_builder
            .distributor(Pubkey::new_from_array(distributor_pda.to_bytes()))
            .claim_status(claim_status_pda)
            .from(distributor_token_address)
            .to(claimant_token_address)
            .claimant(claimant)
            .token_program(spl_token_interface::id())
            .amount(node.amount)
            .proof(node.proof.unwrap());
        let mut ix = ix_builder.instruction();
        ix.program_id = self.bam_boost_program_id;

        log::info!("Claiming parameters: {ix_builder:?}");

        // Build instruction list: nonce advance (if any) -> lighthouse assert (if any) -> ATA create -> claim
        let mut ixs: Vec<Instruction> = Vec::new();

        // 1. Durable nonce advance (must be first instruction)
        if let Some(nonce_account) = self.nonce {
            let authority = self.nonce_authority.unwrap_or(nonce_account);
            ixs.push(
                solana_system_interface::instruction::advance_nonce_account(
                    &nonce_account,
                    &authority,
                ),
            );
        }

        // 2. Lighthouse deploy-slot assertion
        if let Some(slot) = self.assert_deploy_slot {
            ixs.push(build_assert_deploy_slot_ix(slot));
        }

        // 3. Create ATA (idempotent)
        ixs.push(create_associated_token_account_idempotent(
            &claimant,
            &claimant,
            &JITOSOL_MINT,
            &spl_token_interface::id(),
        ));

        // 4. Claim instruction
        ixs.push(ix);

        self.process_transaction(&ixs, &claimant)?;

        // Only check claim status when we actually sent the transaction
        if !self.should_print_tx() {
            let claim_status_acc = self
                .get_account::<ClaimStatus>(&Pubkey::new_from_array(claim_status_pda.to_bytes()))?;
            log::info!("ClaimStatus: {claim_status_acc:?}");
        }

        Ok(())
    }

    fn get_claim_status(&self, epoch: u64, claimant: Pubkey) -> anyhow::Result<()> {
        let distributor_pda = self.merkle_distributor_address(JITOSOL_MINT, epoch);

        let claim_status_pda = self.claim_status_address(claimant, distributor_pda);

        println!("ClaimStatus PDA: {claim_status_pda}");

        let account =
            self.get_account::<ClaimStatus>(&Pubkey::new_from_array(claim_status_pda.to_bytes()))?;

        println!("{}", serde_json::to_string_pretty(&account)?);

        Ok(())
    }

    /// Creates a new RPC client using the configuration from the CLI handler.
    ///
    /// This method constructs an RPC client with the URL and commitment level specified in the
    /// CLI configuration. The client can be used to communicate with a Solana node for
    /// submitting transactions, querying account data, and other RPC operations.
    fn get_rpc_client(&self) -> RpcClient {
        RpcClient::new_with_commitment(self.cli_config.rpc_url.clone(), self.cli_config.commitment)
    }

    /// Fetches and deserializes an account
    ///
    /// This method retrieves account data using the configured RPC client,
    /// then deserializes it into the specified account type using Borsh deserialization.
    fn get_account<T: BorshDeserialize>(&self, account_pubkey: &Pubkey) -> anyhow::Result<T> {
        let rpc_client = self.get_rpc_client();

        let account = rpc_client.get_account(account_pubkey)?;
        let account = T::deserialize(&mut account.data.as_slice())?;

        Ok(account)
    }

    /// Processes a transaction by either printing it as base58 or signing and sending it.
    ///
    /// When `should_print_tx()` is true (either --print-tx or --address mode):
    ///   - Builds an unsigned transaction
    ///   - Serializes it with bincode
    ///   - Prints the base58-encoded bytes to stdout
    ///   - Returns Ok(()) without signing or sending
    ///
    /// Otherwise, signs and sends the transaction normally.
    fn process_transaction(
        &self,
        ixs: &[Instruction],
        payer: &Pubkey,
    ) -> anyhow::Result<()> {
        let rpc_client = self.get_rpc_client();

        if self.should_print_tx() {
            // Build unsigned transaction
            let blockhash = if let Some(nonce_account) = self.nonce {
                let account_data = rpc_client.get_account(&nonce_account)?;
                Self::parse_nonce_hash(&account_data.data)?
            } else {
                rpc_client.get_latest_blockhash()?
            };

            let message = Message::new(ixs, Some(payer));
            let mut tx = Transaction::new_unsigned(message);
            tx.message.recent_blockhash = blockhash;

            let serialized = tx.message_data();
            let num_sigs = tx.message.header.num_required_signatures as usize;
            let mut wire: Vec<u8> = Vec::with_capacity(1 + num_sigs * 64 + serialized.len());
            wire.push(num_sigs as u8);
            for _ in 0..num_sigs {
                wire.extend_from_slice(&[0u8; 64]);
            }
            wire.extend_from_slice(&serialized);
            println!("{}", bs58::encode(&wire).into_string());

            return Ok(());
        }

        // Signed mode: requires a signer
        let signer = self
            .cli_config
            .signer
            .clone()
            .ok_or_else(|| anyhow!("signer is required to send transactions"))?;

        let blockhash = if let Some(nonce_account) = self.nonce {
            let account_data = rpc_client.get_account(&nonce_account)?;
            Self::parse_nonce_hash(&account_data.data)?
        } else {
            rpc_client.get_latest_blockhash()?
        };

        let tx = Transaction::new_signed_with_payer(ixs, Some(payer), &[&*signer], blockhash);
        let result = rpc_client.send_and_confirm_transaction(&tx)?;

        log::info!("Transaction confirmed: {:?}", result);

        Ok(())
    }
}
