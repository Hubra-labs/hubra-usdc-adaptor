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
//! marginfi lets anyone close an account that is empty and has had no
//! deposit/withdraw for 60 days (`admin_close_account`, rent goes to P0's fee
//! wallet). A closed account reports value 0, and the next deposit re-opens
//! it, paying rent from the strategy authority's own lamports: fund that PDA
//! with a little SOL (scripts/setup-hubra-adaptor.ts does).
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
/// themselves are pinned; `mint` is a layout check on the bank's data.
pub struct P0Market {
    pub bank: Pubkey,
    pub group: Pubkey,
    pub mint: Pubkey,
}

/// Registered markets, keyed by `bank` (== Voltr strategy id).
pub const P0_MARKETS: &[P0Market] = &[
    // USDC, main P0 group
    P0Market {
        bank: pubkey!("2s37akK2eyBbp8DZgCm7RtsaEz8eJP3Nxd4urLHQv7yB"),
        group: pubkey!("4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG8"),
        mint: pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
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
/// marginfi `ASSET_TAG_DEFAULT`: a plain lending bank (not Kamino/Drift/... wrapped).
const ASSET_TAG_DEFAULT: u8 = 0;
const ACCOUNT_INDEX: u16 = 0;
const THIRD_PARTY_ID: u16 = 0;

/// Byte offsets into the raw accounts (8-byte anchor discriminator included),
/// from marginfi's `#[repr(C)]` zero-copy layouts: Bank (3904 bytes) and
/// MarginfiAccount (2304 bytes), each holding I80F48 values as i128 LE.
/// marginfi is upgradeable, so `position_value_from_data` cross-checks fields
/// it can verify (mint, group, share totals, balance shape) and fails rather
/// than report a value if the layout ever moves.
const BANK_LEN: usize = 8 + 3904;
const BANK_MINT: usize = 8;
const BANK_GROUP: usize = 8 + 33;
const BANK_ASSET_SHARE_VALUE: usize = 8 + 72;
const BANK_TOTAL_ASSET_SHARES: usize = 8 + 264;
const ACCOUNT_LEN: usize = 8 + 2304;
const ACCOUNT_GROUP: usize = 8;
const ACCOUNT_AUTHORITY: usize = 8 + 32;
const ACCOUNT_BALANCES: usize = 8 + 64;
const BALANCE_LEN: usize = 104;
const MAX_BALANCES: usize = 16;
const BALANCE_ACTIVE: usize = 0;
const BALANCE_BANK: usize = 1;
const BALANCE_ASSET_TAG: usize = 33;
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
    market: &P0Market,
    authority: &Pubkey,
) -> Result<u64> {
    require_keys_eq!(*bank.owner, P0_PROGRAM_ID, AdaptorError::InvalidProtocolVault);
    require_keys_eq!(*marginfi_account.owner, P0_PROGRAM_ID, AdaptorError::InvalidProtocolAccount);
    position_value_from_data(
        &bank.try_borrow_data()?,
        &marginfi_account.try_borrow_data()?,
        market,
        authority,
    )
}

/// Decodes the position from raw bank + marginfi account data, failing on
/// anything that doesn't fit the expected layout instead of reporting a value.
pub fn position_value_from_data(
    bank: &[u8],
    acc: &[u8],
    market: &P0Market,
    authority: &Pubkey,
) -> Result<u64> {
    require!(
        bank.len() >= BANK_LEN
            && bank[..8] == BANK_ACCOUNT_DISC
            && bank[BANK_MINT..BANK_MINT + 32] == market.mint.to_bytes()
            && bank[BANK_GROUP..BANK_GROUP + 32] == market.group.to_bytes(),
        AdaptorError::InvalidProtocolVault
    );
    let share_value = read_i128(bank, BANK_ASSET_SHARE_VALUE)?;
    let bank_total_shares = read_i128(bank, BANK_TOTAL_ASSET_SHARES)?;

    require!(
        acc.len() >= ACCOUNT_LEN
            && acc[..8] == MARGINFI_ACCOUNT_DISC
            && acc[ACCOUNT_GROUP..ACCOUNT_GROUP + 32] == market.group.to_bytes()
            && acc[ACCOUNT_AUTHORITY..ACCOUNT_AUTHORITY + 32] == authority.to_bytes(),
        AdaptorError::InvalidProtocolAccount
    );

    // Only the strategy authority can add balances, and it only ever lends to
    // `market.bank`, so every active slot must be that bank. Anything else
    // means the layout moved (or something is badly wrong): fail, don't report.
    let mut value = None;
    for i in 0..MAX_BALANCES {
        let b = &acc[ACCOUNT_BALANCES + i * BALANCE_LEN..ACCOUNT_BALANCES + (i + 1) * BALANCE_LEN];
        match b[BALANCE_ACTIVE] {
            0 => continue,
            1 => {}
            _ => return err!(AdaptorError::InvalidProtocolAccount),
        }
        require!(
            value.is_none()
                && b[BALANCE_BANK..BALANCE_BANK + 32] == market.bank.to_bytes()
                && b[BALANCE_ASSET_TAG] == ASSET_TAG_DEFAULT
                && read_i128(b, BALANCE_LIABILITY_SHARES)? == 0,
            AdaptorError::InvalidProtocolAccount
        );
        let shares = read_i128(b, BALANCE_ASSET_SHARES)?;
        require!(shares <= bank_total_shares, AdaptorError::InvalidProtocolAccount);
        value = Some(shares_to_amount(shares, share_value)?);
    }
    Ok(value.unwrap_or(0))
}

/// marginfi_account_initialize_pda(ACCOUNT_INDEX, None) for `authority`, rent
/// paid by `payer`. Account order per program: group, marginfi_account,
/// authority, fee_payer, instructions_sysvar, system_program.
fn init_marginfi_account<'info>(
    p0_program: &AccountInfo<'info>,
    group: &AccountInfo<'info>,
    marginfi_account: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    payer: &AccountInfo<'info>,
    instructions: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
) -> Result<()> {
    let mut data = Vec::with_capacity(11);
    data.extend_from_slice(&INITIALIZE_PDA_DISC);
    data.extend_from_slice(&ACCOUNT_INDEX.to_le_bytes());
    data.push(0); // third_party_id: None (seed uses 0)
    invoke(
        &Instruction {
            program_id: P0_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(group.key(), false),
                AccountMeta::new(marginfi_account.key(), false),
                AccountMeta::new_readonly(authority.key(), true),
                AccountMeta::new(payer.key(), true),
                AccountMeta::new_readonly(instructions.key(), false),
                AccountMeta::new_readonly(system_program.key(), false),
            ],
            data,
        },
        &[
            group.clone(),
            marginfi_account.clone(),
            authority.clone(),
            payer.clone(),
            instructions.clone(),
            system_program.clone(),
            p0_program.clone(),
        ],
    )?;
    Ok(())
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
/// for withdraw's health check, and what re-opening a closed account needs.
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

    /// CHECK: address-checked; marginfi's init_pda reads it when re-opening.
    #[account(address = instructions_sysvar::ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

impl<'info> P0Operate<'info> {
    /// The marginfi account exists. Its address is pinned to the PDA, which only
    /// marginfi can allocate, so anything else means never opened or closed by
    /// `admin_close_account` (only possible while empty).
    fn account_open(&self) -> bool {
        *self.marginfi_account.owner == P0_PROGRAM_ID
    }

    /// Re-creates a closed marginfi account, rent paid by the strategy authority.
    fn reopen_account(&self) -> Result<()> {
        msg!("P0: re-opening closed marginfi account");
        init_marginfi_account(
            &self.p0_program.to_account_info(),
            &self.marginfi_group.to_account_info(),
            &self.marginfi_account.to_account_info(),
            &self.voltr.vault_strategy_auth.to_account_info(),
            &self.voltr.vault_strategy_auth.to_account_info(),
            &self.instructions_sysvar.to_account_info(),
            &self.system_program.to_account_info(),
        )
    }

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

    /// Refresh: accrue the bank, then report. A closed account is empty, so
    /// it reports 0 without touching marginfi.
    fn refresh(&self) -> Result<u64> {
        if !self.account_open() {
            return Ok(0);
        }
        self.accrue_interest()?;
        self.position_value()
    }

    /// Position value in USDC: asset shares x the bank's asset_share_value
    /// (0 while the account is closed).
    fn position_value(&self) -> Result<u64> {
        if !self.account_open() {
            return Ok(0);
        }
        read_position_value(
            &self.bank.to_account_info(),
            &self.marginfi_account.to_account_info(),
            market_for(self.voltr.strategy.key)?,
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
    init_marginfi_account(
        &a.p0_program.to_account_info(),
        &a.marginfi_group.to_account_info(),
        &a.marginfi_account.to_account_info(),
        &a.voltr.vault_strategy_auth.to_account_info(),
        &a.voltr.payer.to_account_info(),
        &a.instructions_sysvar.to_account_info(),
        &a.voltr.system_program.to_account_info(),
    )
}

/// Lends the `amount` of USDC the vault just moved into the strategy ATA and
/// returns the position value. `amount == 0` only refreshes the position.
/// Re-opens the marginfi account first if it was closed.
pub fn deposit(ctx: Context<P0Operate>, amount: u64) -> Result<u64> {
    let a = ctx.accounts;
    a.validate()?;
    if amount == 0 {
        return a.refresh();
    }
    if !a.account_open() {
        a.reopen_account()?;
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
        return a.refresh();
    }
    require!(a.account_open(), AdaptorError::NoPosition);
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

    fn market() -> &'static P0Market {
        &P0_MARKETS[0]
    }

    /// Bank with asset_share_value 1.25 and 1M total shares.
    fn bank_data() -> Vec<u8> {
        let m = market();
        let mut d = vec![0u8; BANK_LEN];
        d[..8].copy_from_slice(&BANK_ACCOUNT_DISC);
        d[BANK_MINT..BANK_MINT + 32].copy_from_slice(&m.mint.to_bytes());
        d[BANK_GROUP..BANK_GROUP + 32].copy_from_slice(&m.group.to_bytes());
        d[BANK_ASSET_SHARE_VALUE..BANK_ASSET_SHARE_VALUE + 16].copy_from_slice(&(ONE + ONE / 4).to_le_bytes());
        d[BANK_TOTAL_ASSET_SHARES..BANK_TOTAL_ASSET_SHARES + 16].copy_from_slice(&(1_000_000 * ONE).to_le_bytes());
        d
    }

    /// marginfi account for `auth` with one active balance of `shares` (whole shares) in slot `slot`.
    fn account_data(auth: &Pubkey, slot: usize, shares: i128) -> Vec<u8> {
        let m = market();
        let mut d = vec![0u8; ACCOUNT_LEN];
        d[..8].copy_from_slice(&MARGINFI_ACCOUNT_DISC);
        d[ACCOUNT_GROUP..ACCOUNT_GROUP + 32].copy_from_slice(&m.group.to_bytes());
        d[ACCOUNT_AUTHORITY..ACCOUNT_AUTHORITY + 32].copy_from_slice(&auth.to_bytes());
        let b = ACCOUNT_BALANCES + slot * BALANCE_LEN;
        d[b + BALANCE_ACTIVE] = 1;
        d[b + BALANCE_BANK..b + BALANCE_BANK + 32].copy_from_slice(&m.bank.to_bytes());
        d[b + BALANCE_ASSET_SHARES..b + BALANCE_ASSET_SHARES + 16].copy_from_slice(&(shares * ONE).to_le_bytes());
        d
    }

    #[test]
    fn position_decodes_expected_layout() {
        let auth = Pubkey::new_unique();
        // 1000 shares x 1.25, in any slot
        for slot in [0, 7, 15] {
            let v = position_value_from_data(&bank_data(), &account_data(&auth, slot, 1000), market(), &auth).unwrap();
            assert_eq!(v, 1250);
        }
        // no active balance: empty position
        let mut empty = account_data(&auth, 0, 1000);
        empty[ACCOUNT_BALANCES + BALANCE_ACTIVE] = 0;
        assert_eq!(position_value_from_data(&bank_data(), &empty, market(), &auth).unwrap(), 0);
    }

    #[test]
    fn position_rejects_unexpected_bank_data() {
        let auth = Pubkey::new_unique();
        let acc = account_data(&auth, 0, 1000);
        for (off, name) in [(BANK_MINT, "mint"), (BANK_GROUP, "group"), (0, "discriminator")] {
            let mut bank = bank_data();
            bank[off] ^= 0xff;
            assert!(position_value_from_data(&bank, &acc, market(), &auth).is_err(), "bank {name}");
        }
        assert!(position_value_from_data(&bank_data()[..BANK_LEN - 1], &acc, market(), &auth).is_err(), "short bank");
        // more shares than the bank has in total
        let big = account_data(&auth, 0, 2_000_000);
        assert!(position_value_from_data(&bank_data(), &big, market(), &auth).is_err(), "shares > bank total");
    }

    #[test]
    fn position_rejects_unexpected_account_data() {
        let auth = Pubkey::new_unique();
        let bank = bank_data();
        let base = account_data(&auth, 0, 1000);
        let b = ACCOUNT_BALANCES;
        let cases: Vec<(&str, Box<dyn Fn(&mut Vec<u8>)>)> = vec![
            ("other authority", Box::new(|d| d[ACCOUNT_AUTHORITY] ^= 0xff)),
            ("other group", Box::new(|d| d[ACCOUNT_GROUP] ^= 0xff)),
            ("active flag not 0/1", Box::new(move |d| d[b + BALANCE_ACTIVE] = 2)),
            ("active balance in another bank", Box::new(move |d| d[b + BALANCE_BANK] ^= 0xff)),
            ("wrapped asset tag", Box::new(move |d| d[b + BALANCE_ASSET_TAG] = 3)),
            ("liability", Box::new(move |d| d[b + BALANCE_LIABILITY_SHARES] = 1)),
            ("second active balance", Box::new(move |d| {
                let (src, dst) = (b, b + BALANCE_LEN);
                let slot: Vec<u8> = d[src..src + BALANCE_LEN].to_vec();
                d[dst..dst + BALANCE_LEN].copy_from_slice(&slot);
            })),
            // a layout shift: balances start 8 bytes later than we assume
            ("balances shifted", Box::new(move |d| {
                let moved: Vec<u8> = d[b..ACCOUNT_LEN - 8].to_vec();
                d[b + 8..].copy_from_slice(&moved);
                d[b..b + 8].fill(0x01);
            })),
        ];
        for (name, mutate) in cases {
            let mut acc = base.clone();
            mutate(&mut acc);
            assert!(position_value_from_data(&bank, &acc, market(), &auth).is_err(), "{name}");
        }
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
