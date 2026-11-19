// The Firecracker plugin's env file is sourced by the guest's shell: names
// that are not variable names must never reach it. Imports the plugin (it
// runs nothing on import) and calls its env_lines with hostile names, next to
// a valid one as the positive control.
import { spawnSync } from "node:child_process";
import path from "node:path";
import { repo, fail } from "../../sandbox-providers/checks/lib.mjs";

const plugin = path.join(repo, "plugins/sandbox/firecracker/cuma-sandbox-firecracker");
const program = `
import importlib.machinery, importlib.util, sys
loader = importlib.machinery.SourceFileLoader("fc", sys.argv[1])
spec = importlib.util.spec_from_loader("fc", loader)
fc = importlib.util.module_from_spec(spec)
loader.exec_module(fc)
hostile = ["OK_NAME", "X=1; touch /tmp/pwned #", "$(id)", "A B", "UNSET"]
environ = {name: "it's" for name in hostile if name != "UNSET"}
got = fc.env_lines(hostile, environ)
want = ["OK_NAME=" + fc.sh_quote("it's") + "\\n"]
assert got == want, got
print("env_lines ok")
`;
const r = spawnSync("python3", ["-B", "-c", program, plugin], { encoding: "utf8" });
if (r.status !== 0 || !r.stdout.includes("env_lines ok")) {
  fail(`env_lines check failed (exit ${r.status}):\n${r.stdout}${r.stderr}`);
}
console.log("firecracker env names verified");
