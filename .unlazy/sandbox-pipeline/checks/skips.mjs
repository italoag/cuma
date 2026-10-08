// Every live test compiles, and without its environment passes having done
// nothing — saying so — so `cargo test` stays hermetic.
import { readdirSync } from "node:fs";
import { run, fail, passedTests } from "../../sandbox-providers/checks/lib.mjs";

const tests = readdirSync("crates/cuma-sandbox/tests")
  .filter((name) => /^live_[a-z0-9_]+\.rs$/.test(name))
  .map((name) => name.replace(/\.rs$/, ""));
if (tests.length < 9) fail(`only ${tests.length} live tests: ${tests.join(", ")}`);

const env = Object.fromEntries(Object.entries(process.env).filter(([name]) => !name.startsWith("CUMA_LIVE_")));
Object.assign(env, { CARGO_INCREMENTAL: "0", CARGO_PROFILE_DEV_DEBUG: "line-tables-only", RUSTFLAGS: "-D warnings" });
const args = ["test", "-p", "cuma-sandbox", ...tests.flatMap((t) => ["--test", t]), "--", "--nocapture"];
const r = run("cargo", args, { env });
if (r.code !== 0) fail(`the live tests did not pass without their environment:\n${r.out.slice(-6000)}`);

const passed = passedTests(r.out);
if (passed.length !== tests.length) fail(`${passed.length} of ${tests.length} live tests passed`);
const skips = (r.out.match(/is not set; skipping the live /g) ?? []).length;
if (skips !== tests.length) fail(`${skips} of ${tests.length} live tests said they were skipping`);
console.log("live tests skip cleanly");
