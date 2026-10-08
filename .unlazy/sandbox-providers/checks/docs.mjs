// The plan and operator documentation cover every requested sandbox and every
// provider kind, with the facts an operator needs for each.
import { readFileSync, existsSync } from "node:fs";
import path from "node:path";
import { repo, fail } from "./lib.mjs";

const read = (file) => {
  const full = path.join(repo, file);
  if (!existsSync(full)) fail(`missing ${file}`);
  return readFileSync(full, "utf8");
};

const guide = read("docs/SANDBOXES.md");
const sandboxes = [
  "OpenSandbox",
  "CubeSandbox",
  "microsandbox",
  "agentOS",
  "ArcBox",
  "Kubernetes agent-sandbox",
  "Firecracker",
  "Wasmer",
];
const facts = ["Mechanism", "Workspace", "Network", "Credentials", "Status"];

for (const name of sandboxes) {
  const start = guide.indexOf(`\n### ${name}\n`);
  if (start < 0) fail(`docs/SANDBOXES.md has no "### ${name}" section`);
  const rest = guide.slice(start + 1);
  const next = rest.slice(4).search(/\n#{2,3} /);
  const section = next < 0 ? rest : rest.slice(0, next + 4);
  for (const fact of facts) {
    if (!section.includes(`**${fact}**`)) fail(`section ${name} does not state **${fact}**`);
  }
}

for (const heading of ["## Plan", "### Phase 1", "### Phase 2", "### Phase 3", "## Writing a plugin"]) {
  if (!guide.includes(`\n${heading}`)) fail(`docs/SANDBOXES.md lacks "${heading}"`);
}

const kinds = [
  "native", "docker", "microsandbox", "arcbox", "kubernetes", "e2b",
  "opensandbox", "wasmer", "command", "plugin",
];
const configuration = read("docs/CONFIGURATION.md");
if (!configuration.includes("[sandboxes.")) fail("CONFIGURATION.md does not document [sandboxes.*]");
for (const kind of kinds) {
  if (!configuration.includes(`kind = "${kind}"`)) fail(`CONFIGURATION.md has no kind = "${kind}" example`);
  if (!guide.includes(`kind = "${kind}"`)) fail(`SANDBOXES.md has no kind = "${kind}" example`);
}

const adr = read("docs/adr/ADR-018-sandbox-providers.md");
for (const word of ["## Decision", "## Consequences", "## Alternatives"]) {
  if (!adr.includes(word)) fail(`ADR-018 lacks "${word}"`);
}

for (const plugin of ["plugins/sandbox/agentos/cuma-sandbox-agentos.mjs", "plugins/sandbox/firecracker/cuma-sandbox-firecracker", "plugins/sandbox/firecracker/cuma-init"]) {
  read(plugin);
}

console.log(`${sandboxes.length} sandboxes, ${kinds.length} kinds, ADR and plugins present`);
console.log("docs coverage verified");
