// Every sandbox kind has a live test and a job in the sandboxes workflow
// that runs it; the workflow runs on pull requests that touch the sandbox
// code and by hand; self-hosted jobs only run when a repository variable
// enables them, and never for pull requests from forks.
import { existsSync, readFileSync } from "node:fs";
import { fail } from "../../sandbox-providers/checks/lib.mjs";

const workflow = readFileSync(".github/workflows/sandboxes.yml", "utf8");

// Triggers.
const on = workflow.slice(workflow.indexOf("\non:"), workflow.indexOf("\njobs:"));
for (const needle of ["pull_request:", "workflow_dispatch:", '"crates/cuma-sandbox/**"', '"plugins/**"']) {
  if (!on.includes(needle)) fail(`the workflow's triggers lack ${needle}`);
}
if (/\n\s+push:/.test(on)) fail("the sandboxes workflow must not run on every push");

// Jobs, as blocks under `jobs:`.
const body = workflow.slice(workflow.indexOf("\njobs:") + 6);
const jobs = new Map();
let current = null;
for (const line of body.split("\n")) {
  const header = line.match(/^  ([a-z0-9-]+):\s*$/);
  if (header) {
    current = header[1];
    jobs.set(current, "");
  } else if (current) {
    jobs.set(current, jobs.get(current) + line + "\n");
  }
}

const expected = {
  docker: { test: "live_docker", hosted: true },
  microsandbox: { test: "live_microsandbox", hosted: true, kvm: true },
  wasmer: { test: "live_wasmer", hosted: true },
  agentos: { test: "live_agentos", hosted: true },
  kubernetes: { test: "live_kubernetes", hosted: true },
  opensandbox: { test: "live_opensandbox", hosted: true },
  firecracker: { test: "live_firecracker", hosted: true, kvm: true },
  arcbox: { test: "live_arcbox", hosted: false, variable: "CUMA_ARCBOX_RUNNER" },
  cubesandbox: { test: "live_e2b", hosted: false, variable: "CUMA_CUBESANDBOX_RUNNER" },
};

for (const [job, want] of Object.entries(expected)) {
  const text = jobs.get(job);
  if (text === undefined) fail(`no ${job} job in the sandboxes workflow`);
  const file = `crates/cuma-sandbox/tests/${want.test}.rs`;
  if (!existsSync(file)) fail(`${file} does not exist`);
  if (!new RegExp(`--test ${want.test}\\b`).test(text)) fail(`the ${job} job does not run ${want.test}`);
  const runsOn = (text.match(/\n    runs-on: (.+)/) ?? [])[1] ?? "";
  if (want.hosted) {
    if (runsOn.trim() !== "ubuntu-latest") fail(`the ${job} job should run on ubuntu-latest, not ${runsOn}`);
  } else {
    if (!runsOn.includes("self-hosted")) fail(`the ${job} job should run on a self-hosted runner`);
    const condition = (text.match(/\n    if: (.+)/) ?? [])[1] ?? "";
    if (!condition.includes(`vars.${want.variable} == 'true'`)) {
      fail(`the ${job} job must be enabled by vars.${want.variable}`);
    }
    if (!condition.includes("github.event.pull_request.head.repo.full_name == github.repository")) {
      fail(`the ${job} job must not run a fork's pull request on a self-hosted runner`);
    }
  }
  if (want.kvm && !text.includes("/dev/kvm") && !text.includes("99-kvm")) fail(`the ${job} job does not enable KVM`);
}

const extra = [...jobs.keys()].filter((job) => !(job in expected));
if (extra.length > 0) fail(`unexpected jobs, not checked: ${extra.join(", ")}`);
console.log("pipeline coverage verified");
