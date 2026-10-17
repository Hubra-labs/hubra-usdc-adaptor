// P0 (marginfi v2) USDC bank suite: lifecycle against forked mainnet state,
// liquidity/position limits, and account substitution attacks.
const path = require("path");
const { Clock } = require("litesvm");
const {
  B, USDC, meta, randomAddress, setTokenAccount, tokenBalance,
  voltrStrategy, initializeIx, operateIx, send, assert, expectOk, expectAdaptorError,
} = require("../../harness");
const { getAddressEncoder, getAddressDecoder, getProgramDerivedAddress, lamports } = require("@solana/kit");
const crypto = require("crypto");

const MARGINFI = B("MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA");
const INSTRUCTIONS_SYSVAR = B("Sysvar1nstructions1111111111111111111111111");
const SYSTEM = B("11111111111111111111111111111111");
const M = {
  group: B("4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG8"),
  bank: B("2s37akK2eyBbp8DZgCm7RtsaEz8eJP3Nxd4urLHQv7yB"),
  liquidityVault: B("7jaiZR5Sk8hdYN9MxTpczTcwbWpb5WEoxSANuUwveuat"),
  oracle: B("6HAuqASbHEh4w4REJEUUUCginTLfj1kwCh215ZLtMkrT"),
  otherBank: B("EwURhaBfL9ahMqPHrJYpjaNsbXBWmwDxyLNXeudncF8N"), // another USDC bank in the same group
};
const USDC_1 = 1_000_000n;
const enc = getAddressEncoder();

const u16le = (n) => { const b = Buffer.alloc(2); b.writeUInt16LE(n); return b; };
const marginfiAccount = async (authority, group = M.group) =>
  (await getProgramDerivedAddress({
    programAddress: MARGINFI,
    seeds: [Buffer.from("marginfi_account"), enc.encode(group), enc.encode(authority), u16le(0), u16le(0)],
  }))[0];
const liquidityVaultAuthority = async (bank) =>
  (await getProgramDerivedAddress({ programAddress: MARGINFI, seeds: [Buffer.from("liquidity_vault_auth"), enc.encode(bank)] }))[0];

async function initRemaining(voltr, o = {}) {
  return [
    meta(o.program ?? MARGINFI, false), meta(o.group ?? M.group, false), meta(o.bank ?? M.bank, false),
    meta(o.account ?? await marginfiAccount(voltr.auth.address), true), meta(INSTRUCTIONS_SYSVAR, false),
  ];
}

async function operateRemaining(voltr, o = {}) {
  const bank = o.bank ?? M.bank;
  return [
    meta(o.program ?? MARGINFI, false), meta(o.group ?? M.group, false),
    meta(o.account ?? await marginfiAccount(voltr.auth.address), true), meta(bank, true),
    meta(M.liquidityVault, true), meta(await liquidityVaultAuthority(bank), false), meta(M.oracle, false),
    meta(INSTRUCTIONS_SYSVAR, false), meta(SYSTEM, false),
  ];
}

const deposit = async (voltr, amount, o = {}) =>
  operateIx("deposit", "p0", voltr, o.strategy ?? M.bank, amount, await operateRemaining(voltr, o));
const withdraw = async (voltr, amount, o = {}) =>
  operateIx("withdraw", "p0", voltr, o.strategy ?? M.bank, amount, await operateRemaining(voltr, o));
const initialize = async (voltr, o = {}) =>
  initializeIx("p0", voltr, o.strategy ?? M.bank, await initRemaining(voltr, o));

/** Strategy authority with `usdc` in its ATA and an initialized marginfi account. */
async function initialized(svm, usdc) {
  const voltr = await voltrStrategy(svm, usdc);
  expectOk(await send(svm, voltr, [await initialize(voltr)]), "initialize");
  return voltr;
}

/** What the Voltr vault does right after a successful withdraw: sweep the whole strategy ATA to idle. */
function vaultSweep(svm, voltr) {
  setTokenAccount(svm, voltr.usdcAta, USDC, voltr.auth.address, 0);
}

/** marginfi's permissionless admin_close_account (empty account, no activity for > 60 days). */
async function adminCloseIx(svm, account) {
  const group = Buffer.from(svm.getAccount(M.group).data);
  const globalFeeWallet = getAddressDecoder().decode(group.subarray(48, 80)); // fee_state_cache.global_fee_wallet
  const pool = (await getProgramDerivedAddress({ programAddress: MARGINFI, seeds: [Buffer.from("rebalance_fee_pool"), enc.encode(account)] }))[0];
  return {
    programAddress: MARGINFI,
    accounts: [meta(M.group, false), meta(account, true), meta(globalFeeWallet, true), meta(pool, false)],
    data: new Uint8Array(crypto.createHash("sha256").update("global:admin_close_account").digest().subarray(0, 8)),
  };
}

/** Deposit, fully exit, then let a third party close the empty account after 61 idle days. */
async function exitedAndClosed(svm) {
  const voltr = await initialized(svm, 100n * USDC_1);
  const [v] = expectOk(await send(svm, voltr, [await deposit(voltr, 100n * USDC_1)]), "deposit").returns;
  expectOk(await send(svm, voltr, [await withdraw(voltr, v)]), "full exit");
  vaultSweep(svm, voltr);
  const account = await marginfiAccount(voltr.auth.address);
  const stranger = await voltrStrategy(svm, 0);
  assert(!(await send(svm, stranger, [await adminCloseIx(svm, account)])).ok, "close must fail while recently active");
  advanceClock(svm, 61 * 86_400);
  expectOk(await send(svm, stranger, [await adminCloseIx(svm, account)]), "admin_close after 61 days");
  assert(!svm.getAccount(account).exists, "marginfi account should be closed");
  return { voltr, account };
}

function advanceClock(svm, seconds) {
  const c = svm.getClock();
  const ts = c.unixTimestamp + BigInt(seconds);
  svm.setClock(new Clock(c.slot + BigInt(seconds) * 2n, ts, c.epoch, c.leaderScheduleEpoch, ts));
}

const cases = [
  {
    name: "lifecycle: init → deposit → refresh → partial withdraw → full exit",
    async run(svm) {
      const voltr = await initialized(svm, 500n * USDC_1);

      const [valDep] = expectOk(await send(svm, voltr, [await deposit(voltr, 500n * USDC_1)]), "deposit 500").returns;
      assert(tokenBalance(svm, voltr.usdcAta) === 0n, "deposit must consume the full 500 USDC");
      assert(valDep >= 500n * USDC_1 - 1n && valDep <= 500n * USDC_1, `deposit value ${valDep} not in [500 - 1 unit, 500]`);

      const [valRefD] = expectOk(await send(svm, voltr, [await deposit(voltr, 0)]), "deposit 0").returns;
      const [valRefW] = expectOk(await send(svm, voltr, [await withdraw(voltr, 0)]), "withdraw 0").returns;
      assert(valRefD === valDep && valRefW === valDep, "refresh must not move funds or value");

      const [valW] = expectOk(await send(svm, voltr, [await withdraw(voltr, 200n * USDC_1)]), "withdraw 200").returns;
      assert(tokenBalance(svm, voltr.usdcAta) === 200n * USDC_1, "withdraw must deliver exactly 200 USDC");
      const drop = valDep - valW;
      assert(drop >= 200n * USDC_1 && drop <= 200n * USDC_1 + 1n, `withdraw 200 moved value by ${drop}`);
      vaultSweep(svm, voltr);

      // Full exit as the rebalancer requests it: the current position value.
      const [valX] = expectOk(await send(svm, voltr, [await withdraw(voltr, valW)]), "full exit").returns;
      assert(valX === 0n, `full exit left value ${valX}`);
      assert(tokenBalance(svm, voltr.usdcAta) === valW, "full exit must deliver the full position value");
    },
  },
  {
    name: "refresh accrues interest (value grows over a day, no oracle needed)",
    async run(svm) {
      const voltr = await initialized(svm, 10_000n * USDC_1);
      const [v0] = expectOk(await send(svm, voltr, [await deposit(voltr, 10_000n * USDC_1)]), "deposit").returns;
      advanceClock(svm, 86_400);
      const [v1] = expectOk(await send(svm, voltr, [await deposit(voltr, 0)]), "refresh").returns;
      assert(v1 > v0, `value did not grow after a day: ${v0} -> ${v1}`);
      // ~2-20% APY on 10k over a day is ~0.5-5.5 USDC.
      const gain = v1 - v0;
      assert(gain > 100_000n && gain < 10n * USDC_1, `implausible daily interest ${gain}`);
    },
  },
  {
    name: "initialize is idempotent",
    async run(svm) {
      const voltr = await initialized(svm, 0n);
      expectOk(await send(svm, voltr, [await initialize(voltr)]), "re-initialize");
    },
  },
  {
    name: "refresh before any deposit reports zero",
    async run(svm) {
      const voltr = await initialized(svm, 0n);
      const [v] = expectOk(await send(svm, voltr, [await deposit(voltr, 0)]), "refresh").returns;
      assert(v === 0n, `expected 0, got ${v}`);
    },
  },
  {
    name: "withdraw above the position fails, funds stay put",
    async run(svm) {
      const voltr = await initialized(svm, 100n * USDC_1);
      const [v] = expectOk(await send(svm, voltr, [await deposit(voltr, 100n * USDC_1)]), "deposit").returns;
      const res = await send(svm, voltr, [await withdraw(voltr, v + 1n * USDC_1)]);
      assert(!res.ok, "withdrawing more than the position must fail");
      const [after] = expectOk(await send(svm, voltr, [await withdraw(voltr, 0)]), "refresh").returns;
      assert(after === v, "failed withdraw must not change the position");
    },
  },
  {
    name: "withdraw above the bank's free liquidity fails, funds stay put",
    async run(svm) {
      const voltr = await initialized(svm, 1_000n * USDC_1);
      const [v] = expectOk(await send(svm, voltr, [await deposit(voltr, 1_000n * USDC_1)]), "deposit").returns;
      // Drain the bank's vault down to 10 USDC, as if borrowers took the rest.
      setTokenAccount(svm, M.liquidityVault, USDC, await liquidityVaultAuthority(M.bank), 10n * USDC_1);
      const res = await send(svm, voltr, [await withdraw(voltr, 500n * USDC_1)]);
      assert(!res.ok, "withdrawing beyond free liquidity must fail");
      const [after] = expectOk(await send(svm, voltr, [await withdraw(voltr, 0)]), "refresh").returns;
      assert(after === v, "failed withdraw must not change the position");
    },
  },
  {
    name: "closed account (permissionless admin_close): refresh reports 0, withdraw fails, deposit re-opens",
    async run(svm) {
      const { voltr, account } = await exitedAndClosed(svm);
      const [vd] = expectOk(await send(svm, voltr, [await deposit(voltr, 0)]), "refresh (deposit 0)").returns;
      const [vw] = expectOk(await send(svm, voltr, [await withdraw(voltr, 0)]), "refresh (withdraw 0)").returns;
      assert(vd === 0n && vw === 0n, `closed account must report 0, got ${vd}/${vw}`);
      expectAdaptorError(await send(svm, voltr, [await withdraw(voltr, 1n * USDC_1)]), "NoPosition", "withdraw from closed account");

      setTokenAccount(svm, voltr.usdcAta, USDC, voltr.auth.address, 50n * USDC_1);
      const res = expectOk(await send(svm, voltr, [await deposit(voltr, 50n * USDC_1)]), "deposit re-opens");
      assert(res.logs.some((l) => l.includes("re-opening closed marginfi account")), "deposit should log the re-open");
      assert(svm.getAccount(account).exists, "marginfi account should exist again");
      assert(res.returns[0] >= 50n * USDC_1 - 1n, `re-opened position ${res.returns[0]} < 50 USDC`);
      const [after] = expectOk(await send(svm, voltr, [await withdraw(voltr, 20n * USDC_1)]), "withdraw after re-open").returns;
      assert(after >= 30n * USDC_1 - 1n && after <= 30n * USDC_1, `value after withdraw ${after}`);
    },
  },
  {
    name: "re-open still works if someone pre-funds the closed account's address",
    async run(svm) {
      const { voltr, account } = await exitedAndClosed(svm);
      svm.airdrop(account, lamports(1_000_000n)); // griefing attempt: make it a funded system account
      setTokenAccount(svm, voltr.usdcAta, USDC, voltr.auth.address, 10n * USDC_1);
      const [v] = expectOk(await send(svm, voltr, [await deposit(voltr, 10n * USDC_1)]), "deposit re-opens").returns;
      assert(v >= 10n * USDC_1 - 1n, `re-opened position ${v}`);
    },
  },
  {
    name: "rejects wrong protocol program",
    async run(svm) {
      const voltr = await initialized(svm, 0n);
      const program = randomAddress();
      expectAdaptorError(await send(svm, voltr, [await deposit(voltr, 0, { program })]), "InvalidProtocolProgram", "deposit");
      expectAdaptorError(await send(svm, voltr, [await withdraw(voltr, 0, { program })]), "InvalidProtocolProgram", "withdraw");
      expectAdaptorError(await send(svm, voltr, [await initialize(voltr, { program })]), "InvalidProtocolProgram", "initialize");
    },
  },
  {
    name: "rejects a bank or group other than the strategy's market",
    async run(svm) {
      const voltr = await initialized(svm, 0n);
      for (const [name, op] of [["deposit", deposit], ["withdraw", withdraw], ["initialize", initialize]]) {
        const a = (o) => (name === "initialize" ? op(voltr, o) : op(voltr, 0, o));
        expectAdaptorError(await send(svm, voltr, [await a({ bank: M.otherBank })]), "InvalidProtocolVault", `${name} other bank`);
        expectAdaptorError(await send(svm, voltr, [await a({ strategy: M.otherBank, bank: M.otherBank })]), "UnknownStrategy", `${name} other bank as strategy`);
        expectAdaptorError(await send(svm, voltr, [await a({ group: randomAddress() })]), "InvalidProtocolAccount", `${name} other group`);
      }
    },
  },
  {
    name: "rejects a marginfi account that isn't the strategy authority's PDA",
    async run(svm) {
      const voltr = await initialized(svm, 100n * USDC_1);
      expectOk(await send(svm, voltr, [await deposit(voltr, 100n * USDC_1)]), "deposit");
      // Someone else's real marginfi account, e.g. to misreport value.
      const other = await initialized(svm, 0n);
      const foreign = await marginfiAccount(other.auth.address);
      for (const [name, op] of [["deposit", deposit], ["withdraw", withdraw]]) {
        expectAdaptorError(await send(svm, voltr, [await op(voltr, 0, { account: foreign })]), "InvalidProtocolAccount", `${name} foreign account`);
        expectAdaptorError(await send(svm, voltr, [await op(voltr, 0, { account: randomAddress() })]), "InvalidProtocolAccount", `${name} random account`);
      }
      expectAdaptorError(await send(svm, voltr, [await initialize(voltr, { account: randomAddress() })]), "InvalidProtocolAccount", "initialize random account");
    },
  },
];

module.exports = {
  name: "p0",
  programs: [{ id: MARGINFI, file: path.join(__dirname, "marginfi.so") }],
  fixtures: path.join(__dirname, "fixtures.json"),
  strategies: [M.bank],
  // Hooks for the cross-protocol checks in run-all.js.
  initializeForeign: (svm) => initialized(svm, 0n),
  refreshIxs: async (voltr, strategy) => [await deposit(voltr, 0, { strategy }), await withdraw(voltr, 0, { strategy })],
  cases,
};
