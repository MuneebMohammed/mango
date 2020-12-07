use std::mem::size_of;

use arrayref::{array_ref, array_refs};
use solana_program::account_info::AccountInfo;
use solana_program::entrypoint::ProgramResult;
use solana_program::program_error::ProgramError;
use solana_program::program_pack::Pack;
use solana_program::pubkey::Pubkey;
use solana_program::rent::Rent;
use solana_program::sysvar::Sysvar;
use spl_token::state::Account;

use crate::instruction::MangoInstruction;
use crate::state::{AccountFlag, Loadable, MangoGroup, NUM_MARKETS, NUM_TOKENS, MangoIndex};
use crate::utils::gen_signer_key;
use serum_dex::state::ToAlignedBytes;
use solana_program::clock::Clock;

pub struct Processor {}


impl Processor {
    fn init_mango_group(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        signer_nonce: u64
    ) -> ProgramResult {
        const NUM_FIXED: usize = 5;

        let accounts = array_ref![accounts, 0, NUM_FIXED + 2 * NUM_TOKENS + NUM_MARKETS];
        let (fixed_accs, token_mint_accs, vault_accs, spot_market_accs) =
            array_refs![accounts, NUM_FIXED, NUM_TOKENS, NUM_TOKENS, NUM_MARKETS];

        let [
            mango_group_acc,
            rent_acc,
            clock_acc,
            signer_acc,
            dex_prog_acc
        ] = fixed_accs;

        let rent = Rent::from_account_info(rent_acc)?;
        let clock = Clock::from_account_info(clock_acc)?;
        let mut mango_group = MangoGroup::load_mut(mango_group_acc)?;

        assert_eq!(mango_group_acc.owner, program_id);
        assert_eq!(mango_group.account_flags, 0);
        mango_group.account_flags = (AccountFlag::Initialized | AccountFlag::MangoGroup).bits();

        assert!(rent.is_exempt(mango_group_acc.lamports(), size_of::<MangoGroup>()));

        assert_eq!(gen_signer_key(signer_nonce, mango_group_acc.key, program_id)?, *signer_acc.key);
        mango_group.signer_nonce = signer_nonce;
        mango_group.signer_key = *signer_acc.key;

        let quote_mint_acc = &token_mint_accs[NUM_MARKETS];
        let quote_vault_acc = &vault_accs[NUM_MARKETS];
        let quote_vault = Account::unpack(&quote_vault_acc.try_borrow_data()?)?;
        assert_eq!(&quote_vault.owner, signer_acc.key);
        assert_eq!(&quote_vault.mint, quote_mint_acc.key);

        let curr_ts = clock.unix_timestamp as u64;

        for i in 0..NUM_MARKETS {
            let spot_market_acc = &spot_market_accs[i];
            let spot_market = serum_dex::state::MarketState::load(
                spot_market_acc, dex_prog_acc.key
            )?;
            let base_mint_acc = &token_mint_accs[i];
            let base_vault_acc = &vault_accs[i];
            let base_vault = Account::unpack(&base_vault_acc.try_borrow_data()?)?;
            assert_eq!(&base_vault.owner, signer_acc.key);
            assert_eq!(&base_vault.mint, base_mint_acc.key);

            let sm_base_mint = spot_market.coin_mint;
            let sm_quote_mint = spot_market.pc_mint;
            assert_eq!(sm_base_mint, base_mint_acc.key.to_aligned_bytes());
            assert_eq!(sm_quote_mint, quote_mint_acc.key.to_aligned_bytes());
            mango_group.spot_markets[i] = *spot_market_acc.key;
            mango_group.tokens[i] = *base_mint_acc.key;
            mango_group.vaults[i] = *base_vault_acc.key;

            // TODO what to initialize index to?
            mango_group.indexes[i] = MangoIndex {
                last_update: curr_ts,
                borrow: 1,
                lend: 1
            }
        }

        Ok(())
    }

    pub fn process(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        data: &[u8]
    ) -> ProgramResult {
        let instruction = MangoInstruction::unpack(data).ok_or(ProgramError::InvalidInstructionData)?;
        match instruction {
            MangoInstruction::InitMangoGroup {
                signer_nonce
            } => {
                Self::init_mango_group(program_id, accounts, signer_nonce)?;
            }
            MangoInstruction::InitMarginAccount => {}
            MangoInstruction::Deposit => {}
            MangoInstruction::Withdraw => {}
            MangoInstruction::Liquidate => {}
            MangoInstruction::PlaceOrder => {}
            MangoInstruction::SettleFunds => {}
            MangoInstruction::CancelOrder => {}
            MangoInstruction::CancelOrderByClientId => {}
        }
        Ok(())
    }
}

/*
TODO
Initial launch
- UI
- funding book
- we market make on the book
- liquidation bot
- cranks
- testing
 */

/*
Perp Bond
- cleaner
- no way to enforce loss on bond holders
- risk horizon is potentially infinite
-
 */

/*
FMB (Fixed Maturity Bond)
- enforcers keep a list of all who have liab balances and submit at settlement
- liab holders may set if they want auto roll and to which bond they want to auto roll
-

 */

/*
Lending Pool
- Enforcers periodically update index based on time past and interest rate
- https://docs.dydx.exchange/#interest
 */

/*
Dynamic Expansion



 */