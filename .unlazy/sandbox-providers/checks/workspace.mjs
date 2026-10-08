// Usage: node workspace.mjs fmt|clippy|tests
// Whole-workspace quality gates, in the configuration CI uses.
import { run, fail, failedTests, totals } from "./lib.mjs";

// Measured with `cargo test --workspace` before this work began.
const BASELINE_TESTS = 756;

const mode = process.argv[2];

if (mode === "fmt") {
  const r = run("cargo", ["fmt", "--all", "--", "--check"]);
  if (r.code !== 0) fail(`cargo fmt --check:\n${r.out.slice(-4000)}`);
  console.log("formatting verified");
} else if (mode === "clippy") {
  const all = run("cargo", ["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]);
  if (all.code !== 0) fail(`clippy (workspace):\n${all.out.slice(-6000)}`);
  const otel = run("cargo", ["clippy", "-p", "cuma-cli", "--features", "otel", "--all-targets", "--", "-D", "warnings"]);
  if (otel.code !== 0) fail(`clippy (otel):\n${otel.out.slice(-6000)}`);
  console.log("clippy verified");
} else if (mode === "tests") {
  const r = run("cargo", ["test", "--workspace"]);
  const failed = failedTests(r.out);
  if (failed.length > 0) fail(`failing tests: ${failed.join(", ")}`);
  if (r.code !== 0) fail(`cargo test --workspace exited ${r.code}\n${r.out.slice(-6000)}`);
  const { passed, failed: failures } = totals(r.out);
  if (failures !== 0) fail(`${failures} failures`);
  if (passed <= BASELINE_TESTS) fail(`${passed} tests passed; expected more than the ${BASELINE_TESTS} baseline`);
  console.log(`${passed} tests passed (baseline ${BASELINE_TESTS})`);
  console.log("test suite verified");
} else {
  fail("usage: workspace.mjs fmt|clippy|tests");
}
