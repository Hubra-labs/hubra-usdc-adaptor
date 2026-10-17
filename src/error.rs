use anchor_lang::prelude::*;

/// Errors shared by every protocol integration. Keep variants generic so a new
/// protocol reuses them instead of growing a per-protocol error list; append
/// only (error codes are positional and clients match on them).
#[error_code]
pub enum AdaptorError {
    #[msg("Protocol program id does not match the pinned program.")]
    InvalidProtocolProgram,
    #[msg("Strategy is not a registered market of this protocol.")]
    UnknownStrategy,
    #[msg("Protocol vault account does not match the strategy's market.")]
    InvalidProtocolVault,
    #[msg("Share mint does not match the strategy's market.")]
    InvalidShareMint,
    #[msg("Auxiliary protocol account does not match the strategy's market.")]
    InvalidProtocolAccount,
    #[msg("Share token account is not the strategy authority's ATA.")]
    InvalidShareAta,
    #[msg("Protocol returned no position value.")]
    MissingReturnData,
    #[msg("No position to withdraw from.")]
    NoPosition,
    #[msg("Deposit did not consume exactly the requested amount.")]
    DepositNotSettled,
    #[msg("Withdrawal did not deliver the requested amount in the same instruction.")]
    WithdrawNotSettled,
    #[msg("Math overflow.")]
    MathOverflow,
}
