// The Sandboxes workflow lints clean and its pull_request paths include the
// CLI's launch wiring and native confinement, besides the sandbox crate.
import { readFileSync } from "node:fs";
import path from "node:path";
import { repo, run, fail } from "../../sandbox-providers/checks/lib.mjs";

const file = path.join(repo, ".github/workflows/sandboxes.yml");
const lint = run("actionlint", [file]);
if (lint.code !== 0) fail(`actionlint:\n${lint.out}`);

const text = readFileSync(file, "utf8");
const block = text.match(/pull_request:\n\s+paths:\n((?:\s+- .*\n)+)/);
if (!block) fail("no pull_request paths block found");
const paths = [...block[1].matchAll(/- "([^"]+)"/g)].map((m) => m[1]);
const required = [
  "crates/cuma-sandbox/**",
  "crates/cuma-cli/src/harness.rs",
  "crates/cuma-workspace/src/confine.rs",
  "crates/cuma-protocol-acp/src/adapter.rs",
];
const missing = required.filter((p) => !paths.includes(p));
if (missing.length > 0) fail(`pull_request paths lack: ${missing.join(", ")}`);
console.log(`${paths.length} trigger paths, all required present`);
console.log("workflow triggers verified");
