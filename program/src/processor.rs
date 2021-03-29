use std::cmp;
use std::mem::size_of;

use arrayref::{array_ref, array_refs};
use fixed::types::U64F64;
use flux_aggregator::borsh_state::InitBorshState;
use serum_dex::matching::Side;
use serum_dex::state::ToAlignedBytes;
use solana_program::account_info::AccountInfo;
use solana_program::clock::Clock;
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::msg;
use solana_program::program_error::ProgramError;
use solana_program::program_pack::{IsInitialized, Pack};
use solana_program::pubkey::Pubkey;
use solana_program::rent::Rent;
use solana_program::sysvar::{Sysvar};
use spl_token::state::{Account, Mint};

use crate::error::{check_assert, MangoResult, SourceFileId};
use crate::instruction::{MangoInstruction};
use crate::state::{AccountFlag, check_open_orders, load_market_state, load_open_orders, Loadable, MangoGroup, MangoIndex, MarginAccount, NUM_MARKETS, NUM_TOKENS, MangoSrmAccount};
use crate::utils::{gen_signer_key, gen_signer_seeds};
use solana_program::entrypoint::ProgramResult;

macro_rules! prog_assert {
    ($cond:expr) => {
        check_assert($cond, line!() as u16, SourceFileId::Processor)
    }
}
macro_rules! prog_assert_eq {
    ($x:expr, $y:expr) => {
        check_assert($x == $y, line!() as u16, SourceFileId::Processor)
    }
}

mod srm_token {
    use solana_program::declare_id;
    #[cfg(feature = "devnet")]
    declare_id!("9FbAMDvXqNjPqZSYt4EWTguJuDrGkfvwr3gSFpiSbX9S");
    #[cfg(not(feature = "devnet"))]
    declare_id!("SRMuApVNdxXokk5GT7XD5cUUgXMBCoAz2LHeuAoKWRt");
}

pub struct Processor {}

impl Processor {
    #[inline(never)]
    fn init_mango_group(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        signer_nonce: u64,
        maint_coll_ratio: U64F64,
        init_coll_ratio: U64F64,
        borrow_limits: [u64; NUM_TOKENS]
    ) -> MangoResult<()> {
        const NUM_FIXED: usize = 7;
        let accounts = array_ref![accounts, 0, NUM_FIXED + 2 * NUM_TOKENS + 2 * NUM_MARKETS];
        let (
            fixed_accs,
            token_mint_accs,
            vault_accs,
            spot_market_accs,
            oracle_accs,
        ) = array_refs![accounts, NUM_FIXED, NUM_TOKENS, NUM_TOKENS, NUM_MARKETS, NUM_MARKETS];

        let [
            mango_group_acc,
            rent_acc,
            clock_acc,
            signer_acc,
            dex_prog_acc,
            srm_vault_acc,
            admin_acc
        ] = fixed_accs;

        // Note: no need to check rent and clock because they're being checked in from_account_info
        let rent = Rent::from_account_info(rent_acc)?;
        let clock = Clock::from_account_info(clock_acc)?;

        // TODO this may not be necessary since load_mut maps the data and will fail if size incorrect
        prog_assert_eq!(size_of::<MangoGroup>(), mango_group_acc.data_len())?;
        let mut mango_group = MangoGroup::load_mut(mango_group_acc)?;

        prog_assert_eq!(mango_group_acc.owner, program_id)?;
        prog_assert_eq!(mango_group.account_flags, 0)?;
        mango_group.account_flags = (AccountFlag::Initialized | AccountFlag::MangoGroup).bits();

        prog_assert!(rent.is_exempt(mango_group_acc.lamports(), size_of::<MangoGroup>()))?;
        prog_assert_eq!(gen_signer_key(signer_nonce, mango_group_acc.key, program_id)?, *signer_acc.key)?;
        mango_group.signer_nonce = signer_nonce;
        mango_group.signer_key = *signer_acc.key;
        mango_group.dex_program_id = *dex_prog_acc.key;
        mango_group.maint_coll_ratio = maint_coll_ratio;
        mango_group.init_coll_ratio = init_coll_ratio;

        // verify SRM vault is valid then set
        let srm_vault = Account::unpack(&srm_vault_acc.try_borrow_data()?)?;
        prog_assert!(srm_vault.is_initialized())?;
        prog_assert_eq!(&srm_vault.owner, signer_acc.key)?;
        prog_assert_eq!(srm_token::ID, srm_vault.mint)?;
        prog_assert_eq!(srm_vault_acc.owner, &spl_token::id())?;
        mango_group.srm_vault = *srm_vault_acc.key;

        // Set the admin key and make sure it's a signer
        prog_assert!(admin_acc.is_signer)?;
        mango_group.admin = *admin_acc.key;
        mango_group.borrow_limits = borrow_limits;

        let curr_ts = clock.unix_timestamp as u64;
        for i in 0..NUM_TOKENS {
            let mint_acc = &token_mint_accs[i];
            let mint = Mint::unpack(&mint_acc.try_borrow_data()?)?;
            let vault_acc = &vault_accs[i];
            let vault = Account::unpack(&vault_acc.try_borrow_data()?)?;
            prog_assert!(vault.is_initialized())?;
            prog_assert_eq!(&vault.owner, signer_acc.key)?;
            prog_assert_eq!(&vault.mint, mint_acc.key)?;
            prog_assert_eq!(vault_acc.owner, &spl_token::id())?;
            mango_group.tokens[i] = *mint_acc.key;
            mango_group.vaults[i] = *vault_acc.key;
            mango_group.indexes[i] = MangoIndex {
                last_update: curr_ts,
                borrow: U64F64::from_num(1),
                deposit: U64F64::from_num(1)  // Smallest unit of interest is 0.0001% or 0.000001
            };
            mango_group.mint_decimals[i] = mint.decimals;
        }

        for i in 0..NUM_MARKETS {
            let spot_market_acc: &AccountInfo = &spot_market_accs[i];
            let spot_market = load_market_state(
                spot_market_acc, dex_prog_acc.key
            )?;
            let sm_base_mint = spot_market.coin_mint;
            let sm_quote_mint = spot_market.pc_mint;
            prog_assert_eq!(sm_base_mint, token_mint_accs[i].key.to_aligned_bytes())?;
            prog_assert_eq!(sm_quote_mint, token_mint_accs[NUM_MARKETS].key.to_aligned_bytes())?;
            mango_group.spot_markets[i] = *spot_market_acc.key;
            mango_group.oracles[i] = *oracle_accs[i].key;

            let oracle = flux_aggregator::state::Aggregator::load_initialized(&oracle_accs[i])?;
            mango_group.oracle_decimals[i] = oracle.config.decimals;
        }

        Ok(())
    }

    #[inline(never)]
    fn init_margin_account(
        program_id: &Pubkey,
        accounts: &[AccountInfo]
    ) -> MangoResult<()> {
        const NUM_FIXED: usize = 4;
        let accounts = array_ref![accounts, 0, NUM_FIXED];

        let [
            mango_group_acc,
            margin_account_acc,
            owner_acc,
            rent_acc
        ] = accounts;

        let _mango_group = MangoGroup::load_checked(mango_group_acc, program_id)?;
        let mut margin_account = MarginAccount::load_mut(margin_account_acc)?;
        let rent = Rent::from_account_info(rent_acc)?;

        prog_assert_eq!(margin_account_acc.owner, program_id)?;
        prog_assert!(rent.is_exempt(margin_account_acc.lamports(), size_of::<MarginAccount>()))?;
        prog_assert_eq!(margin_account.account_flags, 0)?;
        prog_assert!(owner_acc.is_signer)?;

        margin_account.account_flags = (AccountFlag::Initialized | AccountFlag::MarginAccount).bits();
        margin_account.mango_group = *mango_group_acc.key;
        margin_account.owner = *owner_acc.key;

        Ok(())
    }

    #[inline(never)]
    fn deposit(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        quantity: u64
    ) -> MangoResult<()> {
        const NUM_FIXED: usize = 7;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            mango_group_acc,
            margin_account_acc,
            owner_acc,
            token_account_acc,
            vault_acc,
            token_prog_acc,
            clock_acc,
        ] = accounts;

        let mut mango_group = MangoGroup::load_mut_checked(mango_group_acc, program_id)?;
        let mut margin_account = MarginAccount::load_mut_checked(
            program_id, margin_account_acc, mango_group_acc.key
        )?;

        let clock = Clock::from_account_info(clock_acc)?;
        mango_group.update_indexes(&clock)?;

        let token_index = mango_group.get_token_index_with_vault(vault_acc.key).unwrap();
        prog_assert_eq!(&mango_group.vaults[token_index], vault_acc.key)?;

        prog_assert_eq!(token_prog_acc.key, &spl_token::id())?;
        let deposit_instruction = spl_token::instruction::transfer(
            &spl_token::id(),
            token_account_acc.key,
            vault_acc.key,
            &owner_acc.key, &[], quantity
        )?;
        let deposit_accs = [
            token_account_acc.clone(),
            vault_acc.clone(),
            owner_acc.clone(),
            token_prog_acc.clone()
        ];

        solana_program::program::invoke_signed(&deposit_instruction, &deposit_accs, &[])?;

        let deposit: U64F64 = U64F64::from_num(quantity) / mango_group.indexes[token_index].deposit;
        checked_add_deposit(&mut mango_group, &mut margin_account, token_index, deposit)?;

        Ok(())
    }

    #[inline(never)]
    fn withdraw(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        quantity: u64
    ) -> MangoResult<()> {
        const NUM_FIXED: usize = 8;
        let accounts = array_ref![accounts, 0, NUM_FIXED + 2 * NUM_MARKETS];
        let (
            fixed_accs,
            open_orders_accs,
            oracle_accs,
        ) = array_refs![accounts, NUM_FIXED, NUM_MARKETS, NUM_MARKETS];

        let [
            mango_group_acc,
            margin_account_acc,
            owner_acc,
            token_account_acc,
            vault_acc,
            signer_acc,
            token_prog_acc,
            clock_acc,
        ] = fixed_accs;

        let mut mango_group = MangoGroup::load_mut_checked(
            mango_group_acc, program_id
        )?;
        let mut margin_account = MarginAccount::load_mut_checked(
            program_id, margin_account_acc, mango_group_acc.key
        )?;
        let clock = Clock::from_account_info(clock_acc)?;
        mango_group.update_indexes(&clock)?;

        prog_assert!(owner_acc.is_signer)?;
        prog_assert_eq!(&margin_account.owner, owner_acc.key)?;

        for i in 0..NUM_MARKETS {
            prog_assert_eq!(open_orders_accs[i].key, &margin_account.open_orders[i])?;
            check_open_orders(&open_orders_accs[i], signer_acc.key)?;
        }

        let token_index = mango_group.get_token_index_with_vault(vault_acc.key).unwrap();
        prog_assert_eq!(&mango_group.vaults[token_index], vault_acc.key)?;

        let index: &MangoIndex = &mango_group.indexes[token_index];
        let native_deposits: u64 = (margin_account.deposits[token_index] * index.deposit).to_num();
        let available = native_deposits;

        prog_assert!(available >= quantity)?;
        // TODO just borrow (quantity - available)

        let prices = get_prices(&mango_group, oracle_accs)?;

        // Withdraw from deposit
        let withdrew: U64F64 = U64F64::from_num(quantity) / index.deposit;
        checked_sub_deposit(&mut mango_group, &mut margin_account, token_index, withdrew)?;

        // Make sure accounts are in valid state after withdrawal
        let coll_ratio = margin_account.get_collateral_ratio(&mango_group, &prices, open_orders_accs)?;
        prog_assert!(coll_ratio >= mango_group.init_coll_ratio)?;
        prog_assert!(mango_group.has_valid_deposits_borrows(token_index))?;

        // Send out withdraw instruction to SPL token program
        prog_assert_eq!(token_prog_acc.key, &spl_token::id())?;
        let withdraw_instruction = spl_token::instruction::transfer(
            &spl_token::ID,
            vault_acc.key,
            token_account_acc.key,
            signer_acc.key,
            &[],
            quantity
        )?;
        let withdraw_accs = [
            vault_acc.clone(),
            token_account_acc.clone(),
            signer_acc.clone(),
            token_prog_acc.clone()
        ];
        let signer_seeds = gen_signer_seeds(&mango_group.signer_nonce, mango_group_acc.key);
        solana_program::program::invoke_signed(&withdraw_instruction, &withdraw_accs, &[&signer_seeds])?;
        Ok(())
    }

    #[inline(never)]
    fn borrow(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        token_index: usize,
        quantity: u64
    ) -> MangoResult<()> {
        const NUM_FIXED: usize = 4;
        let accounts = array_ref![accounts, 0, NUM_FIXED + 2 * NUM_MARKETS];
        let (
            fixed_accs,
            open_orders_accs,
            oracle_accs,
        ) = array_refs![accounts, NUM_FIXED, NUM_MARKETS, NUM_MARKETS];

        let [
            mango_group_acc,
            margin_account_acc,
            owner_acc,
            clock_acc,
        ] = fixed_accs;

        let mut mango_group = MangoGroup::load_mut_checked(mango_group_acc, program_id)?;
        let mut margin_account = MarginAccount::load_mut_checked(
            program_id, margin_account_acc, mango_group_acc.key
        )?;
        prog_assert!(owner_acc.is_signer)?;
        prog_assert_eq!(&margin_account.owner, owner_acc.key)?;

        for i in 0..NUM_MARKETS {
            prog_assert_eq!(open_orders_accs[i].key, &margin_account.open_orders[i])?;
            check_open_orders(&open_orders_accs[i], &mango_group.signer_key)?;
        }
        let clock = Clock::from_account_info(clock_acc)?;
        mango_group.update_indexes(&clock)?;

        let index: MangoIndex = mango_group.indexes[token_index];

        let borrow = U64F64::from_num(quantity) / index.borrow;
        let deposit = U64F64::from_num(quantity) / index.deposit;

        checked_add_deposit(&mut mango_group, &mut margin_account, token_index, deposit)?;
        checked_add_borrow(&mut mango_group, &mut margin_account, token_index, borrow)?;

        let prices = get_prices(&mango_group, oracle_accs)?;
        let coll_ratio = margin_account.get_collateral_ratio(&mango_group, &prices, open_orders_accs)?;

        prog_assert!(coll_ratio >= mango_group.init_coll_ratio)?;
        prog_assert!(mango_group.has_valid_deposits_borrows(token_index))?;
        prog_assert!(margin_account.get_native_borrow(&index, token_index) <= mango_group.borrow_limits[token_index])?;
        Ok(())
    }

    #[inline(never)]
    fn settle_borrow(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        token_index: usize,
        quantity: u64
    ) -> MangoResult<()> {
        const NUM_FIXED: usize = 4;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            mango_group_acc,
            margin_account_acc,
            owner_acc,
            clock_acc,
        ] = accounts;

        let mut mango_group = MangoGroup::load_mut_checked(mango_group_acc, program_id)?;
        let mut margin_account = MarginAccount::load_mut_checked(
            program_id, margin_account_acc, mango_group_acc.key
        )?;
        let clock = Clock::from_account_info(clock_acc)?;
        mango_group.update_indexes(&clock)?;
        prog_assert!(owner_acc.is_signer)?;
        prog_assert_eq!(&margin_account.owner, owner_acc.key)?;

        settle_borrow_unchecked(&mut mango_group, &mut margin_account, token_index, quantity)?;
        Ok(())
    }

    #[inline(never)]
    fn liquidate(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        deposit_quantities: [u64; NUM_TOKENS]
    ) -> MangoResult<()> {
        const NUM_FIXED: usize = 5;
        let accounts = array_ref![accounts, 0, NUM_FIXED + 2 * NUM_MARKETS + 2 * NUM_TOKENS];
        let (
            fixed_accs,
            open_orders_accs,
            oracle_accs,
            vault_accs,
            liqor_token_account_accs,
        ) = array_refs![accounts, NUM_FIXED, NUM_MARKETS, NUM_MARKETS, NUM_TOKENS, NUM_TOKENS];

        let [
            mango_group_acc,
            liqor_acc,
            liqee_margin_account_acc,
            token_prog_acc,
            clock_acc
        ] = fixed_accs;

        // margin ratio = equity / val(borrowed)
        // equity = val(positions) - val(borrowed) + val(collateral)
        prog_assert!(liqor_acc.is_signer)?;
        let mut mango_group = MangoGroup::load_mut_checked(
            mango_group_acc, program_id
        )?;
        let mut liqee_margin_account = MarginAccount::load_mut_checked(
            program_id, liqee_margin_account_acc, mango_group_acc.key
        )?;
        let clock = Clock::from_account_info(clock_acc)?;
        mango_group.update_indexes(&clock)?;

        for i in 0..NUM_MARKETS {
            prog_assert_eq!(open_orders_accs[i].key, &liqee_margin_account.open_orders[i])?;
            check_open_orders(&open_orders_accs[i], &mango_group.signer_key)?;
        }

        let prices = get_prices(&mango_group, oracle_accs)?;
        let coll_ratio = liqee_margin_account.get_collateral_ratio(
            &mango_group, &prices, open_orders_accs)?;

        // No liquidations if account above maint collateral ratio
        prog_assert!(coll_ratio < mango_group.maint_coll_ratio)?;

        // Settle borrows to see if it gets us above maint
        for i in 0..NUM_TOKENS {
            let native_borrow = liqee_margin_account.get_native_borrow(&mango_group.indexes[i], i);
            settle_borrow_unchecked(&mut mango_group, &mut liqee_margin_account, i, native_borrow)?;
        }
        let coll_ratio = liqee_margin_account.get_collateral_ratio(
            &mango_group, &prices, open_orders_accs)?;
        if coll_ratio >= mango_group.maint_coll_ratio {  // if account not liquidatable after settle borrow, then return
            return Ok(())
        }

        // TODO liquidator may forcefully SettleFunds and SettleBorrow on account with less than maint

        if coll_ratio < U64F64::from_num(1) {
            let liabs = liqee_margin_account.get_total_liabs(&mango_group)?;
            let liabs_val = liqee_margin_account.get_liabs_val(&mango_group, &prices)?;
            let assets_val = liqee_margin_account.get_assets_val(&mango_group, &prices, open_orders_accs)?;

            // reduction_val = amount of quote currency value to reduce liabilities by to get coll_ratio = 1.01
            let reduction_val = liabs_val
                .checked_sub(assets_val / U64F64::from_num(1.01)).unwrap();

            for i in 0..NUM_TOKENS {
                let proportion = U64F64::from_num(liabs[i])
                    .checked_div(liabs_val).unwrap();

                let token_reduce = proportion.checked_mul(reduction_val).unwrap();
                socialize_loss(&mut mango_group, &mut liqee_margin_account, i, token_reduce)?;
                // TODO this will reduce deposits of liqee as well which could put actual value below; way to fix is to SettleBorrow first
                // TODO Can socialize loss cause more liquidations? Perhaps other accounts then go below threshold
                // TODO what happens if unable to socialize loss? If not enough deposits in currency
            }
        }

        // Pull deposits from liqor's token wallets
        prog_assert_eq!(token_prog_acc.key, &spl_token::id())?;
        for i in 0..NUM_TOKENS {
            let quantity = deposit_quantities[i];
            if quantity == 0 {
                continue;
            }

            let vault_acc: &AccountInfo = &vault_accs[i];
            prog_assert_eq!(&mango_group.vaults[i], vault_acc.key)?;
            let token_account_acc: &AccountInfo = &liqor_token_account_accs[i];
            let deposit_instruction = spl_token::instruction::transfer(
                &spl_token::id(),
                token_account_acc.key,
                vault_acc.key,
                &liqor_acc.key, &[], quantity
            )?;
            let deposit_accs = [
                token_account_acc.clone(),
                vault_acc.clone(),
                liqor_acc.clone(),
                token_prog_acc.clone()
            ];

            solana_program::program::invoke_signed(&deposit_instruction, &deposit_accs, &[])?;
            let deposit: U64F64 = U64F64::from_num(quantity) / mango_group.indexes[i].deposit;
            checked_add_deposit(&mut mango_group, &mut liqee_margin_account, i, deposit)?;
        }

        // Check to make sure liqor's deposits brought account above init_coll_ratio
        let coll_ratio = liqee_margin_account.get_collateral_ratio(&mango_group, &prices, open_orders_accs)?;
        prog_assert!(coll_ratio >= mango_group.init_coll_ratio)?;

        // If all deposits are good, transfer ownership of margin account to liqor
        liqee_margin_account.owner = *liqor_acc.key;

        Ok(())
    }

    #[inline(never)]
    fn deposit_srm(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        quantity: u64
    ) -> MangoResult<()> {

        const NUM_FIXED: usize = 8;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            mango_group_acc,
            mango_srm_account_acc,
            owner_acc,
            srm_account_acc,
            vault_acc,
            token_prog_acc,
            clock_acc,
            rent_acc,
        ] = accounts;
        // prog_assert!(owner_acc.is_signer)?; // anyone can deposit, not just owner

        let mut mango_group = MangoGroup::load_mut_checked(mango_group_acc, program_id)?;

        // if MangoSrmAccount is empty, initialize it
        prog_assert_eq!(mango_srm_account_acc.data_len(), size_of::<MangoSrmAccount>())?;
        let mut mango_srm_account = MangoSrmAccount::load_mut(mango_srm_account_acc)?;
        prog_assert_eq!(mango_srm_account_acc.owner, program_id)?;

        if mango_srm_account.account_flags == 0 {
            let rent = Rent::from_account_info(rent_acc)?;
            prog_assert!(rent.is_exempt(mango_srm_account_acc.lamports(), size_of::<MangoSrmAccount>()))?;

            mango_srm_account.account_flags = (AccountFlag::Initialized | AccountFlag::MangoSrmAccount).bits();
            mango_srm_account.mango_group = *mango_group_acc.key;
            prog_assert!(owner_acc.is_signer)?;  // this is not necessary but whatever
            mango_srm_account.owner = *owner_acc.key;
        } else {
            prog_assert_eq!(mango_srm_account.account_flags, (AccountFlag::Initialized | AccountFlag::MangoSrmAccount).bits())?;
            prog_assert_eq!(&mango_srm_account.mango_group, mango_group_acc.key)?;
        }

        let clock = Clock::from_account_info(clock_acc)?;
        mango_group.update_indexes(&clock)?;

        prog_assert_eq!(vault_acc.key, &mango_group.srm_vault)?;
        prog_assert_eq!(token_prog_acc.key, &spl_token::id())?;
        let deposit_instruction = spl_token::instruction::transfer(
            &spl_token::id(),
            srm_account_acc.key,
            vault_acc.key,
            &owner_acc.key, &[], quantity
        )?;
        let deposit_accs = [
            srm_account_acc.clone(),
            vault_acc.clone(),
            owner_acc.clone(),
            token_prog_acc.clone()
        ];

        solana_program::program::invoke_signed(&deposit_instruction, &deposit_accs, &[])?;
        mango_srm_account.amount = mango_srm_account.amount.checked_add(quantity).unwrap();
        Ok(())
    }

    #[inline(never)]
    fn withdraw_srm(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        quantity: u64
    ) -> MangoResult<()> {
        const NUM_FIXED: usize = 8;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            mango_group_acc,
            mango_srm_account_acc,
            owner_acc,
            srm_account_acc,
            vault_acc,
            signer_acc,
            token_prog_acc,
            clock_acc,
        ] = accounts;

        let mut mango_group = MangoGroup::load_mut_checked(mango_group_acc, program_id)?;
        let mut mango_srm_account = MangoSrmAccount::load_mut_checked(
            program_id, mango_srm_account_acc, mango_group_acc.key)?;

        let clock = Clock::from_account_info(clock_acc)?;
        mango_group.update_indexes(&clock)?;
        prog_assert!(owner_acc.is_signer)?;
        prog_assert_eq!(&mango_srm_account.owner, owner_acc.key)?;
        prog_assert_eq!(vault_acc.key, &mango_group.srm_vault)?;
        prog_assert!(mango_srm_account.amount >= quantity)?;
        prog_assert_eq!(token_prog_acc.key, &spl_token::id())?;

        // Send out withdraw instruction to SPL token program
        let withdraw_instruction = spl_token::instruction::transfer(
            &spl_token::id(),
            vault_acc.key,
            srm_account_acc.key,
            signer_acc.key,
            &[],
            quantity
        )?;
        let withdraw_accs = [
            vault_acc.clone(),
            srm_account_acc.clone(),
            signer_acc.clone(),
            token_prog_acc.clone()
        ];
        let signer_seeds = gen_signer_seeds(&mango_group.signer_nonce, mango_group_acc.key);
        solana_program::program::invoke_signed(&withdraw_instruction, &withdraw_accs, &[&signer_seeds])?;
        mango_srm_account.amount = mango_srm_account.amount.checked_sub(quantity).unwrap();

        Ok(())
    }

    #[inline(never)]
    fn change_borrow_limit(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        token_index: usize,
        borrow_limit: u64
    ) -> MangoResult<()> {
        const NUM_FIXED: usize = 2;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            mango_group_acc,
            admin_acc,
        ] = accounts;

        let mut mango_group = MangoGroup::load_mut_checked(
            mango_group_acc,
            program_id
        )?;

        prog_assert_eq!(admin_acc.key, &mango_group.admin)?;
        prog_assert!(admin_acc.is_signer)?;

        mango_group.borrow_limits[token_index] = borrow_limit;
        Ok(())
    }

    #[inline(never)]
    fn place_order(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        order: serum_dex::instruction::NewOrderInstructionV3
    ) -> MangoResult<()> {
        // TODO disallow limit prices that would put account below initCollRatio

        const NUM_FIXED: usize = 17;
        let accounts = array_ref![accounts, 0, NUM_FIXED + 2 * NUM_MARKETS];
        let (
            fixed_accs,
            open_orders_accs,
            oracle_accs,
        ) = array_refs![accounts, NUM_FIXED, NUM_MARKETS, NUM_MARKETS];

        let [
            mango_group_acc,
            owner_acc,
            margin_account_acc,
            clock_acc,
            dex_prog_acc,
            spot_market_acc,
            dex_request_queue_acc,
            dex_event_queue_acc,
            bids_acc,
            asks_acc,
            vault_acc,
            signer_acc,
            dex_base_acc,
            dex_quote_acc,
            token_prog_acc,
            rent_acc,
            srm_vault_acc,
        ] = fixed_accs;

        let mut mango_group = MangoGroup::load_mut_checked(mango_group_acc, program_id)?;
        let mut margin_account = MarginAccount::load_mut_checked(
            program_id, margin_account_acc, mango_group_acc.key
        )?;

        let clock = Clock::from_account_info(clock_acc)?;
        mango_group.update_indexes(&clock)?;

        let prices = get_prices(&mango_group, oracle_accs)?;
        let coll_ratio = margin_account.get_collateral_ratio(&mango_group, &prices, open_orders_accs)?;
        let reduce_only = coll_ratio < mango_group.init_coll_ratio;

        prog_assert!(owner_acc.is_signer)?;
        prog_assert_eq!(&margin_account.owner, owner_acc.key)?;

        let market_i = mango_group.get_market_index(spot_market_acc.key).unwrap();
        let token_i = match order.side {
            Side::Bid => NUM_MARKETS,
            Side::Ask => market_i
        };
        prog_assert_eq!(&mango_group.vaults[token_i], vault_acc.key)?;

        let pre_amount = {  // this is to keep track of how much funds were transferred out
            let vault = Account::unpack(&vault_acc.try_borrow_data()?)?;
            vault.amount
        };

        for i in 0..NUM_MARKETS {
            let open_orders_acc = &open_orders_accs[i];
            if i == market_i {  // this one must not be default pubkey
                prog_assert!(*open_orders_acc.key != Pubkey::default())?;
                if margin_account.open_orders[i] == Pubkey::default() {
                    let open_orders = load_open_orders(open_orders_acc)?;
                    prog_assert_eq!(open_orders.account_flags, 0)?;
                    margin_account.open_orders[i] = *open_orders_acc.key;
                }
            } else {
                prog_assert_eq!(open_orders_accs[i].key, &margin_account.open_orders[i])?;
                check_open_orders(&open_orders_accs[i], &mango_group.signer_key)?;
            }
        }

        prog_assert_eq!(token_prog_acc.key, &spl_token::id())?;
        prog_assert_eq!(dex_prog_acc.key, &mango_group.dex_program_id)?;
        let data = serum_dex::instruction::MarketInstruction::NewOrderV3(order).pack();
        let instruction = Instruction {
            program_id: *dex_prog_acc.key,
            data,
            accounts: vec![
                AccountMeta::new(*spot_market_acc.key, false),
                AccountMeta::new(*open_orders_accs[market_i].key, false),
                AccountMeta::new(*dex_request_queue_acc.key, false),
                AccountMeta::new(*dex_event_queue_acc.key, false),
                AccountMeta::new(*bids_acc.key, false),
                AccountMeta::new(*asks_acc.key, false),
                AccountMeta::new(*vault_acc.key, false),
                AccountMeta::new_readonly(*signer_acc.key, true),
                AccountMeta::new(*dex_base_acc.key, false),
                AccountMeta::new(*dex_quote_acc.key, false),
                AccountMeta::new_readonly(*token_prog_acc.key, false),
                AccountMeta::new_readonly(*rent_acc.key, false),
                AccountMeta::new(*srm_vault_acc.key, false),
            ],
        };
        let account_infos = [
            dex_prog_acc.clone(),  // Have to add account of the program id
            spot_market_acc.clone(),
            open_orders_accs[market_i].clone(),
            dex_request_queue_acc.clone(),
            dex_event_queue_acc.clone(),
            bids_acc.clone(),
            asks_acc.clone(),
            vault_acc.clone(),
            signer_acc.clone(),
            dex_base_acc.clone(),
            dex_quote_acc.clone(),
            token_prog_acc.clone(),
            rent_acc.clone(),
            srm_vault_acc.clone(),
        ];

        let signer_seeds = gen_signer_seeds(&mango_group.signer_nonce, mango_group_acc.key);
        solana_program::program::invoke_signed(&instruction, &account_infos, &[&signer_seeds])?;

        let post_amount = {
            let vault = Account::unpack(&vault_acc.try_borrow_data()?)?;
            vault.amount
        };

        let spent = pre_amount.checked_sub(post_amount).unwrap();
        let index: MangoIndex = mango_group.indexes[token_i];
        let native_deposit = margin_account.get_native_deposit(&index, token_i);

        // user deposits will be used first.
        // If user does not want that to happen, they must first issue a borrow command
        if native_deposit >= spent {
            let spent_deposit = U64F64::from_num(spent) / index.deposit;
            checked_sub_deposit(&mut mango_group, &mut margin_account, token_i, spent_deposit)?;
        } else {

            let avail_deposit = margin_account.deposits[token_i];
            checked_sub_deposit(&mut mango_group, &mut margin_account, token_i, avail_deposit)?;
            let rem_spend = U64F64::from_num(spent - native_deposit);

            prog_assert!(!reduce_only)?;  // Cannot borrow more in reduce only mode
            checked_add_borrow(&mut mango_group, &mut margin_account, token_i , rem_spend / index.borrow)?;
            prog_assert!(margin_account.get_native_borrow(&index, token_i) <= mango_group.borrow_limits[token_i])?;
        }

        let coll_ratio = margin_account.get_collateral_ratio(&mango_group, &prices, open_orders_accs)?;
        prog_assert!(reduce_only || coll_ratio >= mango_group.init_coll_ratio)?;

        prog_assert!(mango_group.has_valid_deposits_borrows(token_i))?;
        Ok(())
    }

    #[inline(never)]
    fn settle_funds(
        program_id: &Pubkey,
        accounts: &[AccountInfo]
    ) -> MangoResult<()> {
        const NUM_FIXED: usize = 14;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            mango_group_acc,
            owner_acc,  // signer
            margin_account_acc,
            clock_acc,

            dex_prog_acc,
            spot_market_acc,
            open_orders_acc,
            signer_acc,
            dex_base_acc,
            dex_quote_acc,
            base_vault_acc,
            quote_vault_acc,
            dex_signer_acc,
            token_prog_acc,
        ] = accounts;

        let mut mango_group = MangoGroup::load_mut_checked(mango_group_acc, program_id)?;
        let mut margin_account = MarginAccount::load_mut_checked(
            program_id,
            margin_account_acc,
            mango_group_acc.key
        )?;
        let clock = Clock::from_account_info(clock_acc)?;
        mango_group.update_indexes(&clock)?;

        let market_i = mango_group.get_market_index(spot_market_acc.key).unwrap();

        prog_assert!(owner_acc.is_signer)?;
        prog_assert_eq!(owner_acc.key, &margin_account.owner)?;
        prog_assert_eq!(&margin_account.open_orders[market_i], open_orders_acc.key)?;
        prog_assert_eq!(base_vault_acc.key, &mango_group.vaults[market_i])?;
        prog_assert_eq!(quote_vault_acc.key, &mango_group.vaults[NUM_MARKETS])?;
        prog_assert_eq!(token_prog_acc.key, &spl_token::id())?;
        prog_assert_eq!(dex_prog_acc.key, &mango_group.dex_program_id)?;

        if *open_orders_acc.key == Pubkey::default() {
            return Ok(());
        }

        let (pre_base, pre_quote) = {
            let open_orders = load_open_orders(open_orders_acc)?;
            (open_orders.native_coin_free, open_orders.native_pc_free)
        };

        if pre_base == 0 && pre_quote == 0 {
            return Ok(());
        }

        let signer_seeds = gen_signer_seeds(&mango_group.signer_nonce, mango_group_acc.key);
        invoke_settle_funds(
            dex_prog_acc,
            spot_market_acc,
            open_orders_acc,
            signer_acc,
            dex_base_acc,
            dex_quote_acc,
            base_vault_acc,
            quote_vault_acc,
            dex_signer_acc,
            token_prog_acc,
            &[&signer_seeds]
        )?;

        let (post_base, post_quote) = {
            let open_orders = load_open_orders(open_orders_acc)?;
            (open_orders.native_coin_free, open_orders.native_pc_free)
        };

        prog_assert!(post_base <= pre_base)?;
        prog_assert!(post_quote <= pre_quote)?;

        let base_change = U64F64::from_num(pre_base - post_base) / mango_group.indexes[market_i].deposit;
        let quote_change = U64F64::from_num(pre_quote - post_quote) / mango_group.indexes[NUM_MARKETS].deposit;

        checked_add_deposit(&mut mango_group, &mut margin_account, market_i, base_change)?;
        checked_add_deposit(&mut mango_group, &mut margin_account, NUM_MARKETS, quote_change)?;
        Ok(())
    }

    #[inline(never)]
    fn cancel_order(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        data: Vec<u8>
    ) -> MangoResult<()> {
        const NUM_FIXED: usize = 11;
        let accounts = array_ref![accounts, 0, NUM_FIXED];

        let [
            mango_group_acc,
            owner_acc,  // signer
            margin_account_acc,
            clock_acc,
            dex_prog_acc,
            spot_market_acc,
            bids_acc,
            asks_acc,
            open_orders_acc,
            signer_acc,
            dex_event_queue_acc,
        ] = accounts;

        let mut mango_group = MangoGroup::load_mut_checked(mango_group_acc, program_id)?;
        let margin_account = MarginAccount::load_checked(
            program_id,
            margin_account_acc,
            mango_group_acc.key
        )?;
        let clock = Clock::from_account_info(clock_acc)?;
        mango_group.update_indexes(&clock)?;
        prog_assert_eq!(dex_prog_acc.key, &mango_group.dex_program_id)?;

        prog_assert!(owner_acc.is_signer)?;
        prog_assert_eq!(&margin_account.owner, owner_acc.key)?;
        let market_i = mango_group.get_market_index(spot_market_acc.key).unwrap();
        prog_assert_eq!(&margin_account.open_orders[market_i], open_orders_acc.key)?;

        let signer_seeds = gen_signer_seeds(&mango_group.signer_nonce, mango_group_acc.key);
        invoke_cancel_order(
            dex_prog_acc,
            spot_market_acc,
            bids_acc,
            asks_acc,
            open_orders_acc,
            signer_acc,
            dex_event_queue_acc,
            data,
            &[&signer_seeds]
        )?;
        Ok(())
    }

    #[inline(never)]
    fn place_and_settle(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        order: serum_dex::instruction::NewOrderInstructionV3
    ) -> MangoResult<()> {
        const NUM_FIXED: usize = 19;  // *** Changed
        let accounts = array_ref![accounts, 0, NUM_FIXED + 2 * NUM_MARKETS];
        let (
            fixed_accs,
            open_orders_accs,
            oracle_accs,
        ) = array_refs![accounts, NUM_FIXED, NUM_MARKETS, NUM_MARKETS];

        let [
            mango_group_acc,
            owner_acc,
            margin_account_acc,
            clock_acc,
            dex_prog_acc,
            spot_market_acc,
            dex_request_queue_acc,
            dex_event_queue_acc,
            bids_acc,
            asks_acc,
            base_vault_acc,
            quote_vault_acc,
            signer_acc,
            dex_base_acc,
            dex_quote_acc,
            token_prog_acc,
            rent_acc,
            srm_vault_acc,
            dex_signer_acc
        ] = fixed_accs;

        let mut mango_group = MangoGroup::load_mut_checked(mango_group_acc, program_id)?;
        let mut margin_account = MarginAccount::load_mut_checked(
            program_id, margin_account_acc, mango_group_acc.key
        )?;

        let clock = Clock::from_account_info(clock_acc)?;
        mango_group.update_indexes(&clock)?;

        let prices = get_prices(&mango_group, oracle_accs)?;
        let coll_ratio = margin_account.get_collateral_ratio(&mango_group, &prices, open_orders_accs)?;
        let reduce_only = coll_ratio < mango_group.init_coll_ratio;

        prog_assert!(owner_acc.is_signer)?;
        prog_assert_eq!(&margin_account.owner, owner_acc.key)?;

        let market_i = mango_group.get_market_index(spot_market_acc.key).unwrap();
        let side = order.side;
        let (in_token_i, out_token_i, vault_acc) = match side {
            Side::Bid => (market_i, NUM_MARKETS, quote_vault_acc),
            Side::Ask => (NUM_MARKETS, market_i, base_vault_acc)
        };
        prog_assert_eq!(&mango_group.vaults[market_i], base_vault_acc.key)?;
        prog_assert_eq!(&mango_group.vaults[NUM_MARKETS], quote_vault_acc.key)?;

        let (pre_base, pre_quote) = {
            (Account::unpack(&base_vault_acc.try_borrow_data()?)?.amount,
             Account::unpack(&quote_vault_acc.try_borrow_data()?)?.amount)
        };

        for i in 0..NUM_MARKETS {
            let open_orders_acc = &open_orders_accs[i];
            if i == market_i {  // this one must not be default pubkey
                prog_assert!(*open_orders_acc.key != Pubkey::default())?;

                // if this is first time using this open_orders_acc, check and save it
                if margin_account.open_orders[i] == Pubkey::default() {
                    let open_orders = load_open_orders(open_orders_acc)?;
                    prog_assert_eq!(open_orders.account_flags, 0)?;
                    margin_account.open_orders[i] = *open_orders_acc.key;
                } else {
                    prog_assert_eq!(open_orders_accs[i].key, &margin_account.open_orders[i])?;
                    check_open_orders(&open_orders_accs[i], &mango_group.signer_key)?;
                }
            } else {
                prog_assert_eq!(open_orders_accs[i].key, &margin_account.open_orders[i])?;
                check_open_orders(&open_orders_accs[i], &mango_group.signer_key)?;
            }
        }

        prog_assert_eq!(token_prog_acc.key, &spl_token::id())?;
        prog_assert_eq!(dex_prog_acc.key, &mango_group.dex_program_id)?;
        let data = serum_dex::instruction::MarketInstruction::NewOrderV3(order).pack();
        let instruction = Instruction {
            program_id: *dex_prog_acc.key,
            data,
            accounts: vec![
                AccountMeta::new(*spot_market_acc.key, false),
                AccountMeta::new(*open_orders_accs[market_i].key, false),
                AccountMeta::new(*dex_request_queue_acc.key, false),
                AccountMeta::new(*dex_event_queue_acc.key, false),
                AccountMeta::new(*bids_acc.key, false),
                AccountMeta::new(*asks_acc.key, false),
                AccountMeta::new(*vault_acc.key, false),
                AccountMeta::new_readonly(*signer_acc.key, true),
                AccountMeta::new(*dex_base_acc.key, false),
                AccountMeta::new(*dex_quote_acc.key, false),
                AccountMeta::new_readonly(*token_prog_acc.key, false),
                AccountMeta::new_readonly(*rent_acc.key, false),
                AccountMeta::new(*srm_vault_acc.key, false),
            ],
        };
        let account_infos = [
            dex_prog_acc.clone(),  // Have to add account of the program id
            spot_market_acc.clone(),
            open_orders_accs[market_i].clone(),
            dex_request_queue_acc.clone(),
            dex_event_queue_acc.clone(),
            bids_acc.clone(),
            asks_acc.clone(),
            vault_acc.clone(),
            signer_acc.clone(),
            dex_base_acc.clone(),
            dex_quote_acc.clone(),
            token_prog_acc.clone(),
            rent_acc.clone(),
            srm_vault_acc.clone(),
        ];

        let signer_seeds = gen_signer_seeds(&mango_group.signer_nonce, mango_group_acc.key);
        solana_program::program::invoke_signed(&instruction, &account_infos, &[&signer_seeds])?;

        // Settle funds for this market
        invoke_settle_funds(
            dex_prog_acc,
            spot_market_acc,
            &open_orders_accs[market_i],
            signer_acc,
            dex_base_acc,
            dex_quote_acc,
            base_vault_acc,
            quote_vault_acc,
            dex_signer_acc,
            token_prog_acc,
            &[&signer_seeds]
        )?;

        let (post_base, post_quote) = {
            (Account::unpack(&base_vault_acc.try_borrow_data()?)?.amount,
             Account::unpack(&quote_vault_acc.try_borrow_data()?)?.amount)
        };

        let (pre_in, pre_out, post_in, post_out) = match side {
            Side::Bid => (pre_base, pre_quote, post_base, post_quote),
            Side::Ask => (pre_quote, pre_base, post_quote, post_base)
        };

        // It's possible the net change was positive for both tokens
        // It's not possible for in_token to be negative
        let out_index: MangoIndex = mango_group.indexes[out_token_i];
        let in_index: MangoIndex = mango_group.indexes[in_token_i];

        // if out token was net negative, then you may need to borrow more
        if post_out < pre_out {
            let total_out = pre_out.checked_sub(post_out).unwrap();
            let native_deposit = margin_account.get_native_deposit(&out_index, out_token_i);
            if native_deposit < total_out {  // need to borrow
                let avail_deposit = margin_account.deposits[out_token_i];
                checked_sub_deposit(&mut mango_group, &mut margin_account, out_token_i, avail_deposit)?;
                let rem_spend = U64F64::from_num(total_out - native_deposit);

                prog_assert!(!reduce_only)?;  // Cannot borrow more in reduce only mode
                checked_add_borrow(&mut mango_group, &mut margin_account, out_token_i, rem_spend / out_index.borrow)?;
                prog_assert!(margin_account.get_native_borrow(&out_index, out_token_i) <= mango_group.borrow_limits[out_token_i])?;
            } else {  // just spend user deposits
                let mango_spent = U64F64::from_num(total_out) / out_index.deposit;
                checked_sub_deposit(&mut mango_group, &mut margin_account, out_token_i, mango_spent)?;
            }
        } else {  // Add out token deposit
            let deposit = U64F64::from_num(post_out.checked_sub(pre_out).unwrap()) / out_index.deposit;
            checked_add_deposit(&mut mango_group, &mut margin_account, out_token_i, deposit)?;
        }

        let total_in = U64F64::from_num(post_in.checked_sub(pre_in).unwrap()) / in_index.deposit;
        checked_add_deposit(&mut mango_group, &mut margin_account, in_token_i, total_in)?;

        // Settle borrow
        // TODO only do ops on tokens that have borrows and deposits
        settle_borrow_full_unchecked(&mut mango_group, &mut margin_account, out_token_i)?;
        settle_borrow_full_unchecked(&mut mango_group, &mut margin_account, in_token_i)?;

        let coll_ratio = margin_account.get_collateral_ratio(&mango_group, &prices, open_orders_accs)?;
        prog_assert!(reduce_only || coll_ratio >= mango_group.init_coll_ratio)?;
        prog_assert!(mango_group.has_valid_deposits_borrows(out_token_i))?;

        Ok(())
    }


    #[inline(never)]
    fn partial_liquidate(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        deposit_quantities: [u64; NUM_TOKENS]
    ) -> MangoResult<()> {

        const NUM_FIXED: usize = 6;
        let accounts = array_ref![accounts, 0, NUM_FIXED + 6 * NUM_MARKETS + 2 * NUM_TOKENS];
        let (
            fixed_accs,
            open_orders_accs,
            oracle_accs,
            spot_market_accs,
            bids_accs,
            asks_accs,
            dex_event_queues,
            vault_accs,
            liqor_token_account_accs,
        ) = array_refs![accounts, NUM_FIXED, NUM_MARKETS, NUM_MARKETS, NUM_MARKETS, NUM_MARKETS,
                        NUM_MARKETS, NUM_MARKETS, NUM_TOKENS, NUM_TOKENS];

        let [
            mango_group_acc,
            liqor_acc,
            liqee_margin_account_acc,
            token_prog_acc,
            clock_acc,
            dex_prog_acc,

        ] = fixed_accs;

        // margin ratio = equity / val(borrowed)
        // equity = val(positions) - val(borrowed) + val(collateral)
        prog_assert!(liqor_acc.is_signer)?;
        let mut mango_group = MangoGroup::load_mut_checked(
            mango_group_acc, program_id
        )?;
        let mut liqee_margin_account = MarginAccount::load_mut_checked(
            program_id, liqee_margin_account_acc, mango_group_acc.key
        )?;
        let clock = Clock::from_account_info(clock_acc)?;
        mango_group.update_indexes(&clock)?;

        for i in 0..NUM_MARKETS {
            prog_assert_eq!(open_orders_accs[i].key, &liqee_margin_account.open_orders[i])?;
            check_open_orders(&open_orders_accs[i], &mango_group.signer_key)?;
        }

        let prices = get_prices(&mango_group, oracle_accs)?;
        let coll_ratio = liqee_margin_account.get_collateral_ratio(
            &mango_group, &prices, open_orders_accs
        )?;

        for i in 0..NUM_MARKETS {
            let spot_market_acc: &AccountInfo = &spot_market_accs[i];
            let open_orders_acc: &AccountInfo = &open_orders_accs[i];
            let bids_acc: &AccountInfo = &bids_accs[i];
            let asks_acc: &AccountInfo = &asks_accs[i];

            let open_orders = load_open_orders(open_orders_acc)?;

            let free_slot_bits = open_orders.free_slot_bits;
            let mut num_orders = free_slot_bits.count_zeros();
            let mut j: usize = 0;

            for j in 0..128 {
                if num_orders == 0 {
                    break;
                }

                let slot_is_free = (free_slot_bits & 1u128) != 0;
                let free_slot_bits = free_slot_bits >> 1;
                if slot_is_free {
                    continue
                }

                let oid = open_orders.orders[j];
                num_orders -= 1;

                invoke_cancel_order(
                    dex_prog_acc,
                    spot_market_acc,
                    bids_acc,
                    asks_acc,
                    open_orders_acc,

                )

            }

            while num_orders > 0 {
                let slot_is_free = (free_slot_bits & 1u128) != 0;
                let free_slot_bits = free_slot_bits >> 1;
                let oid = open_orders.orders[j];
                j += 1;
                if slot_is_free {
                    continue;
                }
                num_orders -= 1;
            }


        }

        // cancel orders,
        // settle funds
        // settle borrows
        // allow liquidations up until the account gets above init collateral ratio
        // if account hits 0 deposits, socialize losses

        // offset borrows
        Ok(())
    }

    pub fn process(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        data: &[u8]
    ) -> MangoResult<()> {
        let instruction = MangoInstruction::unpack(data).ok_or(ProgramError::InvalidInstructionData)?;
        match instruction {
            MangoInstruction::InitMangoGroup {
                signer_nonce, maint_coll_ratio, init_coll_ratio, borrow_limits
            } => {
                msg!("Mango: InitMangoGroup");
                Self::init_mango_group(program_id, accounts, signer_nonce, maint_coll_ratio, init_coll_ratio, borrow_limits)?;
            }
            MangoInstruction::InitMarginAccount => {
                msg!("Mango: InitMarginAccount");
                Self::init_margin_account(program_id, accounts)?;
            }
            MangoInstruction::Deposit {
                quantity
            } => {
                msg!("Mango: Deposit");
                Self::deposit(program_id, accounts, quantity)?;
            }
            MangoInstruction::Withdraw {
                quantity
            } => {
                msg!("Mango: Withdraw");
                Self::withdraw(program_id, accounts, quantity)?;
            }
            MangoInstruction::Borrow {
                token_index,
                quantity
            } => {
                msg!("Mango: Borrow");
                Self::borrow(program_id, accounts, token_index, quantity)?;
            }
            MangoInstruction::SettleBorrow {
                token_index,
                quantity
            } => {
                msg!("Mango: SettleBorrow");
                Self::settle_borrow(program_id, accounts, token_index, quantity)?;
            }
            MangoInstruction::Liquidate {
                deposit_quantities
            } => {
                // Either user takes the position
                // Or the program can liquidate on the serum dex (in case no liquidator wants to take pos)
                msg!("Mango: Liquidate");
                Self::liquidate(program_id, accounts, deposit_quantities)?;
            }
            MangoInstruction::DepositSrm {
                quantity
            } => {
                msg!("Mango: DepositSrm");
                Self::deposit_srm(program_id, accounts, quantity)?;
            }
            MangoInstruction::WithdrawSrm {
                quantity
            } => {
                msg!("Mango: WithdrawSrm");
                Self::withdraw_srm(program_id, accounts, quantity)?;
            }
            MangoInstruction::PlaceOrder {
                order
            } => {
                msg!("Mango: PlaceOrder");
                Self::place_order(program_id, accounts, order)?;
            }
            MangoInstruction::SettleFunds => {
                msg!("Mango: SettleFunds");
                Self::settle_funds(program_id, accounts)?;
            }
            MangoInstruction::CancelOrder {
                order
            } => {
                msg!("Mango: CancelOrder");
                let data =  serum_dex::instruction::MarketInstruction::CancelOrderV2(order).pack();
                Self::cancel_order(program_id, accounts, data)?;
            }
            MangoInstruction::CancelOrderByClientId {
                client_id
            } => {
                msg!("Mango: CancelOrderByClientId");
                Self::cancel_order(program_id, accounts, client_id.to_le_bytes().to_vec())?;
            }

            MangoInstruction::ChangeBorrowLimit {
                token_index, borrow_limit
            } => {
                msg!("Mango: ChangeBorrowLimit");
                Self::change_borrow_limit(program_id, accounts, token_index, borrow_limit)?;
            }
            MangoInstruction::PlaceAndSettle {
                order
            } => {
                msg!("Mango: PlaceAndSettle");
                Self::place_and_settle(program_id, accounts, order)?;
            }
        }
        Ok(())
    }
}


fn settle_borrow_unchecked(
    mango_group: &mut MangoGroup,
    margin_account: &mut MarginAccount,
    token_index: usize,
    quantity: u64
) -> MangoResult<()> {
    let index: &MangoIndex = &mango_group.indexes[token_index];

    let native_borrow = margin_account.get_native_borrow(index, token_index);
    let native_deposit = margin_account.get_native_deposit(index, token_index);

    let quantity = cmp::min(cmp::min(quantity, native_borrow), native_deposit);

    let borr_settle = U64F64::from_num(quantity) / index.borrow;
    let dep_settle = U64F64::from_num(quantity) / index.deposit;

    checked_sub_deposit(mango_group, margin_account, token_index, dep_settle)?;
    checked_sub_borrow(mango_group, margin_account, token_index, borr_settle)?;

    // No need to check collateralization ratio or deposits/borrows validity

    Ok(())

}

fn settle_borrow_full_unchecked(
    mango_group: &mut MangoGroup,
    margin_account: &mut MarginAccount,
    token_index: usize,
) -> MangoResult<()> {
    let index: &MangoIndex = &mango_group.indexes[token_index];

    let native_borrow = margin_account.get_native_borrow(index, token_index);
    let native_deposit = margin_account.get_native_deposit(index, token_index);

    let quantity = cmp::min(native_borrow, native_deposit);

    let borr_settle = U64F64::from_num(quantity) / index.borrow;
    let dep_settle = U64F64::from_num(quantity) / index.deposit;

    checked_sub_deposit(mango_group, margin_account, token_index, dep_settle)?;
    checked_sub_borrow(mango_group, margin_account, token_index, borr_settle)?;

    // No need to check collateralization ratio or deposits/borrows validity

    Ok(())

}

fn socialize_loss(
    mango_group: &mut MangoGroup,
    margin_account: &mut MarginAccount,
    token_index: usize,
    reduce_quantity_native: U64F64
) -> MangoResult<()> {

    // reduce borrow for this margin_account by appropriate amount
    // decrease MangoIndex.deposit by appropriate amount

    // TODO make sure there is enough funds to socialize losses
    let quantity: U64F64 = reduce_quantity_native / mango_group.indexes[token_index].borrow;
    checked_sub_borrow(mango_group, margin_account, token_index, quantity)?;

    let total_deposits = U64F64::from_num(mango_group.get_total_native_deposit(token_index));
    let percentage_loss = reduce_quantity_native.checked_div(total_deposits).unwrap();
    let index: &mut MangoIndex = &mut mango_group.indexes[token_index];
    index.deposit = index.deposit
        .checked_sub(percentage_loss.checked_mul(index.deposit).unwrap()).unwrap();

    Ok(())
}

fn checked_sub_deposit(
    mango_group: &mut MangoGroup,
    margin_account: &mut MarginAccount,
    token_index: usize,
    quantity: U64F64
) -> MangoResult<()> {
    margin_account.checked_sub_deposit(token_index, quantity)?;
    mango_group.checked_sub_deposit(token_index, quantity)
}

fn checked_sub_borrow(
    mango_group: &mut MangoGroup,
    margin_account: &mut MarginAccount,
    token_index: usize,
    quantity: U64F64
) -> MangoResult<()> {
    margin_account.checked_sub_borrow(token_index, quantity)?;
    mango_group.checked_sub_borrow(token_index, quantity)
}

fn checked_add_deposit(
    mango_group: &mut MangoGroup,
    margin_account: &mut MarginAccount,
    token_index: usize,
    quantity: U64F64
) -> MangoResult<()> {
    margin_account.checked_add_deposit(token_index, quantity)?;
    mango_group.checked_add_deposit(token_index, quantity)
}

fn checked_add_borrow(
    mango_group: &mut MangoGroup,
    margin_account: &mut MarginAccount,
    token_index: usize,
    quantity: U64F64
) -> MangoResult<()> {
    margin_account.checked_add_borrow(token_index, quantity)?;
    mango_group.checked_add_borrow(token_index, quantity)
}

pub fn get_prices(
    mango_group: &MangoGroup,
    oracle_accs: &[AccountInfo]
) -> MangoResult<[U64F64; NUM_TOKENS]> {
    let mut prices = [U64F64::from_num(0); NUM_TOKENS];
    prices[NUM_MARKETS] = U64F64::from_num(1);  // quote currency is 1
    let quote_decimals: u8 = mango_group.mint_decimals[NUM_MARKETS];

    for i in 0..NUM_MARKETS {
        prog_assert_eq!(&mango_group.oracles[i], oracle_accs[i].key)?;

        // TODO store this info in MangoGroup, first make sure it cannot be changed by solink
        let quote_adj = U64F64::from_num(
            10u64.pow(quote_decimals.checked_sub(mango_group.oracle_decimals[i]).unwrap() as u32)
        );

        let answer = flux_aggregator::read_median(&oracle_accs[i])?; // this is in USD cents
        let value = U64F64::from_num(answer.median);

        let base_adj = U64F64::from_num(10u64.pow(mango_group.mint_decimals[i] as u32));
        prices[i] = quote_adj
            .checked_div(base_adj).unwrap()
            .checked_mul(value).unwrap();
    }
    Ok(prices)
}

pub fn invoke_settle_funds<'a>(
    dex_prog_acc: &AccountInfo<'a>,
    spot_market_acc: &AccountInfo<'a>,
    open_orders_acc: &AccountInfo<'a>,
    signer_acc: &AccountInfo<'a>,
    dex_base_acc: &AccountInfo<'a>,
    dex_quote_acc: &AccountInfo<'a>,
    base_vault_acc: &AccountInfo<'a>,
    quote_vault_acc: &AccountInfo<'a>,
    dex_signer_acc: &AccountInfo<'a>,
    token_prog_acc: &AccountInfo<'a>,
    signers_seeds: &[&[&[u8]]]
) -> ProgramResult {
    let data = serum_dex::instruction::MarketInstruction::SettleFunds.pack();
    let instruction = Instruction {
        program_id: *dex_prog_acc.key,
        data,
        accounts: vec![
            AccountMeta::new(*spot_market_acc.key, false),
            AccountMeta::new(*open_orders_acc.key, false),
            AccountMeta::new_readonly(*signer_acc.key, true),
            AccountMeta::new(*dex_base_acc.key, false),
            AccountMeta::new(*dex_quote_acc.key, false),
            AccountMeta::new(*base_vault_acc.key, false),
            AccountMeta::new(*quote_vault_acc.key, false),
            AccountMeta::new_readonly(*dex_signer_acc.key, false),
            AccountMeta::new_readonly(*token_prog_acc.key, false),
        ],
    };

    let account_infos = [
        dex_prog_acc.clone(),
        spot_market_acc.clone(),
        open_orders_acc.clone(),
        signer_acc.clone(),
        dex_base_acc.clone(),
        dex_quote_acc.clone(),
        base_vault_acc.clone(),
        quote_vault_acc.clone(),
        dex_signer_acc.clone(),
        token_prog_acc.clone()
    ];
    solana_program::program::invoke_signed(&instruction, &account_infos, signers_seeds)
}

pub fn invoke_cancel_order<'a>(
    dex_prog_acc: &AccountInfo<'a>,
    spot_market_acc: &AccountInfo<'a>,
    bids_acc: &AccountInfo<'a>,
    asks_acc: &AccountInfo<'a>,
    open_orders_acc: &AccountInfo<'a>,
    signer_acc: &AccountInfo<'a>,
    dex_event_queue_acc: &AccountInfo<'a>,
    data: Vec<u8>,
    signers_seeds: &[&[&[u8]]]
) -> ProgramResult {
    let instruction = Instruction {
        program_id: *dex_prog_acc.key,
        data,
        accounts: vec![
            AccountMeta::new(*spot_market_acc.key, false),
            AccountMeta::new(*bids_acc.key, false),
            AccountMeta::new(*asks_acc.key, false),
            AccountMeta::new(*open_orders_acc.key, false),
            AccountMeta::new_readonly(*signer_acc.key, true),
            AccountMeta::new(*dex_event_queue_acc.key, false),

        ],
    };

    let account_infos = [
        dex_prog_acc.clone(),
        spot_market_acc.clone(),
        bids_acc.clone(),
        asks_acc.clone(),
        open_orders_acc.clone(),
        signer_acc.clone(),
        dex_event_queue_acc.clone()
    ];
    solana_program::program::invoke_signed(&instruction, &account_infos, signers_seeds)
}
