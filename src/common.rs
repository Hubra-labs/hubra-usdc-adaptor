//! Protocol-agnostic building blocks: the fixed Voltr account prefixes, the
//! instant-settlement invariants, share math and CPI return-data reading.
//! Every protocol module composes these instead of re-implementing them.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::get_return_data;
use anchor_spl::associated_token::get_associated_token_address_with_program_id;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};

use crate::error::AdaptorError;

pub const BPS_DENOM: u64 = 10_000;

/// Fixed account prefix of the Voltr vault's `initialize_strategy` CPI.
#[derive(Accounts)]
pub struct VoltrInitialize<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    /// Vault strategy authority PDA (signed by the Voltr vault program).
    #[account(mut)]
    pub vault_strategy_auth: Signer<'info>,

    /// CHECK: Voltr strategy id; each protocol binds it to a registered market.
    pub strategy: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

/// Fixed account prefix of the Voltr vault's `deposit_strategy` /
/// `withdraw_strategy` CPIs.
#[derive(Accounts)]
pub struct VoltrOperate<'info> {
    /// Vault strategy authority PDA (signed by the Voltr vault program); acts
    /// as the depositor towards the protocol.
    #[account(mut)]
    pub vault_strategy_auth: Signer<'info>,

    /// CHECK: Voltr strategy id; each protocol binds it to a registered market.
    pub strategy: UncheckedAccount<'info>,

    pub vault_asset_mint: Box<InterfaceAccount<'info, Mint>>,

    /// The strategy's USDC ATA: the vault funds it before `deposit` and sweeps
    /// `amount` out of it after `withdraw`.
    #[account(
        mut,
        associated_token::mint = vault_asset_mint,
        associated_token::authority = vault_strategy_auth,
        associated_token::token_program = asset_token_program,
    )]
    pub vault_strategy_asset_ata: Box<InterfaceAccount<'info, TokenAccount>>,

    pub asset_token_program: Interface<'info, TokenInterface>,
}

impl<'info> VoltrOperate<'info> {
    /// Current balance of the strategy's asset ATA, re-read from account data
    /// so it reflects any CPI that ran since deserialization.
    pub fn asset_balance(&mut self) -> Result<u64> {
        self.vault_strategy_asset_ata.reload()?;
        Ok(self.vault_strategy_asset_ata.amount)
    }
}

/// Instant-deposit invariant: the protocol must pull exactly `amount` out of
/// the strategy asset ATA within this instruction. Anything left behind would
/// be invisible to the vault's position accounting.
pub fn settle_deposit(balance_before: u64, balance_after: u64, amount: u64) -> Result<()> {
    let consumed = balance_before
        .checked_sub(balance_after)
        .ok_or(AdaptorError::DepositNotSettled)?;
    require_eq!(consumed, amount, AdaptorError::DepositNotSettled);
    Ok(())
}

/// Instant-withdraw invariant: after the protocol redemption the strategy
/// asset ATA must already hold `amount`, which the vault sweeps right after.
pub fn settle_withdraw(balance_after: u64, amount: u64) -> Result<()> {
    require_gte!(balance_after, amount, AdaptorError::WithdrawNotSettled);
    Ok(())
}

/// Shares to redeem so that at least `amount` of underlying comes out, given
/// the position's current value and share balance. `amount` is grossed up by
/// `margin_bps` to absorb redemption fees/spread; the result is rounded up and
/// clamped to the balance, so a large margin can never over-burn. Requests at
/// or above the (grossed-down) position value redeem everything.
pub fn shares_for_amount(
    amount: u64,
    position_value: u64,
    share_balance: u64,
    margin_bps: u64,
) -> Result<u64> {
    require!(position_value > 0 && share_balance > 0, AdaptorError::NoPosition);
    let gross = (amount as u128)
        .checked_mul((BPS_DENOM + margin_bps) as u128)
        .ok_or(AdaptorError::MathOverflow)?
        .div_ceil(BPS_DENOM as u128);
    if gross >= position_value as u128 {
        return Ok(share_balance);
    }
    let shares = gross
        .checked_mul(share_balance as u128)
        .ok_or(AdaptorError::MathOverflow)?
        .div_ceil(position_value as u128)
        .min(share_balance as u128);
    // shares <= share_balance (a u64), so this cannot truncate.
    Ok(shares as u64)
}

/// Read a u64 LE value from the return data of the CPI just made to `program`.
pub fn read_u64_return_data(program: &Pubkey) -> Result<u64> {
    let (returning_program, data) =
        get_return_data().ok_or(AdaptorError::MissingReturnData)?;
    require_keys_eq!(returning_program, *program, AdaptorError::MissingReturnData);
    let bytes: [u8; 8] = data
        .get(..8)
        .and_then(|b| b.try_into().ok())
        .ok_or(AdaptorError::MissingReturnData)?;
    Ok(u64::from_le_bytes(bytes))
}

/// Require `account` to be the canonical ATA of (`authority`, `mint`).
pub fn require_ata(
    account: &Pubkey,
    authority: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Result<()> {
    let expected = get_associated_token_address_with_program_id(authority, mint, token_program);
    require_keys_eq!(*account, expected, AdaptorError::InvalidShareAta);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_proportional_with_margin() {
        // 1:1 share price, 30bps margin, rounds up.
        assert_eq!(shares_for_amount(100, 1_000, 1_000, 30).unwrap(), 101);
        // share price 2.0 (value 2000 for 1000 shares): 200 underlying -> 101 shares (200.6/2 ceil)
        assert_eq!(shares_for_amount(200, 2_000, 1_000, 30).unwrap(), 101);
        // zero margin, exact division
        assert_eq!(shares_for_amount(500, 1_000, 1_000, 0).unwrap(), 500);
    }

    #[test]
    fn shares_full_exit_and_clamp() {
        assert_eq!(shares_for_amount(1_000, 1_000, 777, 30).unwrap(), 777);
        assert_eq!(shares_for_amount(u64::MAX, 1_000, 777, 30).unwrap(), 777);
        // grossed-up amount just under value: clamp keeps it <= balance
        assert_eq!(shares_for_amount(996, 1_000, 1_000, 30).unwrap(), 999);
    }

    #[test]
    fn shares_no_position() {
        assert!(shares_for_amount(1, 0, 10, 30).is_err());
        assert!(shares_for_amount(1, 10, 0, 30).is_err());
    }

    #[test]
    fn settlement_invariants() {
        assert!(settle_deposit(1_000, 400, 600).is_ok());
        assert!(settle_deposit(1_000, 401, 600).is_err()); // under-consumed
        assert!(settle_deposit(1_000, 399, 600).is_err()); // over-consumed
        assert!(settle_deposit(100, 200, 0).is_err()); // balance grew
        assert!(settle_withdraw(600, 600).is_ok());
        assert!(settle_withdraw(599, 600).is_err());
    }
}
