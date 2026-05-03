#!/usr/bin/env node

const fs = require("fs");
const path = require("path");
const { spawn, spawnSync } = require("child_process");

const binaryName = process.platform === "win32" ? "phonton.exe" : "phonton";
const configuredBinary = process.env.PHONTON_BINARY || process.env.PHONTON_CLI_BINARY;
const binaryPath = configuredBinary
  ? path.resolve(configuredBinary)
  : path.join(__dirname, "..", "vendor", binaryName);

function ensureBinary() {
  if (fs.existsSync(binaryPath)) {
    return;
  }

  if (configuredBinary) {
    console.error(`Configured Phonton binary does not exist: ${binaryPath}`);
    process.exit(1);
  }

  const installScript = path.join(__dirname, "..", "install.js");
  const result = spawnSync(process.execPath, [installScript], { stdio: "inherit" });
  if (result.status !== 0 || !fs.existsSync(binaryPath)) {
    process.exit(result.status || 1);
  }
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
