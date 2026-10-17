// Perena USD* suite: lifecycle against forked mainnet state + account
// substitution attacks.
const path = require("path");
const {
  B, USDC, TOKEN, ATA_PROG, meta, ata, randomAddress, setRawAccount, setTokenAccount, tokenBalance,
  voltrStrategy, initializeIx, operateIx, send, assert, expectOk, expectAdaptorError,
} = require("../../harness");
const { getAddressEncoder } = require("@solana/kit");

const BANKINECO = B("save8RQVPMWNTzU18t3GBvBkN9hT7jsGjiCQ28FpD9H");
const USDSTAR = B("star9agSpjiFe3M49B3RniVU4CMBBEK3Qnaqn3RGiFM");
const P = {
  vault: B("ECJGrTZ6QYMEwiEAnL4oReWF126uc22e9Lojy9qyCjHT"),
  oracle: B("2YH9QB7qmEBgekeXH4C36L2dwMPfLdoPi5DDxeTi1kyP"),
  tranche: B("FbsiAonbNrC3EKWMrZxx9iSc5Q7e7zSAkro4tD8dQQJS"),
  feeVault: B("EAf7PeuCxhMN9gEhE1Gagmj28krmu3wEqCLMbqpSTVqg"),
  feeVaultAta: B("CSYjpGjrZZVUDshaCcMZYNB7JNnVHFwNFgzbUeFScSXj"),
  vaultAta: B("EgewZEt58TFvMBKKXpfoCT2Vg7JBUAtXXLdzcgmMvh5U"),
};
const USDC_1 = 1_000_000n;

async function initRemaining(voltr, o = {}) {
  const mint = o.mint ?? USDSTAR;
  return [
    meta(o.program ?? BANKINECO, false), meta(o.vault ?? P.vault, false), meta(mint, false),
    meta(o.shareAta ?? await ata(voltr.auth.address, mint), true), meta(TOKEN, false), meta(ATA_PROG, false),
  ];
}

async function operateRemaining(voltr, o = {}) {
  const mint = o.mint ?? USDSTAR;
  return [
    meta(o.program ?? BANKINECO, false), meta(o.vault ?? P.vault, true), meta(P.oracle, false),
    meta(o.tranche ?? P.tranche, false), meta(mint, true), meta(P.vaultAta, true),
    meta(P.feeVault, true), meta(P.feeVaultAta, true),
    meta(o.shareAta ?? await ata(voltr.auth.address, mint), true), meta(TOKEN, false),
  ];
}

const deposit = async (voltr, amount, o = {}) =>
  operateIx("deposit", "perena", voltr, o.strategy ?? P.vault, amount, await operateRemaining(voltr, o));
const withdraw = async (voltr, amount, o = {}) =>
  operateIx("withdraw", "perena", voltr, o.strategy ?? P.vault, amount, await operateRemaining(voltr, o));
const initialize = async (voltr, o = {}) =>
  initializeIx("perena", voltr, o.strategy ?? P.vault, await initRemaining(voltr, o));

/** Strategy authority with `usdc` in its ATA and an initialized USD* ATA. */
async function initialized(svm, usdc) {
  const voltr = await voltrStrategy(svm, usdc);
  expectOk(await send(svm, voltr, [await initialize(voltr)]), "initialize");
  return voltr;
}

/** What the Voltr vault does right after a successful withdraw: sweep `amount` back to idle. */
function vaultSweep(svm, voltr, amount) {
  setTokenAccount(svm, voltr.usdcAta, USDC, voltr.auth.address, tokenBalance(svm, voltr.usdcAta) - BigInt(amount));
}

/** A self-consistent lookalike bankineco vault + share mint + ATA (the pre-pinning attack set). */
async function forgeLookalike(svm, voltr) {
  const vault = randomAddress();
  setRawAccount(svm, vault, { lamports: 10_000_000, data: Buffer.alloc(10016), owner: BANKINECO });
  const mint = randomAddress();
  const m = Buffer.alloc(82);
  m.writeUInt32LE(1, 0); Buffer.from(getAddressEncoder().encode(vault)).copy(m, 4); m.writeUInt8(6, 44); m.writeUInt8(1, 45);
  setRawAccount(svm, mint, { lamports: 2_000_000, data: m, owner: TOKEN });
  const shareAta = await ata(voltr.auth.address, mint);
  setTokenAccount(svm, shareAta, mint, voltr.auth.address, 0);
  const tranche = randomAddress();
  setRawAccount(svm, tranche, { lamports: 2_000_000, data: Buffer.alloc(816), owner: BANKINECO });
  return { vault, mint, shareAta, tranche };
}

const cases = [
  {
    name: "lifecycle: init → deposit → refresh → partial withdraw → full exit",
    async run(svm) {
      const voltr = await initialized(svm, 500n * USDC_1);
      const shareAta = await ata(voltr.auth.address, USDSTAR);

      const [navDep] = expectOk(await send(svm, voltr, [await deposit(voltr, 500n * USDC_1)]), "deposit 500").returns;
      assert(tokenBalance(svm, voltr.usdcAta) === 0n, "deposit must consume the full 500 USDC");
      assert(navDep > 499n * USDC_1 && navDep <= 500n * USDC_1, `deposit nav ${navDep} not in (499, 500]`);

      const [navRefD] = expectOk(await send(svm, voltr, [await deposit(voltr, 0)]), "deposit 0").returns;
      const [navRefW] = expectOk(await send(svm, voltr, [await withdraw(voltr, 0)]), "withdraw 0").returns;
      assert(navRefD === navDep && navRefW === navDep, "refresh must not move funds or nav");

      const [navW] = expectOk(await send(svm, voltr, [await withdraw(voltr, 200n * USDC_1)]), "withdraw 200").returns;
      assert(tokenBalance(svm, voltr.usdcAta) >= 200n * USDC_1, "withdraw must deliver >= 200 USDC");
      const drop = navDep - navW;
      assert(drop >= 199n * USDC_1 && drop <= 202n * USDC_1, `withdraw 200 moved nav by ${drop}`);
      vaultSweep(svm, voltr, 200n * USDC_1);

      // Full exit as the rebalancer requests it: nav minus the 15bps haircut.
      const exitAmount = (navW * 9985n) / 10000n;
      const [navX] = expectOk(await send(svm, voltr, [await withdraw(voltr, exitAmount)]), "full exit").returns;
      assert(navX === 0n && tokenBalance(svm, shareAta) === 0n, "full exit must burn all shares");
      assert(tokenBalance(svm, voltr.usdcAta) >= exitAmount, "full exit must deliver the requested amount");
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
    name: "withdraw that redemption can't cover fails WithdrawNotSettled",
    async run(svm) {
      const voltr = await initialized(svm, 500n * USDC_1);
      expectOk(await send(svm, voltr, [await deposit(voltr, 500n * USDC_1)]), "deposit 500");
      // Asking for the full 500 back: burns everything but fees leave < 500.
      expectAdaptorError(await send(svm, voltr, [await withdraw(voltr, 500n * USDC_1)]), "WithdrawNotSettled", "withdraw 500");
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
    name: "rejects lookalike vault/mint/tranche set (NAV-suppression attack)",
    async run(svm) {
      const voltr = await initialized(svm, 0n);
      const fake = await forgeLookalike(svm, voltr);
      const full = { vault: fake.vault, mint: fake.mint, shareAta: fake.shareAta, tranche: fake.tranche };
      for (const [name, op] of [["deposit", deposit], ["withdraw", withdraw]]) {
        expectAdaptorError(await send(svm, voltr, [await op(voltr, 0, full)]), "InvalidProtocolVault", `${name} full set`);
        expectAdaptorError(await send(svm, voltr, [await op(voltr, 0, { ...full, strategy: fake.vault })]), "UnknownStrategy", `${name} full set + strategy`);
        expectAdaptorError(await send(svm, voltr, [await op(voltr, 0, { mint: fake.mint, shareAta: fake.shareAta })]), "InvalidShareMint", `${name} mint`);
        expectAdaptorError(await send(svm, voltr, [await op(voltr, 0, { tranche: fake.tranche })]), "InvalidProtocolAccount", `${name} tranche`);
      }
      expectAdaptorError(await send(svm, voltr, [await initialize(voltr, { strategy: fake.vault, vault: fake.vault, mint: fake.mint })]), "UnknownStrategy", "initialize lookalike");
      expectAdaptorError(await send(svm, voltr, [await initialize(voltr, { mint: fake.mint })]), "InvalidShareMint", "initialize mint");
    },
  },
  {
    name: "rejects a non-canonical share token account",
    async run(svm) {
      const voltr = await initialized(svm, 0n);
      const other = randomAddress();
      setTokenAccount(svm, other, USDSTAR, voltr.auth.address, 0);
      expectAdaptorError(await send(svm, voltr, [await deposit(voltr, 0, { shareAta: other })]), "InvalidShareAta", "deposit");
      expectAdaptorError(await send(svm, voltr, [await withdraw(voltr, 0, { shareAta: other })]), "InvalidShareAta", "withdraw");
    },
  },
];

module.exports = {
  name: "perena",
  programs: [{ id: BANKINECO, file: path.join(__dirname, "bankineco.so") }],
  fixtures: path.join(__dirname, "fixtures.json"),
  strategies: [P.vault],
  // Hooks for the cross-protocol checks in run-all.js.
  initializeForeign: (svm) => initialized(svm, 0n),
  refreshIxs: async (voltr, strategy) => [await deposit(voltr, 0, { strategy }), await withdraw(voltr, 0, { strategy })],
  cases,
};
