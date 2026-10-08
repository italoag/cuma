// Shared helpers for the sandbox-provider gates. Every check exits nonzero on
// any failed assertion and prints its success marker only at the very end.
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

export const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");

// The same build configuration used throughout the work, so gates reuse the
// artifacts already on disk instead of building a second tree.
export const cargoEnv = {
  ...process.env,
  CARGO_INCREMENTAL: "0",
  CARGO_PROFILE_DEV_DEBUG: "line-tables-only",
  RUSTFLAGS: "-D warnings",
};

export function run(cmd, args, opts = {}) {
  const r = spawnSync(cmd, args, {
    cwd: repo,
    env: cargoEnv,
    encoding: "utf8",
    maxBuffer: 512 * 1024 * 1024,
    ...opts,
  });
  return { code: r.status, out: `${r.stdout ?? ""}${r.stderr ?? ""}`, error: r.error };
}

export function fail(message) {
  console.error(`CHECK FAILED: ${message}`);
  process.exit(1);
}

export function passedTests(out) {
  return [...out.matchAll(/^test (\S+) \.\.\. ok$/gm)].map((m) => m[1]);
}

export function failedTests(out) {
  return [...out.matchAll(/^test (\S+) \.\.\. FAILED$/gm)].map((m) => m[1]);
}

export function totals(out) {
  let passed = 0;
  let failed = 0;
  for (const m of out.matchAll(/^test result: \w+\. (\d+) passed; (\d+) failed;/gm)) {
    passed += Number(m[1]);
    failed += Number(m[2]);
  }
  return { passed, failed };
}
