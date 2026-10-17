# Hubra USDC Adaptor

One Voltr adaptor program for every USDC protocol integration Hubra runs. Each
protocol is a module inside it with its own
`initialize_<p>` / `deposit_<p>` / `withdraw_<p>` instructions, and the manager
picks the protocol through the instruction discriminator it passes to the Voltr
vault's strategy CPIs.

- **Program ID:** `raRnoKWoRcon9dqxvAhtiPyQHgikpcu1g9tXeb9jPGT` (deployed 2026-10-01, slot 452238910; replaces the abandoned `FD7kwF7E…`)
- **Voltr access:** Voltr set our USDC vault's `allow_any_adaptor` policy (approved 2026-10-17), so adding a protocol here needs no new approval from them
- **Upgrade authority:** `RAoEnBNgWR3H4L76UFtXNoJShLQ3yFSUcGPCDEzqmMb` (the vault admin; moved off the manager hot key 2026-10-17). It must never be the bot's manager key: a malicious upgrade gets the strategy authority signer for every position and skips Voltr's add_adaptor cooldown.
- **Crate:** `hubra-usdc-adaptor` → `target/deploy/hubra_usdc_adaptor.so`

| Protocol | Module | Strategy id | Status |
|---|---|---|---|
| Perena USD* | `src/protocols/perena.rs` | Perena vault `ECJGrTZ6QYMEwiEAnL4oReWF126uc22e9Lojy9qyCjHT` | deployed |
| P0 (marginfi v2) USDC | `src/protocols/p0.rs` | marginfi bank `2s37akK2eyBbp8DZgCm7RtsaEz8eJP3Nxd4urLHQv7yB` | deployed |

Protocols Voltr already ships adaptors for (Kamino vaults, Jupiter Lend) keep
using Voltr's adaptors. This program is for the protocols they don't cover.

## Admission rule: instant deposit and withdraw only

A protocol can be integrated only if its deposits and withdrawals settle in
full **inside the same instruction**: no request/claim queues, cooldowns,
epoch unlocks or keeper-filled redemptions. The program enforces this for
every protocol on every call (`src/common.rs`):

- `settle_deposit`: the protocol must consume **exactly** `amount` from the
  strategy's USDC ATA.
- `settle_withdraw`: after redemption the strategy's USDC ATA must already
  hold `amount`, or the instruction fails with `WithdrawNotSettled`.

What Voltr does around these calls (verified 2026-10-17 by running the real
Voltr vault program in LiteSVM): after `withdraw_strategy` it sweeps the
**whole** strategy USDC ATA back to idle, not just `amount`, and its vault
`total_value` books whatever is left in that ATA. So a protocol that
over-delivers (Perena's 30 bps gross-up) loses nothing, and a manager who
swaps the deposit/withdraw discriminators can't move the vault's NAV. The test
suites' `vaultSweep` models this.

A protocol that only *sometimes* settles instantly (e.g. it has liquidity caps)
will fail these checks when it doesn't, and the transaction rolls back. The
funds stay safe, but the strategy can't exit until liquidity returns. Weigh
that before integrating.

### Evaluated and rejected

| Protocol | Date | Why not |
|---|---|---|
| Hylo eHYUSD (earn pool `HysTabVUfmQBFcmzu1ctRd1Y1fxd66RBpboy1bmtDSQQ`) | 2026-10-01 | Deposit is instant (USDC → hyUSD at 0 fee → eHYUSD), but exit is not. hyUSD → USDC redeems only from Hylo's USDC pair vault, which arbitrage keeps near 0 (0.0005 USDC on 2026-10-01; 9.5k max over the prior 7 days). The earn pool also caps total withdrawals at 1M hyUSD per epoch across all users. Every withdrawal would fail `WithdrawNotSettled`. Re-evaluate if Hylo adds a standing USDC redemption buffer. |

## Layout

```
src/lib.rs               program entry: thin per-protocol instruction wrappers
src/common.rs            Voltr account prefixes (VoltrInitialize / VoltrOperate),
                         settlement invariants, share math, return-data reader, ATA check
src/error.rs             one shared error enum (append-only)
src/protocols/<p>.rs     per-protocol: pinned program id, market registry,
                         account structs, CPI builders, handlers
tests/harness.js         LiteSVM harness shared by all suites
tests/run-all.js         runs every suite + cross-protocol isolation checks
tests/fetch-fixtures.js  refreshes every suite's forked mainnet state
tests/protocols/<p>/     suite.js, fixtures.json, dumped protocol program(s)
```

Voltr calling convention (fixed by the vault program):

| CPI | Accounts | Returns |
|---|---|---|
| `initialize_<p>` | `[payer, vault_strategy_auth, strategy, system_program, ..remaining]` | — |
| `deposit_<p>(amount, Option<Vec<u8>>)` | `[vault_strategy_auth, strategy, vault_asset_mint, vault_strategy_asset_ata, asset_token_program, ..remaining]` | position value (u64 LE) |
| `withdraw_<p>(amount, Option<Vec<u8>>)` | same as deposit | remaining position value |

`amount == 0` is a pure position refresh for both deposit and withdraw.

### Security model (applies to every protocol)

- **Position value always comes from the protocol's own on-chain read** (a view
  instruction or the protocol's exchange-rate state), never from computed or
  manager-supplied numbers.
- **Each protocol pins its program id and keeps a market registry** keyed by
  Voltr strategy id. Every protocol-side account that the protocol itself does
  not validate (share mint, NAV inputs such as Perena's tranche state) is pinned
  in the registry entry. This blocks the "lookalike vault" attack, where a
  compromised manager passes a self-consistent but empty vault/mint set and
  reports a false NAV.
- **Isolation:** a strategy id belongs to exactly one protocol's registry, so a
  call into protocol A with protocol B's strategy fails `UnknownStrategy`.
  Voltr derives `vault_strategy_auth` per (vault, strategy), so positions never
  share token accounts.
- Checked math throughout; `overflow-checks = true` in release.

## Adding a protocol

1. **Confirm instant settlement** (see the admission rule above), and that the
   protocol exposes a trustworthy on-chain position value.
2. Create `src/protocols/<p>.rs` modelled on `perena.rs`:
   - `const <P>_PROGRAM_ID`, plus a `<P>_MARKETS` registry with every account
     the protocol doesn't validate itself pinned per market.
   - `<P>Initialize { voltr: VoltrInitialize, ..remaining }` and
     `<P>Operate { voltr: VoltrOperate, ..remaining }`.
   - Handlers `initialize` / `deposit` / `withdraw` that call
     `settle_deposit` / `settle_withdraw`, and `shares_for_amount` if the
     protocol is share-based.
   - Reuse `AdaptorError` variants. Only append new ones if nothing fits.
3. Add `pub mod <p>;` in `protocols/mod.rs`, `pub use protocols::<p>::*;` and
   the three wrapper instructions in `lib.rs`.
4. Add `tests/protocols/<p>/suite.js` (same exports as Perena's: `name`,
   `programs`, `fixtures`, `strategies`, `initializeForeign`, `refreshIxs`,
   `cases`) with at least: the full lifecycle, refresh, `WithdrawNotSettled`,
   and a substitution test for every pinned account. Seed `fixtures.json` with
   the account addresses, then run `npm run fixtures -- <p>`.
5. `cargo test --lib && cargo-build-sbf && npm test`. **Every suite must be
   green, not just the new one.** The runner loads all protocols into every
   test's SVM and automatically adds cross-protocol isolation checks for the
   new strategy ids.
6. Add the client side in the rebalancer (`vaults-rebalancer/src/lib/<p>.ts`)
   using the `initialize_<p>` / `deposit_<p>` / `withdraw_<p>` discriminators.

## Build & test

```bash
npm install          # litesvm 1.x + @solana/kit (dev only)
cargo test --lib     # unit tests: share math, settlement invariants
cargo-build-sbf      # agave 4.x platform tools
npm test             # all protocol suites + isolation checks (LiteSVM, forked mainnet state)
npm run fixtures     # fetch forked state + program dumps (uses solana CLI RPC or $RPC_URL)
                     # required once after cloning: protocol .so dumps are not committed
```

The harness pins the SVM clock to the fixtures' fetch time, so tests are
reproducible until the protocol program itself is upgraded on mainnet. When
that happens, re-run `npm run fixtures` and re-test before redeploying.

Compute usage (Perena, 2026-10-01): initialize 24k, deposit 70k, refresh 21k,
withdraw 80k CU. P0 (2026-10-17): deposit 65k, refresh 34k, withdraw 82k CU.

## Deployment

The upgrade authority is `RAoEnBNgWR3H4L76UFtXNoJShLQ3yFSUcGPCDEzqmMb` (vault admin), not the bot's manager key.

1. Voltr enables the vault's `allow_any_adaptor` policy (checked in the vault
   program's `add_adaptor`). This is per vault: a new Hubra vault needs its
   own approval from Voltr.
2. Upgrade (the program is already deployed; the upgrade authority must sign,
   any funded wallet can pay):
   ```bash
   solana program deploy target/deploy/hubra_usdc_adaptor.so \
     --program-id raRnoKWoRcon9dqxvAhtiPyQHgikpcu1g9tXeb9jPGT \
     --upgrade-authority <upgrade-authority-keypair.json> \
     --keypair <payer-keypair.json> --fee-payer <payer-keypair.json>
   ```
   `deploy` extends the program account automatically when a new build is
   bigger (the payer covers the extra rent), and temporarily holds the full
   build's rent in a buffer that is refunded on completion. The P0 upgrade
   (2026-10-17, 260,968 → 311,080 bytes) cost ~0.26 SOL net.
3. Per protocol: `add_adaptor` once per vault (it's the same program for all
   protocols), then `initialize_strategy` with the `initialize_<p>`
   discriminator: `vaults-rebalancer/scripts/setup-hubra-adaptor.ts` does
   both, and creates the strategy authority's USDC ATA (Voltr doesn't; the
   first deposit fails without it).
   **Cooldown:** Voltr stamps the epoch into the adaptor add receipt (byte 88)
   and rejects `initialize_strategy` / `deposit_strategy` with 6010
   `AdaptorEpochInvalid` until `current_epoch >= added_epoch + 2` (~2 days).
   Program upgrades don't reset it.

**Full exit (Perena):** request at most
`nav * (1 - PERENA_FULL_EXIT_HAIRCUT_BPS/10000)`. Redemption fees leave less
than the full NAV, so asking for all of it fails `WithdrawNotSettled`.

## Perena key addresses

| | |
|---|---|
| bankineco program | `save8RQVPMWNTzU18t3GBvBkN9hT7jsGjiCQ28FpD9H` |
| USD* mint | `star9agSpjiFe3M49B3RniVU4CMBBEK3Qnaqn3RGiFM` |
| Perena USD* vault | `ECJGrTZ6QYMEwiEAnL4oReWF126uc22e9Lojy9qyCjHT` |
| Vault oracle | `2YH9QB7qmEBgekeXH4C36L2dwMPfLdoPi5DDxeTi1kyP` |
| Tranche state | `FbsiAonbNrC3EKWMrZxx9iSc5Q7e7zSAkro4tD8dQQJS` |
| Fee vault / ATA | `EAf7PeuCxhMN9gEhE1Gagmj28krmu3wEqCLMbqpSTVqg` / `CSYjpGjrZZVUDshaCcMZYNB7JNnVHFwNFgzbUeFScSXj` |
| Perena vault USDC ATA | `EgewZEt58TFvMBKKXpfoCT2Vg7JBUAtXXLdzcgmMvh5U` |

Perena fees (mainnet): deposit ~1.6bps, withdraw ~8bps redemption spread;
the adaptor grosses withdrawals up by 30bps and clamps to the share balance.

**Full exit (P0):** request exactly the current position value
(`getP0PositionValue` in the rebalancer). The adaptor withdraws the exact
amount and marginfi rejects anything above the position. Share rounding can
leave 1 base unit, which stays counted and clears on the next exit.

**Closed marginfi account (P0):** marginfi's `admin_close_account` is
permissionless for an account that is empty (all balances < 1 share) and has
had no deposit/withdraw for 60 days; the rent goes to P0's fee wallet. The
adaptor treats a closed account as value 0 (refresh), fails withdraws with
`NoPosition`, and the next deposit re-opens it via `init_pda`, paying the rent
(~0.0124 SOL) **from the strategy authority PDA**
(`9LYtJgCm4DbbJQfaWz2K8gqfFqPWmjnXjHgZbJEvbBw7` for the Hubra USDC vault). Keep
that PDA funded: `setup-hubra-adaptor.ts --send` tops it up to 0.05 SOL. If it
is empty, the re-opening deposit fails cleanly and funds stay idle.

## P0 (marginfi v2) key addresses

| | |
|---|---|
| marginfi program | `MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA` |
| Group | `4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG8` |
| USDC bank (strategy id) | `2s37akK2eyBbp8DZgCm7RtsaEz8eJP3Nxd4urLHQv7yB` |
| Bank liquidity vault | `7jaiZR5Sk8hdYN9MxTpczTcwbWpb5WEoxSANuUwveuat` |
| Bank oracle (Pyth push) | `6HAuqASbHEh4w4REJEUUUCginTLfj1kwCh215ZLtMkrT` |

The strategy authority owns marginfi account
`PDA("marginfi_account", group, authority, 0u16, 0u16)` (no third-party id).
Withdrawals settle instantly only up to the bank's free liquidity (273.9k USDC
at 96.6% utilization on 2026-10-17); beyond that marginfi rejects them and
funds stay put. Withdraws still work with a stale oracle, because the account
never borrows.

