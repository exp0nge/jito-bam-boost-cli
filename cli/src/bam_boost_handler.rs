use std::str::FromStr;

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

use serde_json::{json, Value};

use crate::{
    bam_boost::{BamBoostCommands, ClaimStatusActions, MerkleDistributorActions, NetworkArg},
    cli_args::{DeploySlotGuard, OutputFormat},
    cli_config::CliConfig,
    lighthouse::{build_assert_deploy_slot_ix, parse_program_data_slot, BAM_BOOST_PROGRAM_DATA},
    JITOSOL_MINT,
};

/// Tallies for a multi-epoch scan, used for the final summary line.
#[derive(Default)]
struct ScanCounters {
    claimed: u64,
    already: u64,
    no_tree: u64,
    not_allocated: u64,
    errored: u64,
}

/// The wire encodings of an unsigned transaction.
///
/// Deliberately carries *only* the serialized transaction — no epoch, amount,
/// claimant, blockhash, or guard metadata. A signer/automation must decode the
/// base58/base64 and verify what it is signing from the bytes themselves rather
/// than trusting sidecar fields that could disagree with the actual transaction
/// (e.g. `solana decode-transaction <tx> base64`).
struct UnsignedTx {
    base58: String,
    base64: String,
}

impl UnsignedTx {
    fn to_json(&self) -> Value {
        json!({
            "unsigned_tx_base58": self.base58,
            "unsigned_tx_base64": self.base64,
        })
    }
}

/// A durable nonce account paired with its authority.
type NonceSpec = (Pubkey, Pubkey);

/// Everything needed to build a claim for one epoch, once eligibility is
/// confirmed. Lets the scanner decide nonce assignment before building.
struct EligibleClaim {
    distributor_pda: Pubkey,
    claim_status_pda: Pubkey,
    distributor_token_address: Pubkey,
    claimant_token_address: Pubkey,
    claimant: Pubkey,
    amount: u64,
    proof: Vec<[u8; 32]>,
}

/// Outcome of checking (without building) whether an epoch can be claimed.
enum Eligibility {
    NoTree,
    NotAllocated,
    AlreadyClaimed,
    Eligible(Box<EligibleClaim>),
}

/// Parse a comma-separated list of base58 pubkeys. Whitespace is trimmed and
/// empty entries ignored; `None` or an empty/whitespace string yields `[]`.
pub fn parse_pubkey_list(s: Option<&str>) -> anyhow::Result<Vec<Pubkey>> {
    let Some(s) = s else {
        return Ok(Vec::new());
    };
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(|x| Pubkey::from_str(x).map_err(|e| anyhow!("invalid pubkey '{x}': {e}")))
        .collect()
}

/// Pair nonce accounts with authorities. The authority list may be empty (each
/// nonce authorizes itself), length 1 (applied to every nonce), or equal in
/// length to the nonces (paired 1:1). Any other length is an error.
pub fn resolve_nonce_specs(
    nonces: Vec<Pubkey>,
    authorities: Vec<Pubkey>,
) -> anyhow::Result<Vec<NonceSpec>> {
    if nonces.is_empty() {
        if !authorities.is_empty() {
            return Err(anyhow!("--nonce-authority given without --nonce"));
        }
        return Ok(Vec::new());
    }
    let pairs: Vec<NonceSpec> = match authorities.len() {
        0 => nonces.iter().map(|n| (*n, *n)).collect(),
        1 => nonces.iter().map(|n| (*n, authorities[0])).collect(),
        a if a == nonces.len() => nonces.into_iter().zip(authorities).collect(),
        a => {
            return Err(anyhow!(
                "--nonce-authority count ({a}) must be 0, 1, or equal to the --nonce count ({})",
                nonces.len()
            ))
        }
    };
    Ok(pairs)
}

/// Standard base64 (RFC 4648, with padding) — the encoding Solana RPC expects
/// for `sendTransaction` / `simulateTransaction` with `encoding: "base64"`.
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

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

    /// Lighthouse deploy-slot guard: auto-resolve from RPC, an explicit slot, or off
    assert_deploy_slot: DeploySlotGuard,

    /// Durable nonce accounts paired with their authorities. Empty unless
    /// --nonce was given. Durable nonces are single-use, so in scan mode one
    /// nonce is assigned per eligible epoch and the count must be >= the number
    /// of epochs to claim.
    nonces: Vec<NonceSpec>,

    /// Output format for built unsigned transactions
    output: OutputFormat,
}

impl BamBoostCliHandler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cli_config: CliConfig,
        bam_boost_program_id: Pubkey,
        print_tx: bool,
        print_json: bool,
        print_json_with_reserves: bool,
        assert_deploy_slot: DeploySlotGuard,
        nonces: Vec<NonceSpec>,
        output: OutputFormat,
    ) -> Self {
        Self {
            cli_config,
            bam_boost_program_id,
            print_tx,
            print_json,
            print_json_with_reserves,
            assert_deploy_slot,
            nonces,
            output,
        }
    }

    /// Emit the unsigned transactions built during a claim run in the selected
    /// output format. In `text` mode, prints one base58 transaction per line
    /// (the historical behaviour). In `json` mode, prints a single JSON array
    /// of `{unsigned_tx_base58, unsigned_tx_base64}` objects — and nothing else
    /// — for automation to consume. All logs go to stderr, so stdout stays clean.
    fn emit_unsigned(&self, txs: &[UnsignedTx]) {
        match self.output {
            OutputFormat::Text => {
                for tx in txs {
                    println!("{}", tx.base58);
                }
            }
            OutputFormat::Json => {
                let arr = Value::Array(txs.iter().map(UnsignedTx::to_json).collect());
                // Pretty-print; serialization of this Value cannot fail.
                println!(
                    "{}",
                    serde_json::to_string_pretty(&arr).unwrap_or_else(|_| "[]".to_string())
                );
            }
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

    /// Resolve the deploy-slot guard into a concrete expected slot (or `None`
    /// when disabled). In `auto` mode this reads the BAM Boost ProgramData
    /// account from RPC and uses its current deploy slot, so the claim rolls
    /// back if the program is upgraded between build and on-chain execution.
    fn resolve_assert_slot(&self) -> anyhow::Result<Option<u64>> {
        match &self.assert_deploy_slot {
            DeploySlotGuard::Off => Ok(None),
            DeploySlotGuard::Slot(slot) => Ok(Some(*slot)),
            DeploySlotGuard::Auto => {
                let account = self.get_rpc_client().get_account(&BAM_BOOST_PROGRAM_DATA)?;
                let slot = parse_program_data_slot(&account.data)?;
                log::info!(
                    "Auto-resolved BAM Boost ProgramData deploy slot to {slot} (Lighthouse guard enabled)"
                );
                Ok(Some(slot))
            }
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
                action:
                    MerkleDistributorActions::Claim {
                        network,
                        epoch,
                        first_epoch,
                    },
            } => {
                let network = match network {
                    NetworkArg::Mainnet => "mainnet",
                    NetworkArg::Testnet => "testnet",
                };

                match epoch {
                    Some(epoch) => self.claim(network, epoch).await,
                    None => self.claim_all(network, first_epoch).await,
                }
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

    /// Claim a single, explicitly requested epoch.
    ///
    /// Unlike scan-all mode, a not-eligible or already-claimed epoch here is
    /// surfaced as an error since the user asked for that specific epoch.
    async fn claim(&self, cluster: &str, epoch: u64) -> anyhow::Result<()> {
        if self.nonces.len() > 1 {
            return Err(anyhow!(
                "{} nonce accounts given for a single-epoch claim; pass at most one --nonce.",
                self.nonces.len()
            ));
        }
        let assert_slot = self.resolve_assert_slot()?;
        match self.check_eligibility(cluster, epoch).await? {
            Eligibility::Eligible(ec) => {
                let unsigned = self.build_claim(&ec, assert_slot, self.nonces.first().copied())?;
                let txs: Vec<UnsignedTx> = unsigned.into_iter().collect();
                self.emit_unsigned(&txs);
                Ok(())
            }
            Eligibility::AlreadyClaimed => Err(anyhow!(
                "Claim status account already exists — subsidy for epoch {epoch} has already been claimed."
            )),
            Eligibility::NoTree => Err(anyhow!("No merkle tree is published for epoch {epoch}.")),
            Eligibility::NotAllocated => Err(anyhow!(
                "No claimable BAM Boost rewards for this claimant in epoch {epoch}."
            )),
        }
    }

    /// Scan a range of epochs and claim each one the claimant is eligible for.
    ///
    /// When `first_epoch` is `Some`, scans `first_epoch..=current` ascending.
    /// When `None`, the start is auto-discovered by walking back from the
    /// current epoch until the bottom of the contiguous published-tree range is
    /// found (see [`Self::discover_first_epoch`]) — i.e. it claims back as far
    /// as merkle trees exist, with no hardcoded launch epoch.
    ///
    /// Epochs with no published tree, no allocation, or an existing claim are
    /// skipped; per-epoch errors are logged and do not abort the scan.
    ///
    /// Durable nonces are single-use, so when `--nonce` is given one nonce is
    /// assigned per eligible epoch. If there are more eligible epochs than nonce
    /// accounts, the run aborts before building anything (fail-closed) rather
    /// than emitting transactions that could never all land.
    async fn claim_all(&self, cluster: &str, first_epoch: Option<u64>) -> anyhow::Result<()> {
        // Scan through the current epoch inclusive: BAM Boost publishes a tree
        // for an epoch around the time it ends, so the current epoch's tree may
        // already be claimable.
        let end_epoch = self.get_rpc_client().get_epoch_info()?.epoch;

        let start_epoch = match first_epoch {
            Some(fe) => {
                if fe > end_epoch {
                    return Err(anyhow!(
                        "first-epoch {fe} is beyond the current epoch {end_epoch}"
                    ));
                }
                fe
            }
            None => {
                log::info!(
                    "No --first-epoch given; walking back from epoch {end_epoch} to find the earliest published merkle tree…"
                );
                let discovered = self.discover_first_epoch(cluster, end_epoch).await?;
                log::info!(
                    "Auto-discovered earliest claimable epoch: {discovered} (claiming back {} epochs)",
                    end_epoch - discovered + 1
                );
                discovered
            }
        };

        log::info!("Scanning epochs {start_epoch}..={end_epoch}");

        // Resolve the deploy-slot guard once for the whole scan.
        let assert_slot = self.resolve_assert_slot()?;

        // Pass 1: determine eligibility for every epoch (no transactions built
        // yet), so nonces can be assigned and validated before any work.
        let mut counters = ScanCounters::default();
        let mut eligible: Vec<(u64, EligibleClaim)> = Vec::new();
        for epoch in start_epoch..=end_epoch {
            match self.check_eligibility(cluster, epoch).await {
                Ok(Eligibility::Eligible(ec)) => eligible.push((epoch, *ec)),
                Ok(Eligibility::AlreadyClaimed) => {
                    counters.already += 1;
                    log::info!("Epoch {epoch}: already claimed");
                }
                Ok(Eligibility::NoTree) => {
                    counters.no_tree += 1;
                    log::info!("Epoch {epoch}: no tree published");
                }
                Ok(Eligibility::NotAllocated) => {
                    counters.not_allocated += 1;
                    log::info!("Epoch {epoch}: no allocation for claimant");
                }
                Err(e) => {
                    counters.errored += 1;
                    log::error!("Epoch {epoch}: eligibility check FAILED — {e}");
                }
            }
        }

        // Fail closed: a durable nonce can authorize exactly one transaction, so
        // we need at least one nonce per eligible epoch. Abort before building.
        let nonce_mode = !self.nonces.is_empty();
        if nonce_mode && eligible.len() > self.nonces.len() {
            let epochs: Vec<String> = eligible.iter().map(|(e, _)| e.to_string()).collect();
            return Err(anyhow!(
                "{} eligible epochs ({}) but only {} durable nonce account(s) provided. \
                 Durable nonces are single-use — provide one --nonce per claim, narrow the \
                 range with --first-epoch/--epoch, or omit --nonce to use a recent blockhash.",
                eligible.len(),
                epochs.join(", "),
                self.nonces.len()
            ));
        }

        // Pass 2: build (and sign/send, unless in print mode) each eligible
        // claim, assigning one nonce per epoch when in nonce mode.
        let mut unsigned_txs: Vec<UnsignedTx> = Vec::new();
        for (i, (epoch, ec)) in eligible.iter().enumerate() {
            let nonce = if nonce_mode {
                Some(self.nonces[i])
            } else {
                None
            };
            match self.build_claim(ec, assert_slot, nonce) {
                Ok(unsigned) => {
                    counters.claimed += 1;
                    log::info!("Epoch {epoch}: CLAIMED");
                    unsigned_txs.extend(unsigned);
                }
                Err(e) => {
                    counters.errored += 1;
                    log::error!("Epoch {epoch}: build FAILED — {e}");
                }
            }
        }

        log::info!(
            "Scan complete: {} claimed, {} already claimed, {} no tree, {} not allocated, {} errors (epochs {start_epoch}..={end_epoch})",
            counters.claimed, counters.already, counters.no_tree, counters.not_allocated, counters.errored
        );

        // Emit in one batch (a single JSON array in json mode) so automation
        // gets one document. Order follows the ascending epoch scan.
        self.emit_unsigned(&unsigned_txs);

        Ok(())
    }

    /// Walk backward from `end_epoch` to find the earliest epoch with a
    /// published merkle tree (the bottom of the contiguous claimable range).
    ///
    /// Probes each epoch's tree on GCS with a cheap `HEAD` request. Tolerates
    /// small gaps inside the range, and stops once it has dropped
    /// `MISSING_TOLERANCE` consecutive epochs below the lowest tree seen. Errors
    /// out if no tree is found within `MAX_INITIAL_LOOKBACK` epochs of the top
    /// (which usually means the wrong network or GCS being unreachable).
    async fn discover_first_epoch(&self, cluster: &str, end_epoch: u64) -> anyhow::Result<u64> {
        /// Consecutive missing trees, below the lowest seen, that mark the bottom.
        const MISSING_TOLERANCE: u64 = 3;
        /// How far below the current epoch to look before concluding there are
        /// no trees at all (guards against GCS outages / wrong cluster).
        const MAX_INITIAL_LOOKBACK: u64 = 50;

        let mut lowest_with_tree: Option<u64> = None;
        let mut consecutive_missing = 0u64;
        let mut epoch = end_epoch;

        loop {
            if self.merkle_tree_exists(cluster, epoch).await {
                lowest_with_tree = Some(epoch);
                consecutive_missing = 0;
            } else {
                consecutive_missing += 1;
                match lowest_with_tree {
                    // Below the discovered range: tolerate a few gaps, then stop.
                    Some(_) if consecutive_missing >= MISSING_TOLERANCE => break,
                    // Not yet found any tree near the top: give up after a while.
                    None if consecutive_missing >= MAX_INITIAL_LOOKBACK => {
                        return Err(anyhow!(
                            "No published merkle tree found within {MAX_INITIAL_LOOKBACK} epochs below {end_epoch} (cluster: {cluster}). Check the network/RPC, or pass --first-epoch explicitly."
                        ));
                    }
                    _ => {}
                }
            }

            if epoch == 0 {
                break;
            }
            epoch -= 1;
        }

        lowest_with_tree.ok_or_else(|| {
            anyhow!("Could not discover any claimable epoch walking back from {end_epoch}")
        })
    }

    /// Cheap existence check for an epoch's merkle tree on GCS via `HEAD`.
    async fn merkle_tree_exists(&self, cluster: &str, epoch: u64) -> bool {
        let url = format!(
            "https://storage.googleapis.com/jito-bam-boost/{cluster}/{epoch}/merkle_tree.json",
        );
        match reqwest::Client::new().head(&url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(e) => {
                log::warn!("HEAD probe failed for epoch {epoch}: {e}");
                false
            }
        }
    }

    /// Check — without building a transaction — whether the claimant can claim
    /// this epoch. Fetches the merkle tree and reads the claim-status account.
    /// Non-fatal conditions are reported via [`Eligibility`]; only genuine
    /// errors (network/parse failures) return `Err`.
    async fn check_eligibility(&self, cluster: &str, epoch: u64) -> anyhow::Result<Eligibility> {
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

        // A missing tree (404, etc.) means no subsidy was published for this epoch.
        if !response.status().is_success() {
            log::info!("No merkle tree published for epoch {epoch} (HTTP {})", response.status());
            return Ok(Eligibility::NoTree);
        }

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

        // The claimant may not be in this epoch's tree at all.
        let node = match merkle_tree.convert_to_hashmap().get(&claimant) {
            Some(node) => node.clone(),
            None => {
                log::info!("Claimant {claimant} has no allocation in epoch {epoch}");
                return Ok(Eligibility::NotAllocated);
            }
        };

        let claim_status_pda = Pubkey::new_from_array(claim_status_pda.to_bytes());

        if rpc_client.get_account(&claim_status_pda).is_ok() {
            log::info!("Epoch {epoch} already claimed (claim status account exists)");
            return Ok(Eligibility::AlreadyClaimed);
        }

        let proof = node
            .proof
            .clone()
            .ok_or_else(|| anyhow!("Merkle node for epoch {epoch} is missing its proof"))?;

        Ok(Eligibility::Eligible(Box::new(EligibleClaim {
            distributor_pda: Pubkey::new_from_array(distributor_pda.to_bytes()),
            claim_status_pda,
            distributor_token_address,
            claimant_token_address,
            claimant,
            amount: node.amount,
            proof,
        })))
    }

    /// Build the claim transaction for an already-confirmed-eligible epoch and
    /// either send it (signed mode) or return it unsigned (print/offline mode).
    /// `nonce` is the durable nonce (account, authority) assigned to this claim,
    /// or `None` to use a recent blockhash.
    fn build_claim(
        &self,
        ec: &EligibleClaim,
        assert_slot: Option<u64>,
        nonce: Option<NonceSpec>,
    ) -> anyhow::Result<Option<UnsignedTx>> {
        let mut ix_builder = ClaimBuilder::new();
        ix_builder
            .distributor(ec.distributor_pda)
            .claim_status(ec.claim_status_pda)
            .from(ec.distributor_token_address)
            .to(ec.claimant_token_address)
            .claimant(ec.claimant)
            .token_program(spl_token_interface::id())
            .amount(ec.amount)
            .proof(ec.proof.clone());
        let mut ix = ix_builder.instruction();
        ix.program_id = self.bam_boost_program_id;

        log::info!("Claiming parameters: {ix_builder:?}");

        // Build instruction list: nonce advance (if any) -> lighthouse assert (if any) -> ATA create -> claim
        let mut ixs: Vec<Instruction> = Vec::new();

        // 1. Durable nonce advance (must be first instruction)
        if let Some((nonce_account, authority)) = nonce {
            ixs.push(
                solana_system_interface::instruction::advance_nonce_account(
                    &nonce_account,
                    &authority,
                ),
            );
        }

        // 2. Lighthouse deploy-slot assertion
        if let Some(slot) = assert_slot {
            ixs.push(build_assert_deploy_slot_ix(slot));
        }

        // 3. Create ATA (idempotent)
        ixs.push(create_associated_token_account_idempotent(
            &ec.claimant,
            &ec.claimant,
            &JITOSOL_MINT,
            &spl_token_interface::id(),
        ));

        // 4. Claim instruction
        ixs.push(ix);

        let nonce_account = nonce.map(|(n, _)| n);
        // Signed mode signs and sends, returning None. Print/offline mode returns
        // the unsigned transaction (base58/base64) and nothing else — the signer
        // verifies details by decoding the bytes, not from sidecar metadata.
        let built = self.process_transaction(&ixs, &ec.claimant, nonce_account)?;

        // Only check claim status when we actually sent the transaction
        if !self.should_print_tx() {
            let claim_status_acc = self.get_account::<ClaimStatus>(&Pubkey::new_from_array(
                ec.claim_status_pda.to_bytes(),
            ))?;
            log::info!("ClaimStatus: {claim_status_acc:?}");
        }

        Ok(built)
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

    /// Processes a transaction by either building it unsigned or signing and sending it.
    ///
    /// When `should_print_tx()` is true (either --print-tx or --address mode):
    ///   - Builds an unsigned transaction
    ///   - Returns its base58 + base64 wire encodings via [`UnsignedTx`]
    ///     (the caller decides how to emit it)
    ///
    /// Otherwise, signs and sends the transaction normally and returns `None`.
    fn process_transaction(
        &self,
        ixs: &[Instruction],
        payer: &Pubkey,
        nonce_account: Option<Pubkey>,
    ) -> anyhow::Result<Option<UnsignedTx>> {
        let rpc_client = self.get_rpc_client();

        if self.should_print_tx() {
            // Build unsigned transaction
            let blockhash = if let Some(nonce_account) = nonce_account {
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

            return Ok(Some(UnsignedTx {
                base58: bs58::encode(&wire).into_string(),
                base64: base64_encode(&wire),
            }));
        }

        // Signed mode: requires a signer
        let signer = self
            .cli_config
            .signer
            .clone()
            .ok_or_else(|| anyhow!("signer is required to send transactions"))?;

        let blockhash = if let Some(nonce_account) = nonce_account {
            let account_data = rpc_client.get_account(&nonce_account)?;
            Self::parse_nonce_hash(&account_data.data)?
        } else {
            rpc_client.get_latest_blockhash()?
        };

        let tx = Transaction::new_signed_with_payer(ixs, Some(payer), &[&*signer], blockhash);
        let result = rpc_client.send_and_confirm_transaction(&tx)?;

        log::info!("Transaction confirmed: {:?}", result);

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::{base64_encode, parse_pubkey_list, resolve_nonce_specs, UnsignedTx};
    use solana_pubkey::Pubkey;

    // Distinct, valid base58 pubkeys for tests.
    const A: &str = "11111111111111111111111111111111";
    const B: &str = "So11111111111111111111111111111111111111112";
    const C: &str = "Boostxbpp2ENYHGcTLYt1obpcY13HE4NojdqNWdzqSSb";

    fn pk(s: &str) -> Pubkey {
        s.parse().unwrap()
    }

    #[test]
    fn test_parse_pubkey_list_empty_and_none() {
        assert!(parse_pubkey_list(None).unwrap().is_empty());
        assert!(parse_pubkey_list(Some("")).unwrap().is_empty());
        assert!(parse_pubkey_list(Some("   ")).unwrap().is_empty());
    }

    #[test]
    fn test_parse_pubkey_list_trims_and_skips_blanks() {
        let list = parse_pubkey_list(Some(&format!(" {A} , {B} ,, {C} "))).unwrap();
        assert_eq!(list, vec![pk(A), pk(B), pk(C)]);
    }

    #[test]
    fn test_parse_pubkey_list_rejects_invalid() {
        assert!(parse_pubkey_list(Some("not-a-pubkey")).is_err());
        assert!(parse_pubkey_list(Some(&format!("{A},nope"))).is_err());
    }

    #[test]
    fn test_resolve_nonce_specs_empty() {
        assert!(resolve_nonce_specs(vec![], vec![]).unwrap().is_empty());
    }

    #[test]
    fn test_resolve_nonce_specs_authority_given_without_nonce_errors() {
        assert!(resolve_nonce_specs(vec![], vec![pk(A)]).is_err());
    }

    #[test]
    fn test_resolve_nonce_specs_zero_authorities_defaults_to_self() {
        let specs = resolve_nonce_specs(vec![pk(A), pk(B)], vec![]).unwrap();
        assert_eq!(specs, vec![(pk(A), pk(A)), (pk(B), pk(B))]);
    }

    #[test]
    fn test_resolve_nonce_specs_one_authority_applies_to_all() {
        let specs = resolve_nonce_specs(vec![pk(A), pk(B)], vec![pk(C)]).unwrap();
        assert_eq!(specs, vec![(pk(A), pk(C)), (pk(B), pk(C))]);
    }

    #[test]
    fn test_resolve_nonce_specs_paired_one_to_one() {
        let specs = resolve_nonce_specs(vec![pk(A), pk(B)], vec![pk(B), pk(C)]).unwrap();
        assert_eq!(specs, vec![(pk(A), pk(B)), (pk(B), pk(C))]);
    }

    #[test]
    fn test_resolve_nonce_specs_mismatched_count_errors() {
        // 3 nonces but 2 authorities (not 0/1/3) -> error.
        let err = resolve_nonce_specs(vec![pk(A), pk(B), pk(C)], vec![pk(A), pk(B)]).unwrap_err();
        assert!(err.to_string().contains("must be 0, 1, or equal"));
    }

    #[test]
    fn test_base64_encode_rfc4648_vectors() {
        // Standard RFC 4648 test vectors.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn test_base64_encode_binary() {
        assert_eq!(base64_encode(&[0u8, 0, 0]), "AAAA");
        assert_eq!(base64_encode(&[255u8, 255, 255]), "////");
        assert_eq!(base64_encode(&[0xfb, 0xff, 0xbf]), "+/+/");
    }

    #[test]
    fn test_unsigned_tx_json_carries_only_encodings() {
        let tx = UnsignedTx {
            base58: "B58TX".to_string(),
            base64: "B64TX==".to_string(),
        };
        let v = tx.to_json();
        let obj = v.as_object().expect("manifest entry must be a JSON object");

        // Exactly the two encoding fields — no epoch/claimant/amount/blockhash/etc.
        assert_eq!(
            obj.len(),
            2,
            "manifest entry must contain only the tx encodings, got: {obj:?}"
        );
        assert_eq!(obj["unsigned_tx_base58"], "B58TX");
        assert_eq!(obj["unsigned_tx_base64"], "B64TX==");

        // Guard against regressions reintroducing sidecar metadata.
        for k in [
            "epoch",
            "claimant",
            "amount_lamports",
            "recent_blockhash",
            "uses_nonce",
            "assert_deploy_slot",
        ] {
            assert!(!obj.contains_key(k), "manifest must not include '{k}'");
        }
    }
}
