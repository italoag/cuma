// The cuma binary lists every configured provider with its capabilities, and
// doctor reports which sandbox each agent uses.
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { run, fail, repo } from "./lib.mjs";

const build = run("cargo", ["build", "-p", "cuma-cli"]);
if (build.code !== 0) fail(`build failed:\n${build.out.slice(-4000)}`);
const cuma = path.join(repo, "target", "debug", "cuma");

const ws = mkdtempSync(path.join(tmpdir(), "cuma-cli-check-"));
try {
  mkdirSync(path.join(ws, ".cuma"));
  writeFileSync(
    path.join(ws, ".cuma", "config.toml"),
    `
[security]
agent_sandbox = "container"

[sandboxes.container]
kind = "docker"
image = "node:22-bookworm"

[sandboxes.vm]
kind = "microsandbox"
image = "node:22"

[sandboxes.mac]
kind = "arcbox"
image = "node:22"

[sandboxes.cluster]
kind = "kubernetes"
image = "node:22"

[sandboxes.cube]
kind = "e2b"
api_url = "https://api.cube.example"
domain = "cube.example"
template = "coding-agent"

[sandboxes.osb]
kind = "opensandbox"
server_url = "http://127.0.0.1:8080"
image = "node:22"

[sandboxes.wasm]
kind = "wasmer"

[sandboxes.jail]
kind = "native"
runtime = "sandbox-exec"

[sandboxes.custom]
kind = "command"
prefix = ["env", "--"]

[sandboxes.aos]
kind = "plugin"
program = "/nonexistent/cuma-sandbox-agentos"

[agents.devin]
protocol = "acp"
command = "devin acp"
sandbox = "vm"
state = ["~/.config/devin"]
env = ["DEVIN_API_KEY"]
`,
  );

  const list = run(cuma, ["sandbox", "list", "--json"], { cwd: ws });
  if (list.code !== 0) fail(`sandbox list failed:\n${list.out}`);
  const start = list.out.indexOf("[");
  let entries;
  try {
    entries = JSON.parse(list.out.slice(start, list.out.lastIndexOf("]") + 1));
  } catch (err) {
    fail(`sandbox list --json is not JSON: ${err}\n${list.out}`);
  }
  const kinds = new Set(entries.map((e) => e.kind));
  for (const kind of ["docker", "microsandbox", "arcbox", "kubernetes", "e2b", "opensandbox", "wasmer", "native", "command", "plugin"]) {
    if (!kinds.has(kind)) fail(`sandbox list lacks kind ${kind}: ${JSON.stringify(entries)}`);
  }
  for (const entry of entries) {
    for (const field of ["name", "kind", "isolation", "workspace", "network_allowlist"]) {
      if (!(field in entry)) fail(`entry ${entry.name} lacks ${field}`);
    }
  }
  const byName = Object.fromEntries(entries.map((e) => [e.name, e]));
  if (byName.vm.isolation !== "microvm") fail(`microsandbox isolation is ${byName.vm.isolation}`);
  if (byName.cluster.workspace !== "copied") fail(`kubernetes workspace is ${byName.cluster.workspace}`);
  if (byName.container.workspace !== "mounted") fail(`docker workspace is ${byName.container.workspace}`);
  if (!byName.vm.network_allowlist) fail("microsandbox should enforce network allowlists");
  if (!byName.container.default) fail("the configured default sandbox is not marked");
  if (!(byName.vm.agents ?? []).includes("devin")) fail(`devin is not shown using vm: ${JSON.stringify(byName.vm)}`);

  const unknown = run(cuma, ["sandbox", "probe", "no-such-sandbox"], { cwd: ws });
  if (unknown.code === 0) fail("probing an unknown sandbox succeeded");
  if (!unknown.out.includes("no-such-sandbox")) fail(`unhelpful error: ${unknown.out}`);

  const plugin = run(cuma, ["sandbox", "probe", "aos"], { cwd: ws });
  if (plugin.code === 0) fail("a plugin whose program is missing probed successfully");

  console.log(`${entries.length} sandboxes listed`);
  console.log("cli sandbox verified");
} finally {
  rmSync(ws, { recursive: true, force: true });
}
