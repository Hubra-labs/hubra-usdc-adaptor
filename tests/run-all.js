// Runs every protocol suite against one adaptor build, plus cross-protocol
// isolation checks. Run after ANY change (and always after adding a
// protocol): a new integration is only mergeable when this is fully green.
//
//   node tests/run-all.js            # everything
//   node tests/run-all.js perena     # one suite (+ cross-protocol checks)
const fs = require("fs");
const path = require("path");
const { createSvm, send, expectAdaptorError, randomAddress } = require("./harness");

const dir = path.join(__dirname, "protocols");
const suites = fs.readdirSync(dir)
  .filter((d) => fs.existsSync(path.join(dir, d, "suite.js")))
  .sort()
  .map((d) => require(path.join(dir, d, "suite.js")));
const only = process.argv[2];

const jobs = [];
for (const s of suites) {
  if (only && s.name !== only) continue;
  for (const c of s.cases) jobs.push({ suite: s.name, name: c.name, run: c.run });
}

// Cross-protocol isolation: each protocol must reject every other protocol's
// strategy ids (and an unregistered one) with UnknownStrategy, so a manager
// can't route one strategy's funds or NAV through another integration.
for (const s of suites) {
  const foreign = [
    ...suites.filter((t) => t !== s).flatMap((t) => t.strategies.map((k) => ({ from: t.name, key: k }))),
    { from: "unregistered", key: randomAddress() },
  ];
  for (const f of foreign) {
    jobs.push({
      suite: "isolation",
      name: `${s.name} rejects ${f.from} strategy ${f.key.slice(0, 8)}…`,
      async run(svm) {
        const voltr = await s.initializeForeign(svm);
        for (const ix of await s.refreshIxs(voltr, f.key)) {
          expectAdaptorError(await send(svm, voltr, [ix]), "UnknownStrategy", "refresh");
        }
      },
    });
  }
}

(async () => {
  let failed = 0;
  for (const job of jobs) {
    const t0 = Date.now();
    try {
      await job.run(createSvm(suites));
      console.log(`PASS  [${job.suite}] ${job.name} (${Date.now() - t0}ms)`);
    } catch (e) {
      failed++;
      console.log(`FAIL  [${job.suite}] ${job.name}\n    ${e.message}`);
    }
  }
  console.log(`\n${jobs.length - failed}/${jobs.length} passed`);
  process.exit(failed ? 1 : 0);
})();
