// A real WASIX sandbox runs an ACP agent through the wasmer provider, with
// the workspace mapped. Requires `wasmer` (on PATH or in ~/.wasmer/bin) and
// access to its package registry for `wasmer/bash`.
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { run, fail, passedTests } from "./lib.mjs";

const installed = join(homedir(), ".wasmer", "bin", "wasmer");
const program = process.env.CUMA_LIVE_WASMER_PROGRAM ?? (existsSync(installed) ? installed : "wasmer");
const version = run(program, ["--version"]);
if (version.code !== 0) fail(`wasmer is not installed: ${version.out}`);

const r = run("cargo", ["test", "-p", "cuma-sandbox", "--test", "live_wasmer", "--", "--nocapture"], {
  env: {
    ...process.env,
    CARGO_INCREMENTAL: "0",
    CARGO_PROFILE_DEV_DEBUG: "line-tables-only",
    RUSTFLAGS: "-D warnings",
    CUMA_LIVE_WASMER_PACKAGE: process.env.CUMA_LIVE_WASMER_PACKAGE ?? "wasmer/bash",
    CUMA_LIVE_WASMER_PROGRAM: program,
  },
});
if (r.code !== 0) fail(`live wasmer test failed:\n${r.out.slice(-6000)}`);
if (!passedTests(r.out).some((p) => p.endsWith("an_acp_agent_runs_in_a_wasix_sandbox_with_the_workspace_mapped"))) {
  fail("the live wasmer test did not pass");
}
if (!r.out.includes("LIVE-WASMER-ROUND-TRIP-COMPLETE")) fail("the live round trip did not run");
console.log("live wasmer verified");
