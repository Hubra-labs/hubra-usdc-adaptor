//! Hubra unified Voltr adaptor for USDC integrations.
//!
//! One program id, whitelisted once by Voltr, routing vault USDC into any
//! number of protocols. Each protocol lives in `protocols/<name>.rs` and
//! exposes its own `initialize_<name>` / `deposit_<name>` / `withdraw_<name>`
//! instructions; the manager selects the protocol through the instruction
//! discriminator it passes to the Voltr vault's strategy CPIs.
//!
//! Admission rule: only protocols with **instant, same-instruction** deposit
//! and withdrawal are supported. This is enforced generically on every call
//! (see `common::settle_deposit` / `common::settle_withdraw`): a deposit must
//! consume exactly `amount` and a withdrawal must leave at least `amount` in
//! the strategy's asset ATA before returning, or the instruction fails.
//!
//! Isolation between protocols:
//! - Every protocol pins its program id and keeps a registry of allowed
//!   markets keyed by the Voltr strategy id; calling one protocol's
//!   instruction with another protocol's strategy fails `UnknownStrategy`.
//! - The Voltr strategy authority PDA is derived per (vault, strategy), so
//!   positions in different protocols never share token accounts.
//!
//! Calling convention (fixed by the Voltr vault program, verified on-chain):
//!   initialize: [payer(w,s), vault_strategy_auth(w,s), strategy, system_program, ..remaining]
//!   deposit:    [vault_strategy_auth(w,s), strategy, vault_asset_mint, vault_strategy_asset_ata(w), asset_token_program, ..remaining]
//!   withdraw:   same prefix as deposit
//! Instruction data: discriminator ++ borsh(amount: u64, additional_args: Option<Vec<u8>>).
//! deposit/withdraw return the strategy's position value in USDC base units
//! as u64 LE return data; `amount == 0` is a pure position refresh.

use anchor_lang::prelude::*;

pub mod common;
pub mod error;
pub mod protocols;

pub use common::*;
// Globs bring each protocol's Accounts structs (and anchor's generated client
// modules) to the crate root for #[program]. Every protocol also has handlers
// named initialize/deposit/withdraw; those are always called fully qualified.
#[allow(ambiguous_glob_reexports)]
pub use protocols::p0::*;
#[allow(ambiguous_glob_reexports)]
pub use protocols::perena::*;

declare_id!("raRnoKWoRcon9dqxvAhtiPyQHgikpcu1g9tXeb9jPGT");

#[program]
pub mod hubra_usdc_adaptor {
    use super::*;

    // ---------------------------------------------------------------- Perena USD*

    pub fn initialize_perena(ctx: Context<PerenaInitialize>) -> Result<()> {
        protocols::perena::initialize(ctx)
    }

    pub fn deposit_perena(
        ctx: Context<PerenaOperate>,
        amount: u64,
        _additional_args: Option<Vec<u8>>,
    ) -> Result<u64> {
        protocols::perena::deposit(ctx, amount)
    }

    pub fn withdraw_perena(
        ctx: Context<PerenaOperate>,
        amount: u64,
        _additional_args: Option<Vec<u8>>,
    ) -> Result<u64> {
        protocols::perena::withdraw(ctx, amount)
    }

    // ---------------------------------------------------------------- P0 (marginfi v2)

    pub fn initialize_p0(ctx: Context<P0Initialize>) -> Result<()> {
        protocols::p0::initialize(ctx)
    }

    pub fn deposit_p0(
        ctx: Context<P0Operate>,
        amount: u64,
        _additional_args: Option<Vec<u8>>,
    ) -> Result<u64> {
        protocols::p0::deposit(ctx, amount)
    }

    pub fn withdraw_p0(
        ctx: Context<P0Operate>,
        amount: u64,
        _additional_args: Option<Vec<u8>>,
    ) -> Result<u64> {
        protocols::p0::withdraw(ctx, amount)
    }
}
