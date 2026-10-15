#!/usr/bin/env node
// Aider Polyglot slice: the same exercises, prompt and local model for
// Phonton, the bare model and a rival harness (OpenCode). Scores pass rate
// and false-pass rate: the harness said it succeeded, the pristine tests fail.
//
//   node benchmarks/polyglot/run.mjs --polyglot ../polyglot-benchmark \
//     --model qwen3.5:9b --arms phonton,bare,opencode --per-lang 17
//
// See README.md in this folder for the protocol and every flag.
import { spawnSync } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "../..");
const args = Object.fromEntries(
  process.argv.slice(2).reduce((acc, a, i, all) => {
    if (a.startsWith("--")) acc.push([a.slice(2), all[i + 1]?.startsWith("--") ? "true" : all[i + 1] ?? "true"]);
    return acc;
  }, [])
);
const need = (k) => args[k] ?? (() => { throw new Error(`--${k} is required (see README.md)`); })();
const polyglot = path.resolve(need("polyglot"));
const model = need("model");
const endpoint = (args.endpoint ?? "http://127.0.0.1:11434/v1").replace(/\/$/, "");
const langs = (args.langs ?? "python,javascript,rust").split(",");
const arms = (args.arms ?? "phonton,bare,opencode").split(",");
const perLang = Number(args["per-lang"] ?? 2);
const seed = args.seed ?? "phonton-polyglot-1";
const testsMode = args.tests ?? "visible";
const bareTries = Number(args["bare-tries"] ?? (testsMode === "visible" ? 2 : 1));
const timeoutS = Number(args["timeout-seconds"] ?? 900);
const exe = process.platform === "win32" ? "phonton.exe" : "phonton";
const bin = path.resolve(args.bin ?? path.join(repo, "target", "release", exe));
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const out = path.resolve(args.out ?? path.join(repo, "benchmarks", "results", `polyglot-${stamp}`));
// Test dependencies and harness state, reused across runs; never artifacts.
const cache = path.resolve(args.cache ?? path.join(repo, "benchmarks", "results", ".polyglot-cache"));
for (const a of arms) if (!["phonton", "bare", "opencode"].includes(a)) throw new Error(`unknown arm ${a}`);
if (!["visible", "hidden"].includes(testsMode)) throw new Error("--tests must be visible or hidden");
if (arms.includes("phonton") && !fs.existsSync(bin)) throw new Error(`phonton binary not found: ${bin} (pass --bin)`);
fs.mkdirSync(out, { recursive: true });

// --- process helpers ---------------------------------------------------------

const run = (cmd, argv, opts = {}) => {
  const started = Date.now();
  const r = spawnSync(cmd, argv, {
    encoding: "utf8",
    timeout: (opts.timeoutS ?? timeoutS) * 1000,
    maxBuffer: 64 << 20,
    windowsHide: true,
    ...opts,
  });
  const timedOut = r.error?.code === "ETIMEDOUT";
  return {
    code: r.status,
    timedOut,
    stdout: r.stdout ?? "",
    stderr: (r.stderr ?? "") + (r.error ? `\n${r.error}` : ""),
    seconds: (Date.now() - started) / 1000,
  };
};
const git = (cwd, ...a) => run("git", ["-c", "user.name=bench", "-c", "user.email=bench@localhost", "-c", "core.autocrlf=false", ...a], { cwd });
const tail = (s, n = 4000) => (s.length > n ? s.slice(-n) : s);

// --- exercises ---------------------------------------------------------------

// Aider's benchmark prompts (aider/benchmark/prompts.py), verbatim.
const addendum = (files) => `
####

Use the above instructions to modify the supplied files: ${files.join(" ")}
Don't change the names of existing functions or classes, as they may be referenced from other code like unit tests, etc.
Only use standard libraries, don't suggest installing any packages.
`;
const testFailures = (files) => `
####

See the testing errors above.
The tests are correct, don't try and change them.
Fix the code in ${files.join(" ")} to resolve the errors.
`;

function exercises(lang) {
  const dir = path.join(polyglot, lang, "exercises", "practice");
  const only = args.exercises ? new Set(args.exercises.split(",")) : null;
  // Seeded hash order: a reproducible slice that is not alphabetical.
  const rank = (name) => crypto.createHash("sha256").update(`${seed}:${lang}:${name}`).digest("hex");
  return fs
    .readdirSync(dir)
    .filter((name) => !only || only.has(name))
    .sort((a, b) => rank(a).localeCompare(rank(b)))
    .slice(0, only ? undefined : perLang)
    .map((name) => {
      const src = path.join(dir, name);
      const meta = JSON.parse(fs.readFileSync(path.join(src, ".meta", "config.json"), "utf8"));
      const docs = ["instructions.md", "instructions.append.md"]
        .map((f) => path.join(src, ".docs", f))
        .filter((f) => fs.existsSync(f))
        .map((f) => fs.readFileSync(f, "utf8"))
        .join("\n");
      return { lang, name, src, solution: meta.files.solution, tests: meta.files.test, prompt: docs + addendum(meta.files.solution) };
    });
}

// Exercism ships most JS tests skipped; Aider's harness runs all of them.
const unskip = (text) =>
  text.replace(/\bxtest\(/g, "test(").replace(/\bxit\(/g, "it(").replace(/\b(test|it|describe)\.skip\(/g, "$1(").replace(/\bxdescribe\(/g, "describe(");

function pristineTests(ex) {
  return ex.tests.map((file) => {
    let body = fs.readFileSync(path.join(ex.src, file), "utf8");
    if (ex.lang === "javascript") body = unskip(body);
    return [file, body];
  });
}

const jsDeps = path.join(cache, "js");
function ensureJsDeps(src) {
  if (fs.existsSync(path.join(jsDeps, "node_modules", "jest"))) return;
  fs.mkdirSync(jsDeps, { recursive: true });
  const pkg = JSON.parse(fs.readFileSync(path.join(src, "package.json"), "utf8"));
  fs.writeFileSync(path.join(jsDeps, "package.json"), JSON.stringify({ private: true, devDependencies: pkg.devDependencies }, null, 2));
  const npm = run(process.platform === "win32" ? "npm.cmd" : "npm", ["install", "--no-audit", "--no-fund", "--loglevel=error"], { cwd: jsDeps, shell: process.platform === "win32", timeoutS: 900 });
  if (npm.code !== 0) throw new Error(`npm install for JS test deps failed:\n${tail(npm.stderr)}`);
}

function workspace(ex) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `polyglot-${ex.lang}-${ex.name}-`));
  // .meta holds the reference solution; it never reaches a harness.
  fs.cpSync(ex.src, dir, { recursive: true, filter: (p) => path.basename(p) !== ".meta" });
  for (const [file, body] of pristineTests(ex)) {
    if (testsMode === "hidden") fs.rmSync(path.join(dir, file), { force: true });
    else fs.writeFileSync(path.join(dir, file), body);
  }
  fs.writeFileSync(path.join(dir, ".gitignore"), "node_modules/\ntarget/\n__pycache__/\n");
  if (ex.lang === "javascript") {
    ensureJsDeps(ex.src);
    fs.symlinkSync(path.join(jsDeps, "node_modules"), path.join(dir, "node_modules"), "junction");
  }
  git(dir, "init", "-q");
  git(dir, "add", "-A");
  git(dir, "commit", "-qm", "exercise");
  return dir;
}

const cargoTarget = path.join(cache, "cargo-target");
// `root` is "." for a check Phonton runs in its own copy of the workspace.
function testCommand(ex, root) {
  switch (ex.lang) {
    case "python":
      // --pattern, not -p: Phonton's check guard reads a bare -p as inline code.
      return ["python", ["-m", "unittest", "discover", "-s", ".", "--pattern", "*_test.py"], {}];
    case "javascript":
      return [process.execPath, [path.join(jsDeps, "node_modules", "jest", "bin", "jest.js"), "--ci", "--rootDir", root], {}];
    case "rust":
      return ["cargo", ["test", "--quiet", "--", "--include-ignored"], { env: { ...process.env, CARGO_TARGET_DIR: cargoTarget } }];
    default:
      throw new Error(`no test command for ${ex.lang} yet`);
  }
}
const runTests = (ex, dir) => {
  const [cmd, argv, opts] = testCommand(ex, dir);
  return run(cmd, argv, { cwd: dir, timeoutS: 600, ...opts });
};

// --- arms --------------------------------------------------------------------

// Phonton: the local harness with the calibrated model in PHONTON_HOME.
function phontonArm(ex, dir, logDir) {
  const promptFile = path.join(logDir, "prompt.md");
  const unverified = args.runtime === "unverified" ? ["--allow-unverified-runtime"] : [];
  // The same files and test command the other arms get from the prompt and
  // the scorer; with hidden tests Phonton gets no check either. Nor does it
  // for JavaScript: its isolated candidate copy leaves out node_modules and
  // its offline `npm ci` takes at most 512 locked packages (jest + babel lock
  // 580), so it cannot run jest yet. Those runs end unverified and never claim.
  const [cmd, argv] = testCommand(ex, ".");
  const check = testsMode === "visible" && ex.lang !== "javascript" ? ["--check", JSON.stringify([cmd, ...argv])] : [];
  const scope = ["--files", ex.solution.join(",")];
  const r = run(bin, ["goal", "--local", ex.prompt, ...scope, ...check, "--yes", "--allow-host-checks", ...unverified], { cwd: dir });
  fs.writeFileSync(promptFile, ex.prompt);
  const id = (r.stdout.match(/--local (?:apply|show) ([0-9a-f-]{36})/) ?? [])[1];
  let status = r.timedOut ? "timeout" : "failed";
  let tokensIn = null;
  let tokensOut = null;
  if (id) {
    const shown = run(bin, ["goal", "--local", "show", id], { cwd: dir });
    fs.writeFileSync(path.join(logDir, "evidence.json"), shown.stdout);
    try {
      const ev = JSON.parse(shown.stdout).evidence ?? {};
      status = ev.state ?? status;
      tokensIn = (ev.candidates ?? []).reduce((n, c) => n + (c.input_tokens ?? 0), 0);
      tokensOut = (ev.candidates ?? []).reduce((n, c) => n + (c.output_tokens ?? 0), 0);
    } catch {}
  }
  // Phonton claims success only for a verified, review-ready candidate.
  const claimed = status === "review_ready";
  if (claimed) {
    const applied = run(bin, ["goal", "--local", "apply", id, "--yes"], { cwd: dir });
    r.stdout += `\n--- apply ---\n${applied.stdout}`;
    r.stderr += `\n--- apply ---\n${applied.stderr}`;
  }
  return { r, status, claimed, tokensIn, tokensOut };
}

// Bare model: one chat call per try, whole files back, test output fed back
// once (Aider's two-try protocol). It has no checks, so any written answer
// counts as a claim.
async function bareArm(ex, dir) {
  const system =
    "You are an expert programmer. Reply with the complete new contents of every file you change. " +
    "For each file write its path on its own line, then the whole file in one fenced code block. No other prose.";
  const files = () => ex.solution.map((f) => `${f}\n\`\`\`\n${fs.readFileSync(path.join(dir, f), "utf8")}\`\`\``).join("\n\n");
  const messages = [{ role: "system", content: system }, { role: "user", content: `${files()}\n\n${ex.prompt}` }];
  let tokensIn = 0;
  let tokensOut = 0;
  let written = 0;
  let log = "";
  const started = Date.now();
  const chat = () =>
    fetch(`${endpoint}/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: "Bearer local" },
      body: JSON.stringify({ model, messages, temperature: 0, stream: false }),
      signal: AbortSignal.timeout(timeoutS * 1000),
    });
  for (let attempt = 1; attempt <= bareTries; attempt++) {
    // One retry for a keep-alive socket the server closed while tests ran.
    const res = await chat()
      .catch(chat)
      .catch((e) => ({ ok: false, statusText: `${e} ${e.cause ?? ""}`, json: async () => ({}) }));
    const body = await res.json().catch(() => ({}));
    if (!res.ok) {
      log += `attempt ${attempt}: HTTP error ${res.status ?? ""} ${res.statusText}\n${JSON.stringify(body)}\n`;
      break;
    }
    tokensIn += body.usage?.prompt_tokens ?? 0;
    tokensOut += body.usage?.completion_tokens ?? 0;
    const reply = (body.choices?.[0]?.message?.content ?? "").replace(/<think>[\s\S]*?<\/think>/g, "");
    log += `--- attempt ${attempt} reply ---\n${reply}\n`;
    messages.push({ role: "assistant", content: reply });
    written = 0;
    for (const file of ex.solution) {
      const escaped = file.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
      // As in Aider's whole-file format, an unnamed block goes to the only file.
      const m =
        reply.match(new RegExp(`${escaped}\\W*\\n\`\`\`[^\\n]*\\n([\\s\\S]*?)\`\`\``)) ??
        (ex.solution.length === 1 ? reply.match(/```[^\n]*\n([\s\S]*?)```/) : null);
      if (m) {
        fs.writeFileSync(path.join(dir, file), m[1]);
        written++;
      }
    }
    if (attempt === bareTries || testsMode === "hidden") break;
    const t = runTests(ex, dir);
    if (t.code === 0) break;
    messages.push({ role: "user", content: `${tail(t.stdout + t.stderr)}\n${testFailures(ex.solution)}` });
  }
  const r = { code: 0, stdout: log, stderr: "", seconds: (Date.now() - started) / 1000 };
  return { r, status: written ? "answered" : "no_files", claimed: written > 0, tokensIn, tokensOut };
}

// OpenCode, isolated config, same Ollama endpoint and model.
function opencodeEntry() {
  if (args.opencode) return args.opencode;
  const root = run(process.platform === "win32" ? "npm.cmd" : "npm", ["root", "-g"], { shell: process.platform === "win32", timeoutS: 60 }).stdout.trim();
  return path.join(root, "opencode-ai", "bin", "opencode");
}
function opencodeArm(ex, dir, logDir) {
  const home = path.join(cache, "opencode");
  const config = path.join(home, "opencode.json");
  fs.mkdirSync(home, { recursive: true });
  fs.writeFileSync(
    config,
    JSON.stringify(
      {
        $schema: "https://opencode.ai/config.json",
        provider: { ollama: { npm: "@ai-sdk/openai-compatible", name: "Ollama", options: { baseURL: endpoint }, models: { [model]: { name: model } } } },
        model: `ollama/${model}`,
        autoupdate: false,
        share: "disabled",
      },
      null,
      2
    )
  );
  // Keep the user's own instructions out: OpenCode otherwise reads
  // ~/.claude/CLAUDE.md and global skills into the system prompt.
  const env = {
    ...process.env,
    OPENCODE_CONFIG: config,
    XDG_CONFIG_HOME: path.join(home, "config"),
    XDG_DATA_HOME: path.join(home, "data"),
    OPENCODE_DISABLE_CLAUDE_CODE: "1",
    OPENCODE_DISABLE_EXTERNAL_SKILLS: "1",
    OPENCODE_DISABLE_AUTOUPDATE: "1",
    OPENCODE_DISABLE_MODELS_FETCH: "1",
  };
  const r = run(process.execPath, [opencodeEntry(), "run", "--pure", "--format", "json", "--dangerously-skip-permissions", "--model", `ollama/${model}`, ex.prompt], { cwd: dir, env });
  let tokensIn = 0;
  let tokensOut = 0;
  for (const line of r.stdout.split("\n")) {
    try {
      const t = JSON.parse(line)?.part?.tokens;
      if (t) {
        tokensIn += (t.input ?? 0) + (t.cache?.read ?? 0);
        tokensOut += (t.output ?? 0) + (t.reasoning ?? 0);
      }
    } catch {}
  }
  fs.writeFileSync(path.join(logDir, "opencode-config.json"), fs.readFileSync(config));
  // OpenCode has no verdict of its own: finishing without an error is its
  // claim that the task is done.
  const claimed = r.code === 0 && !r.timedOut;
  return { r, status: r.timedOut ? "timeout" : r.code === 0 ? "finished" : `exit_${r.code}`, claimed, tokensIn, tokensOut };
}

// --- main --------------------------------------------------------------------

const version = (cmd, argv) => run(cmd, argv, { timeoutS: 60, shell: false }).stdout.trim().split("\n")[0];
const meta = {
  date: new Date().toISOString(),
  polyglot_commit: git(polyglot, "rev-parse", "HEAD").stdout.trim(),
  model,
  endpoint,
  runtime_version: await fetch(endpoint.replace(/\/v1$/, "") + "/api/version").then((r) => r.json()).then((j) => j.version).catch(() => "unknown"),
  phonton: arms.includes("phonton") ? version(bin, ["version"]) : null,
  opencode: arms.includes("opencode") ? version(process.execPath, [opencodeEntry(), "--version"]) : null,
  node: process.version,
  os: `${os.type()} ${os.release()} ${os.arch()}`,
  cpu: os.cpus()[0]?.model,
  ram_gb: Math.round(os.totalmem() / 1e9),
  tests: testsMode,
  bare_tries: bareTries,
  seed,
  argv: process.argv.slice(2),
};
fs.writeFileSync(path.join(out, "meta.json"), JSON.stringify(meta, null, 2));
console.log(JSON.stringify(meta));

const rows = [];
for (const ex of langs.flatMap(exercises)) {
  for (const arm of arms) {
    const logDir = path.join(out, ex.lang, ex.name, arm);
    fs.mkdirSync(logDir, { recursive: true });
    fs.writeFileSync(path.join(logDir, "prompt.md"), ex.prompt);
    const dir = workspace(ex);
    // Every arm starts cold. A model left resident also hides the GPU from
    // Phonton's memory admission. Non-Ollama endpoints ignore this.
    await fetch(`${endpoint.replace(/\/v1\/?$/, "")}/api/generate`, {
      method: "POST",
      body: JSON.stringify({ model, keep_alive: 0 }),
    }).catch(() => {});
    const res = arm === "phonton" ? phontonArm(ex, dir, logDir) : arm === "bare" ? await bareArm(ex, dir) : opencodeArm(ex, dir, logDir);
    git(dir, "add", "-A");
    const diff = git(dir, "diff", "--cached", "HEAD").stdout;
    const touched = git(dir, "diff", "--cached", "--name-only", "HEAD").stdout.split("\n").filter(Boolean);
    // Score against the pristine tests: a harness that edits or deletes them
    // gains nothing.
    for (const [file, body] of pristineTests(ex)) {
      fs.mkdirSync(path.dirname(path.join(dir, file)), { recursive: true });
      fs.writeFileSync(path.join(dir, file), body);
    }
    const tests = runTests(ex, dir);
    const passed = tests.code === 0;
    const row = {
      lang: ex.lang,
      exercise: ex.name,
      arm,
      status: res.status,
      claimed: res.claimed,
      passed,
      false_pass: res.claimed && !passed,
      tests_touched: touched.some((f) => ex.tests.includes(f)),
      tokens_in: res.tokensIn,
      tokens_out: res.tokensOut,
      seconds: Math.round(res.r.seconds * 10) / 10,
      changed: touched.filter((f) => !f.startsWith("node_modules")),
    };
    rows.push(row);
    fs.writeFileSync(path.join(logDir, "stdout.txt"), res.r.stdout);
    fs.writeFileSync(path.join(logDir, "stderr.txt"), res.r.stderr);
    fs.writeFileSync(path.join(logDir, "diff.patch"), diff);
    fs.writeFileSync(path.join(logDir, "tests.txt"), tests.stdout + tests.stderr);
    fs.appendFileSync(path.join(out, "results.jsonl"), JSON.stringify(row) + "\n");
    console.log(JSON.stringify(row));
    try {
      fs.rmSync(dir, { recursive: true, force: true });
    } catch {
      // A runtime or test process can still hold the copy on Windows.
    }
  }
}

// --- summary -----------------------------------------------------------------

const median = (xs) => {
  const v = xs.filter((x) => typeof x === "number").sort((a, b) => a - b);
  return v.length ? v[Math.floor((v.length - 1) / 2)] : null;
};
const pct = (a, b) => (b ? `${((100 * a) / b).toFixed(1)}%` : "n/a");
const lines = [
  `# Aider Polyglot slice`,
  "",
  `Model \`${model}\` via ${endpoint} (runtime ${meta.runtime_version}); polyglot-benchmark \`${meta.polyglot_commit.slice(0, 7)}\`; tests ${testsMode}; ${meta.date}.`,
  `phonton: \`${meta.phonton ?? "-"}\`; opencode: \`${meta.opencode ?? "-"}\`; ${meta.os}, ${meta.ram_gb} GB RAM.`,
  "",
  "| Arm | Lang | n | Pass | Claimed | False passes | False-pass rate | Tests touched | Median tokens | Median s |",
  "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
];
for (const arm of arms) {
  for (const lang of [...langs, "all"]) {
    const rs = rows.filter((r) => r.arm === arm && (lang === "all" || r.lang === lang));
    if (!rs.length) continue;
    const pass = rs.filter((r) => r.passed).length;
    const claimed = rs.filter((r) => r.claimed).length;
    const fp = rs.filter((r) => r.false_pass).length;
    // Runs that never reached the model (refused, crashed) report no tokens.
    const tok = median(rs.filter((r) => r.tokens_in || r.tokens_out).map((r) => r.tokens_in + r.tokens_out));
    lines.push(
      `| ${arm} | ${lang} | ${rs.length} | ${pass} (${pct(pass, rs.length)}) | ${claimed} | ${fp} | ${pct(fp, claimed)} | ${rs.filter((r) => r.tests_touched).length} | ${tok ?? "n/a"} | ${median(rs.map((r) => r.seconds))} |`
    );
  }
}
lines.push("", "False-pass rate = claimed successes whose pristine tests fail, over claimed successes.");
fs.writeFileSync(path.join(out, "summary.md"), lines.join("\n") + "\n");
console.log(lines.join("\n"));
console.log(`\nlogs: ${out}`);
