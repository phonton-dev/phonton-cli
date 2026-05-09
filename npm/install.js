#!/usr/bin/env node

const fs = require("fs");
const https = require("https");
const os = require("os");
const path = require("path");
const { spawnSync } = require("child_process");

const packageJson = require("../package.json");

const repo = "phonton-dev/phonton-cli";
const version = packageJson.version;
const tag = process.env.PHONTON_RELEASE_TAG || (version.includes("nightly") ? "nightly" : `v${version}`);
const vendorDir = path.join(__dirname, "vendor");
const markerPath = path.join(vendorDir, "version.json");

const targets = {
  "linux-x64": {
    asset: "phonton-x86_64-unknown-linux-gnu.tar.gz",
    binary: "phonton",
    extract: "tar",
  },
  "darwin-arm64": {
    asset: "phonton-aarch64-apple-darwin.tar.gz",
    binary: "phonton",
    extract: "tar",
  },
  "win32-x64": {
    asset: "phonton-x86_64-pc-windows-msvc.zip",
    binary: "phonton.exe",
    extract: "zip",
  },
};

const target = targets[`${process.platform}-${process.arch}`];
if (!target) {
  console.error(`Unsupported platform for prebuilt Phonton CLI: ${process.platform}-${process.arch}`);
  console.error("Install from source instead:");
  console.error(`cargo install --git https://github.com/${repo} --tag ${tag} phonton-cli --locked --force`);
  process.exit(1);
}

const url = `https://github.com/${repo}/releases/download/${tag}/${target.asset}`;
const archivePath = path.join(os.tmpdir(), target.asset);

fs.rmSync(vendorDir, { recursive: true, force: true });
fs.mkdirSync(vendorDir, { recursive: true });

download(url, archivePath)
  .then(() => extract(archivePath, target.extract))
  .then(() => {
    const binaryPath = path.join(vendorDir, target.binary);
    if (!fs.existsSync(binaryPath)) {
      throw new Error(`expected binary missing after install: ${binaryPath}`);
    }
    if (process.platform !== "win32") {
      fs.chmodSync(binaryPath, 0o755);
    }
    fs.writeFileSync(
      markerPath,
      `${JSON.stringify(
        {
          version,
          tag,
          asset: target.asset,
          platform: process.platform,
          arch: process.arch,
        },
        null,
        2,
      )}\n`,
    );
    console.log(`Installed Phonton CLI ${tag} for ${process.platform}-${process.arch}`);
  })
  .catch((error) => {
    console.error(error.message || error);
    process.exit(1);
  });

function download(from, to) {
  return new Promise((resolve, reject) => {
    const file = fs.createWriteStream(to);
    https
      .get(from, (response) => {
        if ([301, 302, 303, 307, 308].includes(response.statusCode)) {
          file.close();
          fs.rmSync(to, { force: true });
          download(response.headers.location, to).then(resolve, reject);
          return;
        }
        if (response.statusCode !== 200) {
          reject(new Error(`download failed (${response.statusCode}): ${from}`));
          return;
        }
        response.pipe(file);
        file.on("finish", () => {
          file.close(resolve);
        });
      })
      .on("error", (error) => {
        file.close();
        fs.rmSync(to, { force: true });
        reject(error);
      });
  });
}

function extract(archive, kind) {
  if (kind === "tar") {
    run("tar", ["-xzf", archive, "-C", vendorDir]);
    return;
  }

  const command = "powershell";
  const args = [
    "-NoProfile",
    "-ExecutionPolicy",
    "Bypass",
    "-Command",
    `Expand-Archive -LiteralPath '${archive.replace(/'/g, "''")}' -DestinationPath '${vendorDir.replace(/'/g, "''")}' -Force`,
  ];
  run(command, args);
}

function run(command, args) {
  const result = spawnSync(command, args, { stdio: "inherit" });
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(" ")} failed`);
  }
}
