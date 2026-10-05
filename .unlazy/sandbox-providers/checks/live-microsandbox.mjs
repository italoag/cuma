// A real libkrun microVM runs an ACP agent through the microsandbox provider,
// and the session removes the VM afterwards. Requires `msb` and an image it
// can pull (behind a registry restriction, one from an allowed registry).
import { run, fail, passedTests } from "./lib.mjs";

const image =
  process.env.CUMA_LIVE_MICROSANDBOX_IMAGE ?? "ghcr.io/italoag/mirror/nginxinc/nginx-unprivileged:1.29-alpine";
const msb = run("msb", ["--version"]);
if (msb.code !== 0) fail(`msb is not installed: ${msb.out}`);

const r = run("cargo", ["test", "-p", "cuma-sandbox", "--test", "live_microsandbox", "--", "--nocapture"], {
  env: {
    ...process.env,
    CARGO_INCREMENTAL: "0",
    CARGO_PROFILE_DEV_DEBUG: "line-tables-only",
    RUSTFLAGS: "-D warnings",
    CUMA_LIVE_MICROSANDBOX_IMAGE: image,
  },
});
if (r.code !== 0) fail(`live microsandbox test failed:\n${r.out.slice(-6000)}`);
if (!passedTests(r.out).some((p) => p.endsWith("an_acp_agent_runs_in_a_microvm_and_the_microvm_is_removed"))) {
  fail("the live microsandbox test did not pass");
}
// Printed only after the turn, the workspace check and the teardown check.
if (!r.out.includes("LIVE-MICROSANDBOX-ROUND-TRIP-COMPLETE")) fail("the live round trip did not run");

const left = run("msb", ["ls"]);
if (/\bcuma-[0-9a-f]{12}\b/.test(left.out)) fail(`microVMs left behind:\n${left.out}`);
console.log("live microsandbox verified");
