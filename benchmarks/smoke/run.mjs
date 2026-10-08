#!/usr/bin/env node
// Smoke benchmark: run each task on a fresh fixture copy, then score it with
// hidden tests the run never saw. Not a competitor comparison.
//
//   node benchmarks/smoke/run.mjs --arm cloud --runs 3
//   node benchmarks/smoke/run.mjs --arm local --runs 3 --tasks todo-count
//
// Uses the provider/keys from the environment (set PHONTON_HOME for an
// isolated profile). Raw logs and diffs go to benchmarks/results/ (ignored).
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { TASKS } from "./tasks.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "../..");
const args = Object.fromEntries(
  process.argv.slice(2).reduce((acc, a, i, all) => {
    if (a.startsWith("--")) acc.push([a.slice(2), all[i + 1]]);
    return acc;
  }, [])
);
const arm = args.arm ?? "cloud";
const runs = Number(args.runs ?? 1);
const exe = process.platform === "win32" ? "phonton.exe" : "phonton";
const bin = path.resolve(args.bin ?? path.join(repo, "target", "release", exe));
const only = args.tasks ? new Set(args.tasks.split(",")) : null;
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const out = path.resolve(args.out ?? path.join(repo, "benchmarks", "results", `smoke-${arm}-${stamp}`));
if (!["cloud", "local"].includes(arm)) throw new Error("--arm must be cloud or local");
if (!fs.existsSync(bin)) throw new Error(`phonton binary not found: ${bin} (pass --bin)`);
fs.mkdirSync(out, { recursive: true });

const run = (cmd, argv, cwd, timeoutS = 1200) => {
  const started = Date.now();
  const r = spawnSync(cmd, argv, { cwd, encoding: "utf8", timeout: timeoutS * 1000, maxBuffer: 64 << 20 });
  return { code: r.status, stdout: r.stdout ?? "", stderr: (r.stderr ?? "") + (r.error ? `\n${r.error}` : ""), seconds: (Date.now() - started) / 1000 };
};
const git = (cwd, ...a) => run("git", ["-c", "user.name=bench", "-c", "user.email=bench@localhost", ...a], cwd);

function freshFixture(name) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `phonton-smoke-${name}-`));
  fs.cpSync(path.join(repo, "fixtures", name), dir, { recursive: true });
  git(dir, "init", "-q");
  git(dir, "add", "-A");
  git(dir, "commit", "-qm", "fixture");
  return dir;
}

function cloudRun(dir, goal) {
  const r = run(bin, ["goal", "--yes", "--allow-host-checks", "--json", goal], dir);
  let doc = {};
  try {
    doc = JSON.parse(r.stdout);
  } catch {}
  const status = typeof doc.status === "string" ? doc.status : Object.keys(doc.status ?? { Unknown: 0 })[0];
  const receipt = doc.cost_receipt ?? {};
  return {
    r,
    status,
    tokens: doc.tokens_used ?? null,
    cost_usd: receipt.pricing_known ? (receipt.actual_usd_micros ?? 0) / 1e6 : null,
  };
}

function localRun(dir, goal) {
  const r = run(bin, ["goal", "--local", goal, "--yes", "--allow-host-checks"], dir);
  const id = (r.stdout.match(/--local (?:apply|show) ([0-9a-f-]{36})/) ?? [])[1];
  let status = "failed";
  let tokens = null;
  if (id) {
    const shown = run(bin, ["goal", "--local", "show", id], dir);
    try {
      const ev = JSON.parse(shown.stdout).evidence ?? {};
      status = ev.state ?? status;
      tokens = (ev.candidates ?? []).reduce((n, c) => n + (c.input_tokens ?? 0) + (c.output_tokens ?? 0), 0);
    } catch {}
    if (status === "review_ready") {
      const applied = run(bin, ["goal", "--local", "apply", id, "--yes"], dir);
      r.stdout += `\n--- apply ---\n${applied.stdout}`;
      r.stderr += `\n--- apply ---\n${applied.stderr}`;
    }
  }
  return { r, status, tokens, cost_usd: 0 };
}

const rows = [];
for (const task of TASKS.filter((t) => !only || only.has(t.id))) {
  for (let n = 1; n <= runs; n++) {
    const dir = freshFixture(task.fixture);
    const res = arm === "cloud" ? cloudRun(dir, task.goal) : localRun(dir, task.goal);
    const diff = git(dir, "diff", "HEAD").stdout;
    for (const [file, body] of Object.entries(task.hidden)) {
      fs.writeFileSync(path.join(dir, file), body);
    }
    // Bare --test: Node 22 reads "test/" as a file, not a directory.
    const accept = run("node", ["--test"], dir, 300);
    const row = {
      task: task.id,
      arm,
      run: n,
      status: res.status,
      accepted: accept.code === 0,
      seconds: res.r.seconds,
      tokens: res.tokens,
      cost_usd: res.cost_usd,
      changed: diff.length > 0,
    };
    rows.push(row);
    const logDir = path.join(out, task.id, `${arm}-${n}`);
    fs.mkdirSync(logDir, { recursive: true });
    fs.writeFileSync(path.join(logDir, "stdout.txt"), res.r.stdout);
    fs.writeFileSync(path.join(logDir, "stderr.txt"), res.r.stderr);
    fs.writeFileSync(path.join(logDir, "diff.patch"), diff);
    fs.writeFileSync(path.join(logDir, "acceptance.txt"), accept.stdout + accept.stderr);
    fs.appendFileSync(path.join(out, "results.jsonl"), JSON.stringify(row) + "\n");
    console.log(JSON.stringify(row));
    try {
      fs.rmSync(dir, { recursive: true, force: true });
    } catch {
      // A runtime or test process can still hold the copy on Windows.
    }
  }
}

const median = (xs) => {
  const v = xs.filter((x) => typeof x === "number").sort((a, b) => a - b);
  return v.length ? v[Math.floor((v.length - 1) / 2)] : null;
};
const lines = [
  `# Smoke benchmark (${arm})`,
  "",
  `phonton: \`${run(bin, ["version"], repo).stdout.trim()}\`, ${runs} run(s) per task, ${new Date().toISOString()}`,
  "",
  "| Task | Accepted | Median s | Median tokens | Median USD |",
  "| --- | ---: | ---: | ---: | ---: |",
];
for (const id of [...new Set(rows.map((r) => r.task))]) {
  const rs = rows.filter((r) => r.task === id);
  const usd = median(rs.map((r) => r.cost_usd));
  lines.push(
    `| ${id} | ${rs.filter((r) => r.accepted).length}/${rs.length} | ${median(rs.map((r) => r.seconds))?.toFixed(0)} | ${median(rs.map((r) => r.tokens)) ?? "n/a"} | ${usd === null ? "unpriced" : usd.toFixed(4)} |`
  );
}
fs.writeFileSync(path.join(out, "summary.md"), lines.join("\n") + "\n");
console.log(lines.join("\n"));
console.log(`\nlogs: ${out}`);
