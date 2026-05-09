#!/usr/bin/env node

const fs = require("fs");
const path = require("path");
const { spawn, spawnSync } = require("child_process");

const packageJson = require("../../package.json");
const binaryName = process.platform === "win32" ? "phonton.exe" : "phonton";
const configuredBinary = process.env.PHONTON_BINARY || process.env.PHONTON_CLI_BINARY;
const binaryPath = configuredBinary
  ? path.resolve(configuredBinary)
  : path.join(__dirname, "..", "vendor", binaryName);
const vendorDir = path.join(__dirname, "..", "vendor");
const markerPath = path.join(vendorDir, "version.json");

function ensureBinary() {
  if (configuredBinary && fs.existsSync(binaryPath)) {
    assertBinaryVersion(binaryPath);
    return;
  }

  if (configuredBinary) {
    console.error(`Configured Phonton binary does not exist: ${binaryPath}`);
    process.exit(1);
  }

  if (fs.existsSync(binaryPath) && vendorMatchesPackage() && binaryVersionMatches(binaryPath).ok) {
    return;
  }

  fs.rmSync(vendorDir, { recursive: true, force: true });

  const installScript = process.env.PHONTON_INSTALL_SCRIPT || path.join(__dirname, "..", "install.js");
  const result = spawnSync(process.execPath, [installScript], { stdio: "inherit" });
  if (result.status !== 0 || !fs.existsSync(binaryPath)) {
    process.exit(result.status || 1);
  }
  assertBinaryVersion(binaryPath);
}

function vendorMatchesPackage() {
  try {
    const marker = JSON.parse(fs.readFileSync(markerPath, "utf8"));
    return (
      marker.version === packageJson.version &&
      marker.platform === process.platform &&
      marker.arch === process.arch
    );
  } catch (_error) {
    return false;
  }
}

function assertBinaryVersion(candidate) {
  const check = binaryVersionMatches(candidate);
  if (!check.ok) {
    console.error(`Installed Phonton binary is stale or invalid: expected ${check.expected}, got ${check.output}`);
    if (!configuredBinary) {
      fs.rmSync(vendorDir, { recursive: true, force: true });
    }
    process.exit(1);
  }
}

function binaryVersionMatches(candidate) {
  const result = spawnSync(candidate, ["version"], { encoding: "utf8" });
  const output = `${result.stdout || ""}${result.stderr || ""}`.trim() || "no output";
  const expected = `phonton ${packageJson.version}`;
  return {
    ok: result.status === 0 && output.includes(expected),
    expected,
    output,
  };
}

ensureBinary();

const child = spawn(binaryPath, process.argv.slice(2), { stdio: "inherit" });
child.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 0);
});
