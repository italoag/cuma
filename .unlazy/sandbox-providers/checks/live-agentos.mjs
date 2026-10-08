// A real agentOS VM runs an ACP agent through the plugin provider and the
// reference plugin, writing into the workspace as the user. Requires node and
// @rivet-dev/agentos-core in CUMA_LIVE_AGENTOS_MODULES (default: bun's global
// node_modules).
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { run, fail, passedTests } from "./lib.mjs";

const modules = process.env.CUMA_LIVE_AGENTOS_MODULES ?? join(homedir(), ".bun", "install", "global", "node_modules");
if (!existsSync(join(modules, "@rivet-dev", "agentos-core", "package.json"))) {
  fail(`@rivet-dev/agentos-core is not installed in ${modules}`);
}

const r = run("cargo", ["test", "-p", "cuma-sandbox", "--test", "live_agentos", "--", "--nocapture"], {
  env: {
    ...process.env,
    CARGO_INCREMENTAL: "0",
    CARGO_PROFILE_DEV_DEBUG: "line-tables-only",
    RUSTFLAGS: "-D warnings",
    CUMA_LIVE_AGENTOS_MODULES: modules,
  },
});
if (r.code !== 0) fail(`live agentOS test failed:\n${r.out.slice(-6000)}`);
if (!passedTests(r.out).some((p) => p.endsWith("an_acp_agent_runs_in_an_agentos_vm_through_the_reference_plugin"))) {
  fail("the live agentOS test did not pass");
}
if (!r.out.includes("LIVE-AGENTOS-ROUND-TRIP-COMPLETE")) fail("the live round trip did not run");
console.log("live agentos verified");
