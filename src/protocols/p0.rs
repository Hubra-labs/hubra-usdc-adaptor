//! P0 (Project 0, formerly marginfi v2) lending banks.
//!
//! The strategy authority owns a marginfi account PDA (created through
//! `marginfi_account_initialize_pda`, account_index 0, no third-party id) and
//! lends USDC into one bank via `lending_account_deposit` /
//! `lending_account_withdraw`. Yield accrues in the bank's
//! `asset_share_value`; position value is the account's asset shares times
//! that value, both read from marginfi-owned state right after marginfi itself
//! accrued interest in the same instruction. Settlement is instant as long as
//! the bank's liquidity vault holds enough free USDC.
//!
//! Strategy id convention: the marginfi bank account.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::invoke;
use anchor_lang::solana_program::sysvar::instructions as instructions_sysvar;

use crate::common::*;
use crate::error::AdaptorError;

/// marginfi v2 program (mainnet), which P0 runs on.
pub const P0_PROGRAM_ID: Pubkey = pubkey!("MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA");

/// A marginfi bank this adaptor may lend into. marginfi validates the bank
/// against the group and its own vaults/oracles, so only the group and bank
/// themselves are pinned.
pub struct P0Market {
    pub bank: Pubkey,
    pub group: Pubkey,
}

/// Registered markets, keyed by `bank` (== Voltr strategy id).
pub const P0_MARKETS: &[P0Market] = &[
    // USDC, main P0 group
    P0Market {
        bank: pubkey!("2s37akK2eyBbp8DZgCm7RtsaEz8eJP3Nxd4urLHQv7yB"),
        group: pubkey!("4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG8"),
    },
];

/// marginfi instruction / account discriminators (anchor sha256 hashes).
const INITIALIZE_PDA_DISC: [u8; 8] = [87, 177, 91, 80, 218, 119, 245, 31];
const DEPOSIT_DISC: [u8; 8] = [171, 94, 235, 103, 82, 64, 212, 140];
const WITHDRAW_DISC: [u8; 8] = [36, 72, 74, 19, 210, 210, 192, 192];
const ACCRUE_INTEREST_DISC: [u8; 8] = [108, 201, 30, 87, 47, 65, 97, 188];
const BANK_ACCOUNT_DISC: [u8; 8] = [142, 49, 166, 242, 50, 66, 97, 188];
const MARGINFI_ACCOUNT_DISC: [u8; 8] = [67, 178, 130, 109, 126, 114, 28, 42];

const MARGINFI_ACCOUNT_SEED: &[u8] = b"marginfi_account";
const ACCOUNT_INDEX: u16 = 0;
const THIRD_PARTY_ID: u16 = 0;

/// Byte offsets into the raw accounts (8-byte anchor discriminator included),
/// from marginfi's `#[repr(C)]` zero-copy layouts: Bank (3904 bytes) and
/// MarginfiAccount (2304 bytes), each holding I80F48 values as i128 LE.
const BANK_LEN: usize = 8 + 3904;
const BANK_ASSET_SHARE_VALUE: usize = 8 + 72;
const ACCOUNT_LEN: usize = 8 + 2304;
const ACCOUNT_GROUP: usize = 8;
const ACCOUNT_AUTHORITY: usize = 8 + 32;
const ACCOUNT_BALANCES: usize = 8 + 64;
const BALANCE_LEN: usize = 104;
const MAX_BALANCES: usize = 16;
const BALANCE_ACTIVE: usize = 0;
const BALANCE_BANK: usize = 1;
const BALANCE_ASSET_SHARES: usize = 40;
const BALANCE_LIABILITY_SHARES: usize = 56;

fn market_for(strategy: &Pubkey) -> Result<&'static P0Market> {
    P0_MARKETS
        .iter()
        .find(|m| m.bank == *strategy)
        .ok_or_else(|| error!(AdaptorError::UnknownStrategy))
}

/// The strategy authority's marginfi account PDA in `group`.
pub fn marginfi_account_address(group: &Pubkey, authority: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            MARGINFI_ACCOUNT_SEED,
            group.as_ref(),
            authority.as_ref(),
            &ACCOUNT_INDEX.to_le_bytes(),
            &THIRD_PARTY_ID.to_le_bytes(),
        ],
        &P0_PROGRAM_ID,
    )
    .0
}

/// Bind the protocol-side accounts to the market registered for `strategy`,
/// and the marginfi account to the strategy authority's own PDA.
fn validate_market(
    strategy: &Pubkey,
    p0_program: &Pubkey,
    group: &Pubkey,
    bank: &Pubkey,
    marginfi_account: &Pubkey,
    authority: &Pubkey,
) -> Result<&'static P0Market> {
    require_keys_eq!(*p0_program, P0_PROGRAM_ID, AdaptorError::InvalidProtocolProgram);
    let market = market_for(strategy)?;
    require_keys_eq!(*bank, market.bank, AdaptorError::InvalidProtocolVault);
    require_keys_eq!(*group, market.group, AdaptorError::InvalidProtocolAccount);
    require_keys_eq!(
        *marginfi_account,
        marginfi_account_address(&market.group, authority),
        AdaptorError::InvalidProtocolAccount
    );
    Ok(market)
}

fn read_i128(data: &[u8], offset: usize) -> Result<i128> {
    let bytes: [u8; 16] = data
        .get(offset..offset + 16)
        .and_then(|b| b.try_into().ok())
        .ok_or(AdaptorError::InvalidProtocolAccount)?;
    Ok(i128::from_le_bytes(bytes))
}

/// floor(shares * share_value) for two I80F48 raw values (48 fractional bits),
/// split so the product stays in u128 for any realistic position.
pub fn shares_to_amount(shares: i128, share_value: i128) -> Result<u64> {
    require!(shares >= 0 && share_value > 0, AdaptorError::InvalidProtocolAccount);
    let (s, v) = (shares as u128, share_value as u128);
    let frac_mask = (1u128 << 48) - 1;
    let value_fx48 = (s >> 48)
        .checked_mul(v)
        .and_then(|hi| hi.checked_add(((s & frac_mask) * v) >> 48))
        .ok_or(AdaptorError::MathOverflow)?;
    u64::try_from(value_fx48 >> 48).map_err(|_| error!(AdaptorError::MathOverflow))
}

/// Position value of `marginfi_account` in `bank`, from marginfi-owned state.
fn read_position_value(
    bank: &AccountInfo,
    marginfi_account: &AccountInfo,
    group: &Pubkey,
    authority: &Pubkey,
) -> Result<u64> {
    require_keys_eq!(*bank.owner, P0_PROGRAM_ID, AdaptorError::InvalidProtocolVault);
    let bank_data = bank.try_borrow_data()?;
    require!(
        bank_data.len() >= BANK_LEN && bank_data[..8] == BANK_ACCOUNT_DISC,
        AdaptorError::InvalidProtocolVault
    );
    let share_value = read_i128(&bank_data, BANK_ASSET_SHARE_VALUE)?;

    require_keys_eq!(*marginfi_account.owner, P0_PROGRAM_ID, AdaptorError::InvalidProtocolAccount);
    let acc = marginfi_account.try_borrow_data()?;
    require!(
        acc.len() >= ACCOUNT_LEN
            && acc[..8] == MARGINFI_ACCOUNT_DISC
            && acc[ACCOUNT_GROUP..ACCOUNT_GROUP + 32] == group.to_bytes()
            && acc[ACCOUNT_AUTHORITY..ACCOUNT_AUTHORITY + 32] == authority.to_bytes(),
        AdaptorError::InvalidProtocolAccount
    );

    for i in 0..MAX_BALANCES {
        let b = &acc[ACCOUNT_BALANCES + i * BALANCE_LEN..ACCOUNT_BALANCES + (i + 1) * BALANCE_LEN];
        if b[BALANCE_ACTIVE] == 0 || b[BALANCE_BANK..BALANCE_BANK + 32] != bank.key.to_bytes() {
            continue;
        }
        // The strategy only ever lends; a liability here means something is badly wrong.
        require!(read_i128(b, BALANCE_LIABILITY_SHARES)? == 0, AdaptorError::InvalidProtocolAccount);
        return shares_to_amount(read_i128(b, BALANCE_ASSET_SHARES)?, share_value);
    }
    Ok(0)
}

// ------------------------------------------------------------------ accounts

#[derive(Accounts)]
pub struct P0Initialize<'info> {
    pub voltr: VoltrInitialize<'info>,

    // ---- remaining accounts ----
    /// CHECK: pinned to the marginfi program id.
    pub p0_program: UncheckedAccount<'info>,

    /// CHECK: pinned to the strategy's registered market.
    pub marginfi_group: UncheckedAccount<'info>,

    /// CHECK: pinned to the strategy's registered market.
    pub bank: UncheckedAccount<'info>,

    /// CHECK: must be the strategy authority's marginfi account PDA; created here.
    #[account(mut)]
    pub marginfi_account: UncheckedAccount<'info>,

    /// CHECK: address-checked; marginfi's init_pda reads it for CPI validation.
    #[account(address = instructions_sysvar::ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,
}

/// Shared by deposit and withdraw. Remaining accounts mirror marginfi's
/// lending_account_deposit / lending_account_withdraw plus the bank's oracle
/// for withdraw's health check.
#[derive(Accounts)]
pub struct P0Operate<'info> {
    pub voltr: VoltrOperate<'info>,

    // ---- remaining accounts ----
    /// CHECK: pinned to the marginfi program id.
    pub p0_program: UncheckedAccount<'info>,

    /// CHECK: pinned to the strategy's registered market.
    pub marginfi_group: UncheckedAccount<'info>,

    /// CHECK: must be the strategy authority's marginfi account PDA.
    #[account(mut)]
    pub marginfi_account: UncheckedAccount<'info>,

    /// CHECK: pinned to the strategy's registered market.
    #[account(mut)]
    pub bank: UncheckedAccount<'info>,

    /// CHECK: validated by marginfi against the bank (`has_one`).
    #[account(mut)]
    pub liquidity_vault: UncheckedAccount<'info>,

    /// CHECK: PDA-validated by marginfi.
    pub liquidity_vault_authority: UncheckedAccount<'info>,

    /// CHECK: validated by marginfi against the bank's oracle config.
    pub bank_oracle: UncheckedAccount<'info>,
}

impl<'info> P0Operate<'info> {
    fn validate(&self) -> Result<()> {
        validate_market(
            self.voltr.strategy.key,
            self.p0_program.key,
            self.marginfi_group.key,
            self.bank.key,
            self.marginfi_account.key,
            self.voltr.vault_strategy_auth.key,
        )?;
        Ok(())
    }

    /// lending_account_deposit(amount, None). Account order per program:
    /// group, marginfi_account, authority, bank, signer_token_account,
    /// liquidity_vault, token_program.
    fn deposit(&self, amount: u64) -> Result<()> {
        let v = &self.voltr;
        let mut data = Vec::with_capacity(17);
        data.extend_from_slice(&DEPOSIT_DISC);
        data.extend_from_slice(&amount.to_le_bytes());
        data.push(0); // deposit_up_to_limit: None (deposit exactly `amount` or fail)
        invoke(
            &Instruction {
                program_id: P0_PROGRAM_ID,
                accounts: vec![
                    AccountMeta::new_readonly(self.marginfi_group.key(), false),
                    AccountMeta::new(self.marginfi_account.key(), false),
                    AccountMeta::new_readonly(v.vault_strategy_auth.key(), true),
                    AccountMeta::new(self.bank.key(), false),
                    AccountMeta::new(v.vault_strategy_asset_ata.key(), false),
                    AccountMeta::new(self.liquidity_vault.key(), false),
                    AccountMeta::new_readonly(v.asset_token_program.key(), false),
                ],
                data,
            },
            &[
                self.marginfi_group.to_account_info(),
                self.marginfi_account.to_account_info(),
                v.vault_strategy_auth.to_account_info(),
                self.bank.to_account_info(),
                v.vault_strategy_asset_ata.to_account_info(),
                self.liquidity_vault.to_account_info(),
                v.asset_token_program.to_account_info(),
                self.p0_program.to_account_info(),
            ],
        )?;
        Ok(())
    }

    /// lending_account_withdraw(amount, None). Account order per program:
    /// group, marginfi_account, authority, bank, destination_token_account,
    /// bank_liquidity_vault_authority, liquidity_vault, token_program, then
    /// the health-check accounts [bank, oracle] (the account's only balance).
    fn withdraw(&self, amount: u64) -> Result<()> {
        let v = &self.voltr;
        let mut data = Vec::with_capacity(17);
        data.extend_from_slice(&WITHDRAW_DISC);
        data.extend_from_slice(&amount.to_le_bytes());
        data.push(0); // withdraw_all: None
        invoke(
            &Instruction {
                program_id: P0_PROGRAM_ID,
                accounts: vec![
                    AccountMeta::new_readonly(self.marginfi_group.key(), false),
                    AccountMeta::new(self.marginfi_account.key(), false),
                    AccountMeta::new_readonly(v.vault_strategy_auth.key(), true),
                    AccountMeta::new(self.bank.key(), false),
                    AccountMeta::new(v.vault_strategy_asset_ata.key(), false),
                    AccountMeta::new_readonly(self.liquidity_vault_authority.key(), false),
                    AccountMeta::new(self.liquidity_vault.key(), false),
                    AccountMeta::new_readonly(v.asset_token_program.key(), false),
                    AccountMeta::new_readonly(self.bank.key(), false),
                    AccountMeta::new_readonly(self.bank_oracle.key(), false),
                ],
                data,
            },
            &[
                self.marginfi_group.to_account_info(),
                self.marginfi_account.to_account_info(),
                v.vault_strategy_auth.to_account_info(),
                self.bank.to_account_info(),
                v.vault_strategy_asset_ata.to_account_info(),
                self.liquidity_vault_authority.to_account_info(),
                self.liquidity_vault.to_account_info(),
                v.asset_token_program.to_account_info(),
                self.bank_oracle.to_account_info(),
                self.p0_program.to_account_info(),
            ],
        )?;
        Ok(())
    }

    /// lending_pool_accrue_bank_interest: brings asset_share_value up to date
    /// for a refresh (deposit/withdraw accrue on their own).
    fn accrue_interest(&self) -> Result<()> {
        invoke(
            &Instruction {
                program_id: P0_PROGRAM_ID,
                accounts: vec![
                    AccountMeta::new_readonly(self.marginfi_group.key(), false),
                    AccountMeta::new(self.bank.key(), false),
                ],
                data: ACCRUE_INTEREST_DISC.to_vec(),
            },
            &[
                self.marginfi_group.to_account_info(),
                self.bank.to_account_info(),
                self.p0_program.to_account_info(),
            ],
        )?;
        Ok(())
    }

    /// Position value in USDC: asset shares x the bank's asset_share_value.
    fn position_value(&self) -> Result<u64> {
        read_position_value(
            &self.bank.to_account_info(),
            &self.marginfi_account.to_account_info(),
            self.marginfi_group.key,
            self.voltr.vault_strategy_auth.key,
        )
    }
}

// ------------------------------------------------------------------ handlers

/// Creates the strategy authority's marginfi account PDA (idempotent).
pub fn initialize(ctx: Context<P0Initialize>) -> Result<()> {
    let a = &ctx.accounts;
    validate_market(
        a.voltr.strategy.key,
        a.p0_program.key,
        a.marginfi_group.key,
        a.bank.key,
        a.marginfi_account.key,
        a.voltr.vault_strategy_auth.key,
    )?;
    if *a.marginfi_account.owner == P0_PROGRAM_ID {
        return Ok(());
    }
    let mut data = Vec::with_capacity(11);
    data.extend_from_slice(&INITIALIZE_PDA_DISC);
    data.extend_from_slice(&ACCOUNT_INDEX.to_le_bytes());
    data.push(0); // third_party_id: None (seed uses 0)
    invoke(
        &Instruction {
            program_id: P0_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(a.marginfi_group.key(), false),
                AccountMeta::new(a.marginfi_account.key(), false),
                AccountMeta::new_readonly(a.voltr.vault_strategy_auth.key(), true),
                AccountMeta::new(a.voltr.payer.key(), true),
                AccountMeta::new_readonly(a.instructions_sysvar.key(), false),
                AccountMeta::new_readonly(a.voltr.system_program.key(), false),
            ],
            data,
        },
        &[
            a.marginfi_group.to_account_info(),
            a.marginfi_account.to_account_info(),
            a.voltr.vault_strategy_auth.to_account_info(),
            a.voltr.payer.to_account_info(),
            a.instructions_sysvar.to_account_info(),
            a.voltr.system_program.to_account_info(),
            a.p0_program.to_account_info(),
        ],
    )?;
    Ok(())
}

/// Lends the `amount` of USDC the vault just moved into the strategy ATA and
/// returns the position value. `amount == 0` only refreshes the position.
pub fn deposit(ctx: Context<P0Operate>, amount: u64) -> Result<u64> {
    let a = ctx.accounts;
    a.validate()?;
    if amount == 0 {
        a.accrue_interest()?;
        return a.position_value();
    }
    let before = a.voltr.vault_strategy_asset_ata.amount;
    a.deposit(amount)?;
    settle_deposit(before, a.voltr.asset_balance()?, amount)?;
    a.position_value()
}

/// Withdraws exactly `amount` USDC into the strategy ATA (the vault sweeps it
/// afterwards) and returns the remaining position value. marginfi fails the
/// withdraw if `amount` exceeds the position or the bank's free liquidity.
/// A full exit requests the current position value (floor); the sub-unit
/// share dust left behind stays counted, so nothing is stranded.
pub fn withdraw(ctx: Context<P0Operate>, amount: u64) -> Result<u64> {
    let a = ctx.accounts;
    a.validate()?;
    if amount == 0 {
        a.accrue_interest()?;
        return a.position_value();
    }
    a.withdraw(amount)?;
    settle_withdraw(a.voltr.asset_balance()?, amount)?;
    a.position_value()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anchor_lang::solana_program::hash::hash;

    const ONE: i128 = 1 << 48;

    fn anchor_disc(preimage: &str) -> [u8; 8] {
        hash(preimage.as_bytes()).to_bytes()[..8].try_into().unwrap()
    }

    #[test]
    fn discriminators_match_anchor_hashes() {
        assert_eq!(INITIALIZE_PDA_DISC, anchor_disc("global:marginfi_account_initialize_pda"));
        assert_eq!(DEPOSIT_DISC, anchor_disc("global:lending_account_deposit"));
        assert_eq!(WITHDRAW_DISC, anchor_disc("global:lending_account_withdraw"));
        assert_eq!(ACCRUE_INTEREST_DISC, anchor_disc("global:lending_pool_accrue_bank_interest"));
        assert_eq!(BANK_ACCOUNT_DISC, anchor_disc("account:Bank"));
        assert_eq!(MARGINFI_ACCOUNT_DISC, anchor_disc("account:MarginfiAccount"));
    }

    #[test]
    fn shares_to_amount_floors() {
        assert_eq!(shares_to_amount(100 * ONE, ONE).unwrap(), 100);
        // 100 shares at 1.25 = 125
        assert_eq!(shares_to_amount(100 * ONE, ONE + ONE / 4).unwrap(), 125);
        // 3 shares at 1.5 = 4.5 -> 4
        assert_eq!(shares_to_amount(3 * ONE, ONE + ONE / 2).unwrap(), 4);
        // fractional shares: 0.5 at 1.0 -> 0
        assert_eq!(shares_to_amount(ONE / 2, ONE).unwrap(), 0);
        assert_eq!(shares_to_amount(0, ONE).unwrap(), 0);
    }

    #[test]
    fn shares_to_amount_large_positions() {
        // 10B USDC (1e16 base units) at share value 3.0 must not overflow.
        let shares = 10_000_000_000_000_000i128 * ONE;
        assert_eq!(shares_to_amount(shares, 3 * ONE).unwrap(), 30_000_000_000_000_000);
    }

    #[test]
    fn shares_to_amount_rejects_bad_state() {
        assert!(shares_to_amount(-ONE, ONE).is_err());
        assert!(shares_to_amount(ONE, 0).is_err());
    }
}
