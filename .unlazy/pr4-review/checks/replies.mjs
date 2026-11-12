// Usage: node replies.mjs [pull request number, default 4]
// Every top-level inline review comment on the pull request has a reply
// written by italoag.
import { spawnSync } from "node:child_process";
import { fail } from "../../sandbox-providers/checks/lib.mjs";

const pr = process.argv[2] ?? "4";
if (!/^[0-9]+$/.test(pr)) fail("usage: replies.mjs [pull request number]");
const r = spawnSync(
  "gh",
  ["api", "--paginate", "--slurp", `repos/italoag/cuma/pulls/${pr}/comments?per_page=100`],
  { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 },
);
if (r.status !== 0) fail(`gh api failed: ${r.stderr}`);
const comments = JSON.parse(r.stdout).flat();
const findings = comments.filter((c) => !c.in_reply_to_id && c.user.login !== "italoag");
if (findings.length === 0) fail("no review comments found: the query is wrong, not the PR clean");
const answered = new Set(
  comments.filter((c) => c.in_reply_to_id && c.user.login === "italoag").map((c) => c.in_reply_to_id),
);
const open = findings.filter((c) => !answered.has(c.id));
if (open.length > 0) {
  fail(`unanswered: ${open.map((c) => `${c.id} ${c.path}:${c.line ?? c.original_line}`).join(", ")}`);
}
console.log(`${findings.length} review comments, all answered`);
console.log("every review comment answered");
