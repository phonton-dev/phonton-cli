#!/usr/bin/env node

const fs = require("fs");
const os = require("os");
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

testStaleVendorReinstall();

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

function testStaleVendorReinstall() {
  const vendorDir = path.join(root, "npm", "vendor");
  const vendorBinary = path.join(vendorDir, binaryName);
  const markerPath = path.join(vendorDir, "version.json");
  const installScript = path.join(os.tmpdir(), `phonton-test-install-${process.pid}.js`);

  fs.rmSync(vendorDir, { recursive: true, force: true });
  fs.mkdirSync(vendorDir, { recursive: true });
  fs.copyFileSync(binary, vendorBinary);
  if (process.platform !== "win32") {
    fs.chmodSync(vendorBinary, 0o755);
  }
  fs.writeFileSync(
    markerPath,
    JSON.stringify({
      version: "0.0.0-stale",
      platform: process.platform,
      arch: process.arch,
    }),
  );

  fs.writeFileSync(
    installScript,
    `
const fs = require("fs");
const path = require("path");
const vendorDir = ${JSON.stringify(vendorDir)};
const binary = ${JSON.stringify(binary)};
const binaryName = ${JSON.stringify(binaryName)};
const version = ${JSON.stringify(packageJson.version)};
fs.mkdirSync(vendorDir, { recursive: true });
fs.copyFileSync(binary, path.join(vendorDir, binaryName));
if (process.platform !== "win32") fs.chmodSync(path.join(vendorDir, binaryName), 0o755);
fs.writeFileSync(path.join(vendorDir, "version.json"), JSON.stringify({
  version,
  platform: process.platform,
  arch: process.arch,
}, null, 2));
`,
  );

  try {
    const result = spawnSync(process.execPath, [path.join(root, "npm", "bin", "phonton.js"), "version"], {
      cwd: root,
      encoding: "utf8",
      env: {
        ...process.env,
        PHONTON_INSTALL_SCRIPT: installScript,
        PHONTON_BINARY: "",
        PHONTON_CLI_BINARY: "",
      },
    });

    process.stdout.write(result.stdout || "");
    process.stderr.write(result.stderr || "");

    if (result.status !== 0 || !result.stdout.includes(expected)) {
      console.error("Expected npm wrapper to refresh stale vendor binary metadata.");
      process.exit(result.status || 1);
    }
  } finally {
    fs.rmSync(installScript, { force: true });
    fs.rmSync(vendorDir, { recursive: true, force: true });
  }
}
