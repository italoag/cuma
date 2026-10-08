// A real container engine runs an ACP agent through the docker provider, and
// the session tears the container down afterwards. Requires a reachable
// docker engine and an image with `sh` and `sed`.
import { run, fail, passedTests } from "./lib.mjs";

const image = process.env.CUMA_LIVE_DOCKER_IMAGE ?? "nginxinc/nginx-unprivileged:1.29-alpine";
const probe = run("docker", ["image", "inspect", image]);
if (probe.code !== 0) fail(`image ${image} is not available locally:\n${probe.out.slice(-1000)}`);

const r = run("cargo", ["test", "-p", "cuma-sandbox", "--test", "live_docker", "--", "--nocapture"], {
  env: { ...process.env, CARGO_INCREMENTAL: "0", CARGO_PROFILE_DEV_DEBUG: "line-tables-only", RUSTFLAGS: "-D warnings", CUMA_LIVE_DOCKER_IMAGE: image },
});
if (r.code !== 0) fail(`live docker test failed:\n${r.out.slice(-6000)}`);
const passed = passedTests(r.out);
const required = ["an_acp_agent_runs_in_a_container_and_the_container_is_removed"];
for (const name of required) {
  if (!passed.some((p) => p.endsWith(name))) fail(`did not pass: ${name}`);
}
// The test prints this only after the agent answered and teardown was checked,
// so a skipped (image-less) run cannot satisfy the gate.
if (!r.out.includes("LIVE-DOCKER-ROUND-TRIP-COMPLETE")) fail("the live round trip did not run");

const left = run("docker", ["ps", "-a", "--filter", "label=dev.cuma.sandbox=live-test", "-q"]);
if (left.code !== 0) fail(`docker ps failed: ${left.out}`);
if (left.out.trim() !== "") fail(`containers left behind: ${left.out.trim()}`);
console.log("live docker verified");
