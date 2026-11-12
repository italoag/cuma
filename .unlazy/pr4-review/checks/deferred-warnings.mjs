// End to end, on macOS: with only a global configuration at the old
// location (~/Library/Application Support/cuma), the real `cuma` binary
// still reads it and says so on stderr, after logging is set up. Before the
// fix the warning was emitted before tracing existed, and lost.
import { spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { repo, run, fail } from "../../sandbox-providers/checks/lib.mjs";

if (process.platform !== "darwin") fail("the old location exists only on macOS; run this there");
const build = run("cargo", ["build", "-q", "-p", "cuma-cli"]);
if (build.code !== 0) fail(`cargo build:\n${build.out.slice(-3000)}`);

const home = mkdtempSync(path.join(tmpdir(), "cuma-home-"));
const project = mkdtempSync(path.join(tmpdir(), "cuma-project-"));
try {
  const legacy = path.join(home, "Library", "Application Support", "cuma");
  mkdirSync(legacy, { recursive: true });
  // A value only this file sets, to prove it was read.
  writeFileSync(path.join(legacy, "config.toml"), '[router]\nstrategy = "cost-first"\n');
  const env = { ...process.env, HOME: home };
  delete env.XDG_CONFIG_HOME;
  for (const key of Object.keys(env)) if (key.startsWith("CUMA_") || key === "RUST_LOG") delete env[key];
  const r = spawnSync(path.join(repo, "target", "debug", "cuma"), ["doctor", "--json"], {
    cwd: project,
    env,
    encoding: "utf8",
    timeout: 120_000,
  });
  const stderr = r.stderr ?? "";
  if (!stderr.includes("old location")) fail(`no warning on stderr (exit ${r.status}):\n${stderr.slice(-2000)}`);
  if (!(r.stdout ?? "").includes(legacy)) fail(`cuma doctor does not list the file it read:\n${(r.stdout ?? "").slice(-2000)}`);
  console.log("the old file was read, listed by cuma doctor, and the warning reached stderr");
} finally {
  rmSync(home, { recursive: true, force: true });
  rmSync(project, { recursive: true, force: true });
}
console.log("deferred warnings verified");
