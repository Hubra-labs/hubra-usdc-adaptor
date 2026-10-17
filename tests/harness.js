// Shared LiteSVM harness for every protocol suite.
//
// Each test case gets a fresh SVM that has the adaptor AND every protocol's
// programs + fixtures loaded, so a suite always runs in the same environment
// as all the others — adding a protocol cannot silently change what an
// existing suite sees without that suite failing.
//
// litesvm 1.x (@solana/kit) on purpose: the 0.x web3.js-v1 bindings abort
// the process (std::bad_alloc) on GC after any SBF program invocation.
const fs = require("fs");
const path = require("path");
const crypto = require("crypto");
const { LiteSVM, Clock } = require("litesvm");
const {
  address, AccountRole, generateKeyPairSigner, getAddressEncoder, getAddressDecoder,
  getProgramDerivedAddress, pipe, createTransactionMessage, setTransactionMessageFeePayerSigner,
  appendTransactionMessageInstructions, signTransactionMessageWithSigners, lamports,
} = require("@solana/kit");

const B = address;
const ADAPTOR = B("raRnoKWoRcon9dqxvAhtiPyQHgikpcu1g9tXeb9jPGT");
const ADAPTOR_SO = path.join(__dirname, "..", "target", "deploy", "hubra_usdc_adaptor.so");
const USDC = B("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const TOKEN = B("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const ATA_PROG = B("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const SYSTEM = B("11111111111111111111111111111111");
const COMPUTE_BUDGET = B("ComputeBudget111111111111111111111111111111");

const enc = getAddressEncoder();
const randomAddress = () => getAddressDecoder().decode(crypto.randomBytes(32));
const meta = (addr, writable, signer = false) => ({
  address: addr,
  role: signer
    ? (writable ? AccountRole.WRITABLE_SIGNER : AccountRole.READONLY_SIGNER)
    : (writable ? AccountRole.WRITABLE : AccountRole.READONLY),
});
const disc = (name) => crypto.createHash("sha256").update(`global:${name}`).digest().subarray(0, 8);
const ata = async (owner, mint, tokenProgram = TOKEN) =>
  (await getProgramDerivedAddress({ programAddress: ATA_PROG, seeds: [enc.encode(owner), enc.encode(tokenProgram), enc.encode(mint)] }))[0];

// ------------------------------------------------------------------ svm setup

function loadFixtures(file) {
  const { _meta = {}, ...accounts } = JSON.parse(fs.readFileSync(file, "utf8"));
  return { meta: _meta, accounts };
}

function setRawAccount(svm, addr, { lamports: l, data, owner }) {
  svm.setAccount({
    address: addr, lamports: lamports(BigInt(l)), data: new Uint8Array(data),
    programAddress: owner, executable: false, space: BigInt(data.length),
  });
}

/** Fresh SVM with the adaptor and every suite's programs/fixtures loaded. */
function createSvm(suites) {
  if (!fs.existsSync(ADAPTOR_SO)) throw new Error(`missing ${ADAPTOR_SO} — run cargo-build-sbf first`);
  const svm = new LiteSVM();
  svm.addProgramFromFile(ADAPTOR, ADAPTOR_SO);
  let clockTs = 0;
  for (const s of suites) {
    for (const p of s.programs) svm.addProgramFromFile(p.id, p.file);
    const { meta: m, accounts } = loadFixtures(s.fixtures);
    for (const [addr, f] of Object.entries(accounts)) {
      setRawAccount(svm, B(addr), { lamports: f.lamports, data: Buffer.from(f.data, "base64"), owner: B(f.owner) });
    }
    clockTs = Math.max(clockTs, m.fetchedAt || 0);
  }
  // Pin the clock to when fixtures were fetched so oracle staleness checks
  // behave as they did on mainnet at that moment.
  const ts = BigInt(clockTs || Math.floor(Date.now() / 1000));
  svm.setClock(new Clock(1n, ts, 0n, 0n, ts));
  return svm;
}

// ---------------------------------------------------------------- token utils

function setTokenAccount(svm, addr, mint, owner, amount, tokenProgram = TOKEN) {
  const d = Buffer.alloc(165);
  Buffer.from(enc.encode(mint)).copy(d, 0);
  Buffer.from(enc.encode(owner)).copy(d, 32);
  d.writeBigUInt64LE(BigInt(amount), 64);
  d.writeUInt8(1, 108); // AccountState::Initialized
  setRawAccount(svm, addr, { lamports: 2_039_280, data: d, owner: tokenProgram });
}

function tokenBalance(svm, addr) {
  const acc = svm.getAccount(addr);
  return acc.exists ? Buffer.from(acc.data).readBigUInt64LE(64) : null;
}

// -------------------------------------------------------- voltr-shaped calls

/**
 * Stand-in for the Voltr vault's strategy authority: a funded signer with a
 * USDC ATA holding `usdc` base units. (On mainnet it is a PDA the vault
 * program signs for; the adaptor only sees a signer either way.)
 */
async function voltrStrategy(svm, usdc = 0) {
  const auth = await generateKeyPairSigner();
  svm.airdrop(auth.address, lamports(10_000_000_000n));
  const usdcAta = await ata(auth.address, USDC);
  setTokenAccount(svm, usdcAta, USDC, auth.address, usdc);
  return { auth, usdcAta };
}

/** initialize_<protocol>: [payer, vault_strategy_auth, strategy, system_program, ..remaining] */
function initializeIx(protocol, voltr, strategy, remaining) {
  const a = voltr.auth.address;
  return {
    programAddress: ADAPTOR,
    accounts: [meta(a, true, true), meta(a, true, true), meta(strategy, false), meta(SYSTEM, false), ...remaining],
    data: new Uint8Array(disc(`initialize_${protocol}`)),
  };
}

/** deposit_/withdraw_<protocol>: [auth, strategy, mint, asset_ata, token_program, ..remaining] */
function operateIx(op, protocol, voltr, strategy, amount, remaining) {
  const data = Buffer.alloc(8 + 8 + 1); // disc ++ amount ++ None
  disc(`${op}_${protocol}`).copy(data, 0);
  data.writeBigUInt64LE(BigInt(amount), 8);
  return {
    programAddress: ADAPTOR,
    accounts: [
      meta(voltr.auth.address, true, true), meta(strategy, false), meta(USDC, false),
      meta(voltr.usdcAta, true), meta(TOKEN, false), ...remaining,
    ],
    data: new Uint8Array(data),
  };
}

function computeLimitIx(units) {
  const data = Buffer.alloc(5);
  data.writeUInt8(2, 0);
  data.writeUInt32LE(units, 1);
  return { programAddress: COMPUTE_BUDGET, accounts: [], data: new Uint8Array(data) };
}

/**
 * Send one transaction signed by `voltr.auth`. Returns { ok, err, logs,
 * returns } where `returns` lists the u64 values the adaptor returned.
 */
async function send(svm, voltr, ixs, cu = 1_400_000) {
  svm.expireBlockhash();
  const msg = pipe(
    createTransactionMessage({ version: 0 }),
    (m) => setTransactionMessageFeePayerSigner(voltr.auth, m),
    (m) => svm.setTransactionMessageLifetimeUsingLatestBlockhash(m),
    (m) => appendTransactionMessageInstructions([computeLimitIx(cu), ...ixs], m),
  );
  const res = svm.sendTransaction(await signTransactionMessageWithSigners(msg));
  const failed = res.constructor.name === "FailedTransactionMetadata";
  const logs = failed ? res.meta().logs() : res.logs();
  const returns = logs
    .filter((l) => l.startsWith(`Program return: ${ADAPTOR} `))
    .map((l) => Buffer.from(l.split(" ").pop(), "base64").readBigUInt64LE(0));
  return { ok: !failed, err: failed ? res.toString() : null, logs, returns };
}

// ------------------------------------------------------------------ asserts

function assert(cond, msg) {
  if (!cond) throw new Error(msg);
}

function expectOk(res, what) {
  if (!res.ok) throw new Error(`${what} failed: ${res.err}\n    ${res.logs.slice(-8).join("\n    ")}`);
  return res;
}

/** Expect the tx to fail with the adaptor's named error (anchor logs the variant name). */
function expectAdaptorError(res, errorName, what) {
  assert(!res.ok, `${what}: expected ${errorName}, but tx succeeded`);
  const hit = res.logs.some((l) => l.includes(`Error Code: ${errorName}.`));
  assert(hit, `${what}: expected ${errorName}, got:\n    ${res.logs.slice(-6).join("\n    ")}`);
}

module.exports = {
  B, ADAPTOR, USDC, TOKEN, ATA_PROG, meta, disc, ata, randomAddress,
  createSvm, loadFixtures, setRawAccount, setTokenAccount, tokenBalance,
  voltrStrategy, initializeIx, operateIx, send,
  assert, expectOk, expectAdaptorError,
};
