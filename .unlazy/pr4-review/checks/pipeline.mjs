// Usage: node pipeline.mjs <pull request number>
// Every workflow run on the pull request's head commit has completed
// successfully, and CI and Sandboxes are among them.
import { spawnSync } from "node:child_process";
import { fail } from "../../sandbox-providers/checks/lib.mjs";

const gh = (...args) => {
  const r = spawnSync("gh", args, { encoding: "utf8", maxBuffer: 16 * 1024 * 1024 });
  if (r.status !== 0) fail(`gh ${args.join(" ")}: ${r.stderr}`);
  return r.stdout.trim();
};
const pr = process.argv[2];
if (!/^[0-9]+$/.test(pr ?? "")) fail("usage: pipeline.mjs <pull request number>");
const head = gh("api", `repos/italoag/cuma/pulls/${pr}`, "--jq", ".head.sha");
const runs = JSON.parse(gh("api", `repos/italoag/cuma/actions/runs?head_sha=${head}&per_page=50`))
  .workflow_runs;
// The latest attempt of each workflow decides.
const latest = new Map();
for (const run of runs) {
  const seen = latest.get(run.name);
  if (!seen || run.run_attempt > seen.run_attempt || run.id > seen.id) latest.set(run.name, run);
}
// "Sandbox images" runs only when image definitions change; when it does run,
// it must pass like the rest.
const expected = ["CI", "Sandboxes"];
const missing = expected.filter((name) => !latest.has(name));
if (missing.length > 0) fail(`${head.slice(0, 7)}: no run of ${missing.join(", ")}`);
const bad = [...latest.values()].filter((run) => run.status !== "completed" || run.conclusion !== "success");
if (bad.length > 0) {
  fail(`${head.slice(0, 7)}: ${bad.map((run) => `${run.name}=${run.status}/${run.conclusion}`).join(", ")}`);
}
console.log(`${head.slice(0, 7)}: ${[...latest.keys()].join(", ")} succeeded`);
console.log("pipeline green");
