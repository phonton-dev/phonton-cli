#!/usr/bin/env node

const fs = require("fs");
const path = require("path");
const { spawnSync } = require("child_process");

const root = path.resolve(__dirname, "..");
const packageJson = require(path.join(root, "package.json"));
const binaryName = process.platform === "win32" ? "phonton.exe" : "phonton";
const candidates = [
  path.join(root, "target", "release", binaryName),
  path.join(root, "target", "debug", binaryName),
];
const binary = candidates.find((candidate) => fs.existsSync(candidate));

if (!binary) {
  console.error("No built Phonton binary found under target/release or target/debug.");
  process.exit(1);
}

function runWrapper(args) {
  const result = spawnSync(process.execPath, [path.join(root, "npm", "bin", "phonton.js"), ...args], {
    cwd: root,
    encoding: "utf8",
    env: {
      ...process.env,
      PHONTON_BINARY: binary,
    },
  });

  process.stdout.write(result.stdout || "");
  process.stderr.write(result.stderr || "");

  if (result.status !== 0) {
    process.exit(result.status || 1);
  }

  return result.stdout;
}

const versionOutput = runWrapper(["version"]);

const expected = `phonton ${packageJson.version}`;
if (!versionOutput.includes(expected)) {
  console.error(`Expected npm wrapper to report ${expected}.`);
  process.exit(1);
}

const goal = "add input validation to config loading";
const planOutput = runWrapper(["plan", "--json", "--no-memory", goal]);
let planJson;
try {
  planJson = JSON.parse(planOutput);
} catch (error) {
  console.error(`Expected plan --json to emit parseable JSON: ${error.message}`);
  process.exit(1);
}

if (!planJson.goal_contract) {
  console.error("Expected plan --json to expose goal_contract at the top level.");
  process.exit(1);
}

if (planJson.goal_contract.goal !== goal) {
  console.error("Expected plan --json goal_contract.goal to match the requested goal.");
  process.exit(1);
}
