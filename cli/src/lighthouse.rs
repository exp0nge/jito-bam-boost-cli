use solana_pubkey::{pubkey, Pubkey};
use solana_transaction::Instruction;

/// Lighthouse program ID
pub const LIGHTHOUSE_PROGRAM_ID: Pubkey = pubkey!("L2TExMFKdjpN9kozasaurPirfHy9P8sbXoAN1qA3S95");

/// BAM Boost ProgramData account (the upgradeable loader ProgramData PDA)
pub const BAM_BOOST_PROGRAM_DATA: Pubkey = pubkey!("jpyyQB22b4NaE4SddyzoNcSeUsUbGtBMgX9pBWdPPSr");

/// Discriminator for AssertUpgradeableLoaderAccount instruction in Lighthouse
const ASSERT_UPGRADEABLE_LOADER_ACCOUNT_DISCRIMINATOR: u8 = 13;

/// Build the instruction data for Lighthouse AssertUpgradeableLoaderAccount
/// with assertion: ProgramData { Slot { value: expected_slot, operator: Equal } }.
///
/// Wire format (all Borsh):
///   [0] discriminator = 13u8
///   [1] LogLevel::Silent = 0u8
///   [2] UpgradeableLoaderStateAssertion variant = 3u8 (ProgramData)
///   [3] UpgradeableProgramDataAssertion variant = 1u8 (Slot)
///   [4..12] value = expected_slot as u64 LE
///   [12] IntegerOperator::Equal = 0u8
fn build_assert_deploy_slot_data(expected_slot: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(13);
    // Instruction discriminator
    data.push(ASSERT_UPGRADEABLE_LOADER_ACCOUNT_DISCRIMINATOR);
    // LogLevel::Silent (variant index 0)
    data.push(0u8);
    // UpgradeableLoaderStateAssertion::ProgramData (variant index 3)
    data.push(3u8);
    // UpgradeableProgramDataAssertion::Slot (variant index 1)
    data.push(1u8);
    // value: u64 LE
    data.extend_from_slice(&expected_slot.to_le_bytes());
    // IntegerOperator::Equal (variant index 0)
    data.push(0u8);
    data
}

/// Build a Lighthouse `AssertUpgradeableLoaderAccount` instruction that asserts
/// the BAM Boost ProgramData account was deployed at the given slot.
pub fn build_assert_deploy_slot_ix(expected_slot: u64) -> Instruction {
    Instruction {
        program_id: LIGHTHOUSE_PROGRAM_ID,
        accounts: vec![solana_transaction::AccountMeta {
            pubkey: BAM_BOOST_PROGRAM_DATA,
            is_signer: false,
            is_writable: false,
        }],
        data: build_assert_deploy_slot_data(expected_slot),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lighthouse_ix_program_id() {
        let ix = build_assert_deploy_slot_ix(12345);
        assert_eq!(ix.program_id, LIGHTHOUSE_PROGRAM_ID);
    }

    #[test]
    fn test_lighthouse_ix_accounts() {
        let ix = build_assert_deploy_slot_ix(12345);
        assert_eq!(ix.accounts.len(), 1);
        assert_eq!(ix.accounts[0].pubkey, BAM_BOOST_PROGRAM_DATA);
        assert!(!ix.accounts[0].is_signer, "target account should not be a signer");
        assert!(!ix.accounts[0].is_writable, "target account should be readonly");
    }

    #[test]
    fn test_lighthouse_ix_data_not_empty() {
        let ix = build_assert_deploy_slot_ix(12345);
        assert!(!ix.data.is_empty());
        // First byte should be the discriminator
        assert_eq!(ix.data[0], ASSERT_UPGRADEABLE_LOADER_ACCOUNT_DISCRIMINATOR);
        // Total data length: 1 (disc) + 1 (log_level) + 1 (state_assertion variant)
        //   + 1 (program_data_assertion variant) + 8 (u64 slot) + 1 (operator) = 13
        assert_eq!(ix.data.len(), 13);
    }
}
