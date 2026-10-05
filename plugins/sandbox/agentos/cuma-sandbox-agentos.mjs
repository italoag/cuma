#!/usr/bin/env node
// CUMA sandbox plugin for agentOS (https://github.com/rivet-dev/agentos).
//
// agentOS is an operating system as a Node.js library: a Linux-like kernel
// whose processes are V8 isolates (JavaScript) and WebAssembly (sh, coreutils,
// sed, grep, git, …). This plugin boots one VM per launch with the workspace
// mounted writable at its own path, runs the agent inside it as your uid/gid
// (so what it writes is yours), and relays its stdio — so the agent's
// JSON-RPC reaches CUMA unchanged. Written against @rivet-dev/agentos-core
// 0.2.22.
//
// Install @rivet-dev/agentos-core (npm, pnpm or bun) and configure:
//
//   [sandboxes.aos]
//   kind = "plugin"
//   program = "/path/to/cuma-sandbox-agentos.mjs"
//   [sandboxes.aos.options]
//   module = "/Users/me/.bun/install/global/node_modules"   # where the package is,
//                                                            # unless it is next to this file
//   create = { }        # merged into AgentOs.create's options
//
// Guest programs are JavaScript or WebAssembly only: an agent that ships
// native binaries does not run; a pure-JavaScript one does. The network is
// open (agentOS denies external network by default, and agents need their
// model APIs); `create.permissions` can narrow it.
//
// Protocol (see docs/SANDBOXES.md): probe | open | close | release | abort,
// a JSON request on stdin and a JSON reply on stdout; plus `exec`, which the
// prefix returned by `open` points back at.

import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { homedir, tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const self = fileURLToPath(import.meta.url);
const [operation, ...rest] = process.argv.slice(2);
const PACKAGE = "@rivet-dev/agentos-core";
// Directories a guest has its own of, and the home directory and above:
// never mounted for a file the agent's command names.
const SYSTEM = ["/bin", "/sbin", "/usr", "/lib", "/etc", "/proc", "/sys", "/dev", "/System", "/Library"];

async function readRequest() {
  let text = "";
  for await (const chunk of process.stdin) text += chunk;
  return text.trim() ? JSON.parse(text) : {};
}

function reply(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}

// The package, from `options.module` (a node_modules directory) or from
// beside this file.
async function loadAgentOs(options) {
  try {
    if (options.module) {
      const resolve = createRequire(join(options.module, "noop.js")).resolve;
      return await import(pathToFileURL(resolve(PACKAGE)).href);
    }
    return await import(PACKAGE);
  } catch (error) {
    throw new Error(
      `${PACKAGE} could not be loaded (${error.message}); install it next to the plugin, ` +
        "or set options.module to the node_modules directory that holds it",
    );
  }
}

function within(path, dir) {
  return path === dir || path.startsWith(`${dir}/`);
}

// The mounts for one launch: the workspace and the agent's state writable;
// what its command names read-only — a named file brings its directory,
// never the home directory or above it, nor a system directory.
function mountsFor(session, hostDirMount) {
  const home = homedir();
  const writable = [session.workspace, ...session.state];
  const readable = new Set();
  for (const path of session.readable) {
    let dir = path;
    try {
      if (!statSync(path).isDirectory()) dir = dirname(path);
    } catch {
      continue;
    }
    if (dir === "/" || within(home, dir) || SYSTEM.some((system) => within(dir, system))) continue;
    if (writable.some((mounted) => within(dir, mounted))) continue;
    readable.add(dir);
  }
  return [
    ...writable.map((path) => hostDirMount(path, path, { readOnly: false })),
    ...[...readable].map((path) => hostDirMount(path, path, { readOnly: true })),
  ].sort((a, b) => a.path.length - b.path.length);
}

function createOptions(options, mounts) {
  const create = options.create ?? {};
  return {
    ...create,
    mounts: [...(create.mounts ?? []), ...mounts],
    user: create.user ?? { uid: process.getuid(), gid: process.getgid() },
    permissions: { network: "allow", ...(create.permissions ?? {}) },
  };
}

async function probe() {
  const options = (await readRequest()).options ?? {};
  const mod = await loadAgentOs(options);
  // Something must actually run inside it.
  const vm = await mod.AgentOs.create(createOptions(options, []));
  try {
    const { pid } = await vm.process.spawn("true", []);
    const exit = await vm.process.wait(pid);
    if (exit.exitCode !== 0) throw new Error(`true exited with ${JSON.stringify(exit)}`);
  } finally {
    await vm.dispose();
  }
  reply({ isolation: "wasm", workspace: "mounted", network_allowlist: false, secrets_outside: false });
}

async function open() {
  const request = await readRequest();
  const dir = mkdtempSync(join(tmpdir(), "cuma-agentos-"));
  const session = join(dir, "session.json");
  // Names only: values are read from this process's environment at exec.
  writeFileSync(
    session,
    JSON.stringify({
      workspace: request.workspace,
      cwd: request.purpose === "execute" ? request.workspace : null,
      state: request.state ?? [],
      readable: request.readable ?? [],
      keep_env: request.keep_env ?? [],
      options: request.options ?? {},
    }),
    { mode: 0o600 },
  );
  reply({ prefix: [process.execPath, self, "exec", session, "--"], session: dir });
}

async function exec() {
  const separator = rest.indexOf("--");
  const sessionFile = rest[0];
  const [command, ...args] = rest.slice(separator + 1);
  if (!sessionFile || separator < 0 || !command) {
    throw new Error("usage: cuma-sandbox-agentos exec <session> -- <command> [args...]");
  }
  const session = JSON.parse(readFileSync(sessionFile, "utf8"));
  const mod = await loadAgentOs(session.options);
  const env = {};
  for (const name of session.keep_env) {
    if (process.env[name] !== undefined) env[name] = process.env[name];
  }
  const vm = await mod.AgentOs.create(createOptions(session.options, mountsFor(session, mod.hostDirMount)));

  // Output is taken from the start, so nothing the agent says is missed.
  const { pid } = await vm.process.spawn(command, args, {
    cwd: session.cwd ?? undefined,
    env,
    onStdout: (chunk) => process.stdout.write(chunk),
    onStderr: (chunk) => process.stderr.write(chunk),
  });
  // Stdin in order, then closed.
  let queue = Promise.resolve();
  process.stdin.on("data", (chunk) => {
    queue = queue.then(() => vm.process.writeStdin(pid, new Uint8Array(chunk)));
  });
  process.stdin.on("end", () => {
    queue = queue.then(() => vm.process.closeStdin(pid));
  });

  const exit = await vm.process.wait(pid);
  await vm.dispose();
  process.exit(typeof exit.exitCode === "number" ? exit.exitCode : 1);
}

async function close() {
  const request = await readRequest();
  // A mounted workspace: nothing to bring back.
  if (request.session) rmSync(request.session, { recursive: true, force: true });
  reply({});
}

const operations = { probe, open, exec, close, release: close, abort: close };
const run = operations[operation];
if (!run) {
  console.error(`unknown operation ${operation}; expected one of ${Object.keys(operations).join(", ")}`);
  process.exit(2);
}
run().catch((error) => {
  console.error(error.message ?? String(error));
  process.exit(1);
});
