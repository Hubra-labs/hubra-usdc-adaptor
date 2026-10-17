//! Perena USD* (bankineco vault program).
//!
//! USDC is deposited via `execute_deposit` (USD* shares minted to the strategy
//! authority's ATA); yield accrues in the USD* share price. Position value is
//! read from bankineco's own `get_nav` view instruction, never self-computed.
//! Settlement is instant both ways (mint/redeem against the vault's USDC
//! reserve in the same instruction).
//!
//! Strategy id convention: the Perena vault account (like Kamino's kvault).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::invoke;
use anchor_spl::associated_token::{self, AssociatedToken};
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};

use crate::common::*;
use crate::error::AdaptorError;

/// Perena bankineco program (owns the vaults and their share mints).
pub const BANKINECO_PROGRAM_ID: Pubkey = pubkey!("save8RQVPMWNTzU18t3GBvBkN9hT7jsGjiCQ28FpD9H");

/// A bankineco vault this adaptor may route funds into. Every account that is
/// not validated by bankineco itself is pinned here, so a manager-supplied
/// account set cannot point the adaptor at a lookalike vault (e.g. an empty
/// self-consistent vault/mint pair) and misreport NAV to the Voltr vault.
pub struct PerenaMarket {
    pub vault: Pubkey,
    pub share_mint: Pubkey,
    /// Feeds `get_nav` and is not otherwise checked against the vault.
    pub tranche_state: Pubkey,
}

/// Registered markets, keyed by `vault` (== Voltr strategy id).
pub const PERENA_MARKETS: &[PerenaMarket] = &[
    // USD*
    PerenaMarket {
        vault: pubkey!("ECJGrTZ6QYMEwiEAnL4oReWF126uc22e9Lojy9qyCjHT"),
        share_mint: pubkey!("star9agSpjiFe3M49B3RniVU4CMBBEK3Qnaqn3RGiFM"),
        tranche_state: pubkey!("FbsiAonbNrC3EKWMrZxx9iSc5Q7e7zSAkro4tD8dQQJS"),
    },
];

/// bankineco instruction discriminators (from its on-chain anchor IDL).
const EXECUTE_DEPOSIT_DISC: [u8; 8] = [247, 103, 46, 184, 88, 188, 56, 46];
const EXECUTE_WITHDRAW_DISC: [u8; 8] = [255, 93, 15, 141, 187, 94, 246, 162];
const GET_NAV_DISC: [u8; 8] = [200, 89, 76, 53, 215, 218, 63, 21];

/// Gross-up on withdrawals to cover the redemption fee / NAV spread (observed
/// ~8bps; deliberately larger, and clamped to the share balance).
const WITHDRAW_MARGIN_BPS: u64 = 30;

fn market_for(strategy: &Pubkey) -> Result<&'static PerenaMarket> {
    PERENA_MARKETS
        .iter()
        .find(|m| m.vault == *strategy)
        .ok_or_else(|| error!(AdaptorError::UnknownStrategy))
}

/// Bind the protocol-side accounts to the market registered for `strategy`.
fn validate_market(
    strategy: &Pubkey,
    perena_program: &Pubkey,
    perena_vault: &Pubkey,
    share_mint: &Pubkey,
) -> Result<&'static PerenaMarket> {
    require_keys_eq!(*perena_program, BANKINECO_PROGRAM_ID, AdaptorError::InvalidProtocolProgram);
    let market = market_for(strategy)?;
    require_keys_eq!(*perena_vault, market.vault, AdaptorError::InvalidProtocolVault);
    require_keys_eq!(*share_mint, market.share_mint, AdaptorError::InvalidShareMint);
    Ok(market)
}

// ------------------------------------------------------------------ accounts

#[derive(Accounts)]
pub struct PerenaInitialize<'info> {
    pub voltr: VoltrInitialize<'info>,

    // ---- remaining accounts ----
    /// CHECK: pinned to the bankineco program id.
    pub perena_program: UncheckedAccount<'info>,

    /// CHECK: pinned to the strategy's registered market.
    pub perena_vault: UncheckedAccount<'info>,

    pub share_mint: Box<InterfaceAccount<'info, Mint>>,

    /// CHECK: created idempotently as ATA(vault_strategy_auth, share_mint).
    #[account(mut)]
    pub vault_strategy_share_ata: UncheckedAccount<'info>,

    pub share_token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
}

/// Shared by deposit and withdraw. Remaining accounts mirror bankineco's
/// execute_deposit / execute_withdraw.
#[derive(Accounts)]
pub struct PerenaOperate<'info> {
    pub voltr: VoltrOperate<'info>,

    // ---- remaining accounts ----
    /// CHECK: pinned to the bankineco program id.
    pub perena_program: UncheckedAccount<'info>,

    /// CHECK: pinned to the strategy's registered market.
    #[account(mut)]
    pub perena_vault: UncheckedAccount<'info>,

    /// CHECK: validated by the bankineco program against the vault.
    pub perena_vault_oracle: UncheckedAccount<'info>,

    /// CHECK: pinned to the strategy's registered market.
    pub vault_tranche_state: UncheckedAccount<'info>,

    #[account(mut)]
    pub share_mint: Box<InterfaceAccount<'info, Mint>>,

    /// CHECK: Perena vault's asset ATA; PDA-validated by the bankineco program.
    #[account(mut)]
    pub perena_vault_asset_ata: UncheckedAccount<'info>,

    /// CHECK: validated by the bankineco program.
    #[account(mut)]
    pub fee_vault: UncheckedAccount<'info>,

    /// CHECK: PDA-validated by the bankineco program.
    #[account(mut)]
    pub fee_vault_ata: UncheckedAccount<'info>,

    /// The strategy authority's share ATA (bankineco's `user_share_ata`).
    #[account(mut)]
    pub vault_strategy_share_ata: Box<InterfaceAccount<'info, TokenAccount>>,

    pub share_token_program: Interface<'info, TokenInterface>,
}

impl<'info> PerenaOperate<'info> {
    fn validate(&self) -> Result<()> {
        let market = validate_market(
            self.voltr.strategy.key,
            self.perena_program.key,
            self.perena_vault.key,
            &self.share_mint.key(),
        )?;
        require_keys_eq!(
            self.vault_tranche_state.key(),
            market.tranche_state,
            AdaptorError::InvalidProtocolAccount
        );
        require_ata(
            &self.vault_strategy_share_ata.key(),
            self.voltr.vault_strategy_auth.key,
            &self.share_mint.key(),
            self.share_token_program.key,
        )
    }

    /// bankineco execute_deposit / execute_withdraw(amount). Account order per
    /// IDL: user, vault, vault_oracle, vault_tranche_state, asset_mint,
    /// share_mint, user_asset_ata, vault_asset_ata, fee_vault, fee_vault_ata,
    /// user_share_ata, asset_token_program, share_token_program.
    fn execute(&self, disc: [u8; 8], amount: u64) -> Result<()> {
        let v = &self.voltr;
        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&disc);
        data.extend_from_slice(&amount.to_le_bytes());
        let ix = Instruction {
            program_id: BANKINECO_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(v.vault_strategy_auth.key(), true),
                AccountMeta::new(self.perena_vault.key(), false),
                AccountMeta::new_readonly(self.perena_vault_oracle.key(), false),
                AccountMeta::new_readonly(self.vault_tranche_state.key(), false),
                AccountMeta::new_readonly(v.vault_asset_mint.key(), false),
                AccountMeta::new(self.share_mint.key(), false),
                AccountMeta::new(v.vault_strategy_asset_ata.key(), false),
                AccountMeta::new(self.perena_vault_asset_ata.key(), false),
                AccountMeta::new(self.fee_vault.key(), false),
                AccountMeta::new(self.fee_vault_ata.key(), false),
                AccountMeta::new(self.vault_strategy_share_ata.key(), false),
                AccountMeta::new_readonly(v.asset_token_program.key(), false),
                AccountMeta::new_readonly(self.share_token_program.key(), false),
            ],
            data,
        };
        invoke(
            &ix,
            &[
                v.vault_strategy_auth.to_account_info(),
                self.perena_vault.to_account_info(),
                self.perena_vault_oracle.to_account_info(),
                self.vault_tranche_state.to_account_info(),
                v.vault_asset_mint.to_account_info(),
                self.share_mint.to_account_info(),
                v.vault_strategy_asset_ata.to_account_info(),
                self.perena_vault_asset_ata.to_account_info(),
                self.fee_vault.to_account_info(),
                self.fee_vault_ata.to_account_info(),
                self.vault_strategy_share_ata.to_account_info(),
                v.asset_token_program.to_account_info(),
                self.share_token_program.to_account_info(),
                self.perena_program.to_account_info(),
            ],
        )?;
        Ok(())
    }

    /// Position value in USDC: bankineco get_nav on the strategy's share ATA.
    fn position_value(&self) -> Result<u64> {
        invoke(
            &Instruction {
                program_id: BANKINECO_PROGRAM_ID,
                accounts: vec![
                    AccountMeta::new_readonly(self.perena_vault.key(), false),
                    AccountMeta::new_readonly(self.vault_tranche_state.key(), false),
                    AccountMeta::new_readonly(self.share_mint.key(), false),
                    AccountMeta::new_readonly(self.vault_strategy_share_ata.key(), false),
                ],
                data: GET_NAV_DISC.to_vec(),
            },
            &[
                self.perena_vault.to_account_info(),
                self.vault_tranche_state.to_account_info(),
                self.share_mint.to_account_info(),
                self.vault_strategy_share_ata.to_account_info(),
                self.perena_program.to_account_info(),
            ],
        )?;
        read_u64_return_data(&BANKINECO_PROGRAM_ID)
    }
}

// ------------------------------------------------------------------ handlers

/// Creates the share ATA for the vault strategy authority (idempotent).
pub fn initialize(ctx: Context<PerenaInitialize>) -> Result<()> {
    let a = &ctx.accounts;
    validate_market(
        a.voltr.strategy.key,
        a.perena_program.key,
        a.perena_vault.key,
        &a.share_mint.key(),
    )?;
    associated_token::create_idempotent(CpiContext::new(
        a.associated_token_program.to_account_info(),
        associated_token::Create {
            payer: a.voltr.payer.to_account_info(),
            associated_token: a.vault_strategy_share_ata.to_account_info(),
            authority: a.voltr.vault_strategy_auth.to_account_info(),
            mint: a.share_mint.to_account_info(),
            system_program: a.voltr.system_program.to_account_info(),
            token_program: a.share_token_program.to_account_info(),
        },
    ))
}

/// Deposits the `amount` of USDC the vault just moved into the strategy ATA and
/// returns the position value. `amount == 0` only refreshes the position.
pub fn deposit(ctx: Context<PerenaOperate>, amount: u64) -> Result<u64> {
    let a = ctx.accounts;
    a.validate()?;
    if amount > 0 {
        let before = a.voltr.vault_strategy_asset_ata.amount;
        a.execute(EXECUTE_DEPOSIT_DISC, amount)?;
        settle_deposit(before, a.voltr.asset_balance()?, amount)?;
    }
    a.position_value()
}

/// Redeems enough shares that at least `amount` USDC lands in the strategy ATA
/// (the vault sweeps it afterwards) and returns the remaining position value.
pub fn withdraw(ctx: Context<PerenaOperate>, amount: u64) -> Result<u64> {
    let a = ctx.accounts;
    a.validate()?;
    if amount == 0 {
        return a.position_value();
    }
    let shares = shares_for_amount(
        amount,
        a.position_value()?,
        a.vault_strategy_share_ata.amount,
        WITHDRAW_MARGIN_BPS,
    )?;
    a.execute(EXECUTE_WITHDRAW_DISC, shares)?;
    settle_withdraw(a.voltr.asset_balance()?, amount)?;
    a.position_value()
}
