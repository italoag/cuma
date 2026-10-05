// The images the pipeline builds start from pinned bases that exist, the
// third-party images it mirrors are pinned and exist, and everything is
// published under ghcr.io/italoag/cuma-*.
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { fail } from "../../sandbox-providers/checks/lib.mjs";

const pinned = (ref) => /[:@]/.test(ref.split("/").pop()) && !/:latest$/.test(ref);

// registry/repo:tag → is there a manifest? Anonymous token flow, as a client
// pulling would do.
async function exists(ref) {
  let [name, tag] = ref.includes("@") ? ref.split("@") : [ref.slice(0, ref.lastIndexOf(":")), ref.slice(ref.lastIndexOf(":") + 1)];
  let registry = "registry-1.docker.io";
  const first = name.split("/")[0];
  if (name.includes("/") && (first.includes(".") || first.includes(":"))) {
    registry = first === "docker.io" ? "registry-1.docker.io" : first;
    name = name.slice(first.length + 1);
  }
  if (registry === "registry-1.docker.io" && !name.includes("/")) name = `library/${name}`;
  const url = `https://${registry}/v2/${name}/manifests/${tag}`;
  const accept = [
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
  ].join(", ");
  let response = await fetch(url, { method: "HEAD", headers: { Accept: accept } });
  if (response.status === 401) {
    const challenge = response.headers.get("www-authenticate") ?? "";
    const field = (key) => (challenge.match(new RegExp(`${key}="([^"]+)"`)) ?? [])[1];
    const realm = field("realm");
    if (!realm) return { ok: false, why: "401 without a token realm" };
    const params = new URLSearchParams();
    if (field("service")) params.set("service", field("service"));
    params.set("scope", field("scope") ?? `repository:${name}:pull`);
    const token = await (await fetch(`${realm}?${params}`)).json();
    response = await fetch(url, {
      method: "HEAD",
      headers: { Accept: accept, Authorization: `Bearer ${token.token ?? token.access_token}` },
    });
  }
  return { ok: response.ok, why: `HTTP ${response.status}` };
}

const problems = [];
const check = async (what, ref) => {
  if (!pinned(ref)) return problems.push(`${what}: ${ref} is not pinned`);
  try {
    const found = await exists(ref);
    if (!found.ok) problems.push(`${what}: ${ref} not found (${found.why})`);
  } catch (error) {
    problems.push(`${what}: ${ref} could not be checked (${error.message})`);
  }
};

// The images built here.
const root = "ci/sandboxes/images";
const images = readdirSync(root).filter((name) => existsSync(`${root}/${name}/Dockerfile`));
for (const want of ["sandbox-test", "agent-node"]) {
  if (!images.includes(want)) problems.push(`${root}/${want}/Dockerfile is missing`);
}
for (const image of images) {
  const text = readFileSync(`${root}/${image}/Dockerfile`, "utf8");
  const base = (text.match(/^ARG BASE=(\S+)/m) ?? [])[1];
  if (!base) problems.push(`${image}: no pinned ARG BASE`);
  else await check(`${image} base`, base);
  if (!/^FROM \$\{BASE\}/m.test(text)) problems.push(`${image}: FROM must use \${BASE}`);
}

// The third-party images mirrored.
const lines = readFileSync("ci/sandboxes/mirror.txt", "utf8")
  .split("\n")
  .map((line) => line.trim())
  .filter((line) => line && !line.startsWith("#"));
if (lines.length === 0) problems.push("ci/sandboxes/mirror.txt lists nothing");
for (const line of lines) {
  const [source, target, ...rest] = line.split(/\s+/);
  if (!target || rest.length > 0) {
    problems.push(`mirror.txt: "${line}" is not "<source> <target>"`);
    continue;
  }
  if (!/^cuma-[a-z0-9-]+:[A-Za-z0-9._-]+$/.test(target)) problems.push(`mirror.txt: ${target} is not cuma-<name>:<tag>`);
  await check("mirror source", source);
}

// Where they go.
const workflow = readFileSync(".github/workflows/sandbox-images.yml", "utf8");
if (!workflow.includes("ghcr.io/italoag/cuma")) problems.push("sandbox-images.yml does not publish under ghcr.io/italoag/cuma");
if (!workflow.includes("ci/sandboxes/mirror.txt")) problems.push("sandbox-images.yml does not mirror ci/sandboxes/mirror.txt");
if (!/push: \$\{\{ github\.event_name != 'pull_request' \}\}/.test(workflow)) {
  problems.push("sandbox-images.yml must build pull requests without publishing them");
}

if (problems.length > 0) fail(problems.join("\n"));
console.log("images verified");
