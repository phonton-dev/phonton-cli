<p align="center">
  <img src="assets/readme/phonton-cli-logo.png" width="128" alt="Phonton CLI logo">
</p>

<h1 align="center">Phonton</h1>

<p align="center">
  <strong>The local-first ADE that proves its work.</strong><br>
  Give it a goal. It shows a plan, writes diffs, runs your checks, and hands you
  a receipt: what changed, what passed, and what it cost.
</p>

<p align="center">
  <a href="https://github.com/phonton-dev/phonton-cli/actions/workflows/ci.yml"><img alt="CI Status" src="https://github.com/phonton-dev/phonton-cli/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/phonton-dev/phonton-cli/stargazers"><img alt="GitHub stars" src="https://img.shields.io/github/stars/phonton-dev/phonton-cli?style=flat&label=stars&color=ff69b4"></a>
  <img alt="release" src="https://img.shields.io/badge/release-v0.22.1--beta-6c63ff">
  <img alt="license" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue">
</p>

<p align="center">
  <a href="https://phonton.dev">Website</a> ·
  <a href="docs/getting-started.md">Getting started</a> ·
  <a href="https://discord.gg/avcJb4PjhM">Discord</a>
</p>

<p align="center">
  <img src="assets/readme/phonton-cli-hero.png" alt="Phonton CLI terminal UI" width="800">
</p>

---

<p align="center">
  <img src="assets/readme/phonton-demo.gif" alt="Phonton fixing a failing parsePort test in the terminal UI" width="800"><br>
  <sub>Real recording (sped up): two failing tests in, a verified diff and receipt out.</sub>
</p>

## 60-second start

```bash
npm install -g phonton-cli
phonton doctor     # checks your provider key, git, and local tools
phonton            # opens the TUI; type a goal and press Enter
```

No account, no Phonton server. Bring a key from Anthropic, OpenAI, DeepSeek,
OpenRouter, Gemini, Groq and others, or run on local models (preview, below).

## What a run looks like

From the recorded run above (DeepSeek Flash, small JavaScript repo):

```text
goal     Make parsePort reject ports outside 1-65535 and trailing garbage
plan     1 subtask, verify with the repo's own `node --test`
edit     unified diff against the exact current src/port.js
verify   patch applies · syntax · tests: 3 pass, 0 fail (was 1 pass, 2 fail)
review   receipt: 1 file (+6/-2) · 8.8k tokens · est. $0.003 · checkpoint #1
remember completion stored locally for the next goal
```

Nothing lands in your tree until it passes verification, and every run leaves a
receipt you can audit with `phonton review latest` and `phonton why-tokens`.

## Why Phonton

- **Plan before edits.** Each goal becomes a visible `GoalContract` with
  acceptance criteria and a verify plan before any worker starts.
- **Diffs, not rewrites.** Workers return unified diffs against the current
  source. No prose, no whole-file regeneration.
- **Verification gates review.** Patch apply, syntax, decision memory, Cargo,
  Node and browser checks decide what is review-ready. Failures repair first,
  then escalate to a stronger model.
- **Receipts with real numbers.** Changed files, checks, provider-reported
  tokens, cost, and rollback points in a typed `HandoffPacket`.
- **Local memory.** Decisions, rejected approaches and conventions live in
  local SQLite and steer the next plan.
- **Designed for context efficiency.** Workers get only the files they edit
  plus retrieved symbols; finished subtasks are carried forward as one-line
  notes instead of full transcripts; tests and boilerplate go to your
  configured model, core logic starts one tier up, and only failures
  escalate further. We publish
  measurements only with reproducible artifacts; see
  [Benchmark honesty](#benchmark-honesty).

## Local models (preview)

Run on your own GPU with no key. Phonton measures what the model can do,
asks it only for edits in a format it handled during calibration, runs your
checks on a copy, and lands nothing until you apply the result.

```bash
phonton models catalog                      # models that fit this machine
phonton models setup qwen2.5-coder:3b       # runtime (Windows x64), download,
                                            # calibrate edit formats, select
phonton                                     # TUI: goals now run on the local model
phonton goal "Add a count() method to TodoStore" --yes --allow-host-checks
phonton goal --local apply RUN_ID --yes     # after reviewing the verified candidate
phonton goal --local rollback RUN_ID --yes  # restore the original files
```

With provider `ollama` and a calibrated model selected, both the TUI and
`phonton goal` use the local harness. Every goal pauses for plan approval,
local runs never fall back to a cloud model, and the receipt says whether the
runtime was verified as Phonton-managed.

On macOS and Linux, install [Ollama](https://ollama.com/download) yourself.
Phonton cannot verify a runtime it did not start (an Ollama install can relay
cloud models), so the TUI plan review says so and your approval is consent
for that goal; headless runs need `phonton goal --local ... --allow-unverified-runtime`.

In our smoke runs, qwen2.5-coder:3b on a 6 GB laptop GPU finished small
single-file edits in a Node repo in 20-30 s. Small models work best on one
focused change at a time. Details and limits:
[docs/local-harness.md](docs/local-harness.md).

---

## Install

```bash
npm install -g phonton-cli
```

From source:

```bash
cargo install --git https://github.com/phonton-dev/phonton-cli phonton-cli --locked --force
```

Script installers:

```bash
curl -fsSL https://raw.githubusercontent.com/phonton-dev/phonton-cli/main/scripts/install.sh | sh
```

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/phonton-dev/phonton-cli/main/scripts/install.ps1)))
```

---

## Commands

```bash
# Launch the interactive Ratatui TUI
phonton

# Run a goal non-interactively through plan/edit/verify/review
phonton goal "add input validation to config loading" --yes --allow-host-checks

# Run an exact prompt file, useful for benchmark and CI harnesses
phonton goal --prompt-file prompt.md --yes --allow-host-checks --json

# Preview the task graph and GoalContract without editing files
phonton plan --json "refactor auth layer"

# Audit configuration, provider key, store, trust, and the project's toolchains
phonton doctor --provider

# See where the latest goal spent tokens
phonton why-tokens

# Review the latest receipt, then export typed proof
phonton review latest
phonton proof export --latest --format json

# Inspect MCP capability proposals without invoking tools
phonton mcp capabilities <server-id> --json
```

---

## Configuration

Configure providers and the code index in `~/.phonton/config.toml`:
Set `PHONTON_CONFIG_PATH` to an absolute config path outside repositories for
an isolated CLI run. Local-model state stays at its usual location unless
`PHONTON_LOCAL_STATE` is also set. Provider keys can remain in process
environment variables instead of the isolated file; config-file keys take
precedence when present. The normal config is left untouched.

```toml
[provider]
name = "deepseek"
model = "deepseek-flash"

[provider.keys]
deepseek = "sk-deepseek-api-key-here"
anthropic = "sk-ant-api-key-here"
openai = "sk-proj-openai-key-here"

[index]
backend = "local-hnsw"
```

Optional Qdrant code retrieval:

```toml
[index]
backend = "qdrant"
qdrant_url = "http://127.0.0.1:6333"
qdrant_collection = "phonton-code"
```

---

## Benchmark Honesty

Phonton is designed for context efficiency and accountable verification, but
public comparisons require reproducible evidence: pinned fixtures, exact
prompts, tool versions, model/provider names, provider-reported token usage
where available, raw logs, final diffs, verification logs, quality review, and
handoff evidence.

`benchmarks/smoke` runs fixed goals on fresh copies of `fixtures/` and scores
them with hidden tests the run never sees; any build can be compared the same
way. Method, commands and claim rules: [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

---

## Crate Architecture

- `phonton-cli`: TUI, headless goal runner, benchmark export, and CLI commands.
- `phonton-types`: shared GoalContract, HandoffPacket, PlanGraph, events, and provider types.
- `phonton-planner`: goal decomposition, contract generation, and plan graph metadata.
- `phonton-orchestrator`: task scheduling, confidence gate, retries, verification, and handoff assembly.
- `phonton-worker`: context assembly, provider calls, diff-only output, repair prompts, and MCP flow.
- `phonton-index`: local HNSW symbol/code retrieval plus optional Qdrant retrieval.
- `phonton-verify`: patch, syntax, decision, Cargo, Node, and browser verification.
- `phonton-memory` and `phonton-store`: local memory facade and SQLite persistence.
- `phonton-sandbox`: command and tool execution guardrails.
- `phonton-extensions` and `phonton-mcp`: local extension loading and MCP runtime.
- `phonton-local`: hardware detection, managed Ollama runtime, local model install and calibration (preview).

---

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

At your option.

### TUI local plan approval

On a calibrated local route, every goal pauses at its proposed files, exact
check arguments, model, runtime origin and budget. Press
**Y** to approve that plan or **N / Esc** to cancel. Enter, paste and held-key
repeats do not approve a plan. This is separate from host-check permission;
plan approval never grants access to run project commands on the host.
Changes remain in a copy until a verified candidate is explicitly applied.

Local receipts and the run record include reported tokens from rejected and
interrupted strategy proposals as well as edit candidates. Only a verified
managed runtime counts as local inference; external or unobserved origins are
reported as unknown. The TUI records a local result only after its final receipt
has been saved.
