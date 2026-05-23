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

const result = spawnSync(process.execPath, [path.join(root, "npm", "bin", "phonton.js"), "version"], {
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

const expected = `phonton ${packageJson.version}`;
if (!result.stdout.includes(expected)) {
  console.error(`Expected npm wrapper to report ${expected}.`);
  process.exit(1);
}
