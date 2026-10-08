// Usage: node tests.mjs <cargo package> <required-test-fragment>...
// Runs the package's tests and requires that, for every fragment, at least
// one *passing* test name contains it — so a module with no tests, or a test
// that was renamed away, fails the gate instead of passing vacuously.
import { run, fail, passedTests, failedTests, totals } from "./lib.mjs";

const [pkg, ...required] = process.argv.slice(2);
if (!pkg || required.length === 0) fail("usage: tests.mjs <package> <fragment>...");

const { code, out } = run("cargo", ["test", "-p", pkg]);
const failed = failedTests(out);
if (failed.length > 0) fail(`${pkg}: failing tests: ${failed.join(", ")}`);
if (code !== 0) fail(`${pkg}: cargo test exited ${code}\n${out.slice(-4000)}`);

const passed = passedTests(out);
const missing = required.filter((fragment) => !passed.some((name) => name.includes(fragment)));
if (missing.length > 0) fail(`${pkg}: no passing test matches: ${missing.join(", ")}`);

const { passed: count, failed: failures } = totals(out);
if (failures !== 0) fail(`${pkg}: ${failures} failures reported`);
console.log(`${pkg}: ${count} passed, every required fragment covered`);
console.log(`tests verified: ${pkg}`);
