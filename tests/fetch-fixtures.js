// Refresh every suite's forked-mainnet state: re-fetch the accounts listed in
// its fixtures.json and re-dump its programs. Run when a protocol's on-chain
// state has moved enough to break a lifecycle test (e.g. oracle staleness).
//
//   RPC_URL=https://... node tests/fetch-fixtures.js [suite]
// Defaults to the RPC in the local solana CLI config.
const fs = require("fs");
const path = require("path");
const { execFileSync } = require("child_process");
const { createSolanaRpc } = require("@solana/kit");

function rpcUrl() {
  if (process.env.RPC_URL) return process.env.RPC_URL;
  const out = execFileSync("solana", ["config", "get"], { encoding: "utf8" });
  return out.match(/RPC URL:\s*(\S+)/)[1];
}

(async () => {
  const url = rpcUrl();
  const rpc = createSolanaRpc(url);
  const dir = path.join(__dirname, "protocols");
  for (const d of fs.readdirSync(dir).sort()) {
    if (process.argv[2] && d !== process.argv[2]) continue;
    const suite = require(path.join(dir, d, "suite.js"));
    const { _meta, ...old } = JSON.parse(fs.readFileSync(suite.fixtures, "utf8"));
    const keys = Object.keys(old);
    const { context, value: infos } = await rpc.getMultipleAccounts(keys, { encoding: "base64" }).send();
    const slot = context.slot;
    const out = { _meta: { fetchedAt: Math.floor(Date.now() / 1000), slot: Number(slot) } };
    keys.forEach((k, i) => {
      const a = infos[i];
      if (!a) throw new Error(`${d}: account ${k} (${old[k].name}) not found`);
      out[k] = { name: old[k].name, lamports: Number(a.lamports), owner: a.owner, data: a.data[0] };
    });
    fs.writeFileSync(suite.fixtures, JSON.stringify(out, null, 1) + "\n");
    for (const p of suite.programs) {
      execFileSync("solana", ["program", "dump", p.id, p.file, "--url", url], { stdio: "inherit" });
    }
    console.log(`${d}: ${keys.length} accounts @ slot ${slot}, ${suite.programs.length} program(s)`);
  }
})().catch((e) => { console.error(e); process.exit(1); });
