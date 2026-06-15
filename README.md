<p align="center">
  <img src="assets/readme/phonton-cli-logo.png" width="128" alt="Phonton CLI logo">
</p>

<h1 align="center">Phonton CLI - v0.20.1</h1>

<p align="center">
  <strong>A local-first ADE for verified, accountable code changes.</strong><br>
  Phonton turns a goal into a visible plan, diff-only work, layered verification,
  reviewable receipts, and inspectable memory.
</p>

<p align="center">
  <a href="https://github.com/phonton-dev/phonton-cli/actions/workflows/ci.yml"><img alt="CI Status" src="https://github.com/phonton-dev/phonton-cli/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/phonton-dev/phonton-cli/stargazers"><img alt="GitHub stars" src="https://img.shields.io/github/stars/phonton-dev/phonton-cli?style=flat&label=stars&color=ff69b4"></a>
  <img alt="release" src="https://img.shields.io/badge/release-v0.20.1-6c63ff">
  <img alt="license" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue">
</p>

---

## Quick Start

```powershell
npm install -g phonton-cli@0.20.1
phonton doctor
phonton config edit   # add your provider API key
phonton goal "fix the failing npm test in this repo"
```

Headless benchmark-style runs:

```powershell
phonton goal --prompt-file prompt.md --yes --json --permission-mode full-access
phonton review latest --json
phonton benchmark export --latest
```

When a run finishes, open the **Receipt** focus in the TUI (or `phonton review
latest`) for changed files, verification evidence, run commands, known gaps,
and rollback points. Use `/why-tokens` to see index, memory, and attachment
contributions.

---

## What Is Phonton?

Phonton CLI is a local-first agentic development environment (ADE), not a
generic chatbot. It is built around the accountable development loop:

```text
goal -> plan -> edit -> verify -> review -> remember
```

You bring your own model keys or local model runtime. Phonton runs locally,
keeps its state in local files and SQLite, and sends selected task context only
to the provider or local model you configure. There is no Phonton-hosted proxy
between your workspace and your chosen provider.

<p align="center">
  <img src="assets/readme/phonton-cli-hero.png" alt="Phonton CLI terminal UI preview" width="800">
</p>

---

## Why Phonton?

### Visible Goal Contracts

Before broad work starts, Phonton turns the request into a `GoalContract` with
acceptance criteria, expected artifacts, likely files, verification commands,
assumptions, and clarification questions.

### Interactive Clarification Questionnaire

v0.19.6 integrates a fully Interactive Clarification Questionnaire inside the TUI. When requirements are under-specified (confidence < 70% or unanswered questions), execution suspends, guiding the user step-by-step directly in the terminal, automatically appending answers to the prompt, and initiating a clean planning rerun.

### Diff-Only Workers

Workers produce code changes as diffs. Phonton does not treat worker prose as
the primary artifact, and unverified changes are not promoted as review-ready.

### Layered Verification

Phonton verifies changes with the checks that fit the workspace: patch
applicability, syntax checks, memory/decision checks, Cargo checks and tests,
Node test scripts, and browser rendering checks for web projects when
applicable.

### Typed Handoff Packets

After verification, Phonton writes a typed `HandoffPacket` with changed files,
verification evidence, run commands, known gaps, rollback points, token/cost
summary, and context influence. Review starts from evidence rather than a chat
summary.

### Local Memory And Code Retrieval

Phonton stores task history, decisions, rejected approaches, and conventions in
local SQLite. Code context is retrieved through local symbol indexing and HNSW
search by default, with an optional Qdrant backend for code retrieval in larger
workspaces.

### BYOK Providers And MCP Approval Gates

Phonton supports Anthropic, OpenAI, OpenRouter, Gemini, Ollama, AgentRouter,
Cloudflare, DeepSeek, xAI/Grok, Groq, Together, and custom OpenAI-compatible
endpoints. MCP servers and extension packs are inspectable local config, and
networked or mutating tool use goes through approval-aware flows.

---

## Quick Install

Install from npm:

```bash
npm install -g phonton-cli
phonton version
phonton doctor
```

Install this exact release from source:

```bash
cargo install --git https://github.com/phonton-dev/phonton-cli --tag v0.19.6 phonton-cli --locked --force
```

Alternative installers:

```bash
curl -fsSL https://raw.githubusercontent.com/phonton-dev/phonton-cli/main/scripts/install.sh | sh
```

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/phonton-dev/phonton-cli/main/scripts/install.ps1)))
```

---

## v0.19.6 Highlights

- Beautiful, guided Interactive Clarification step questionnaire (`Mode::Clarify`) directly inside the Ratatui TUI.
- Automatic prompt self-refinement by appending answers to the original goal description.
- Programmatic plan-rerun queueing with strict state, flight log, and checkpoint cleanup to prevent state leaks.
- Full verification coverage via new automated TUI unit testing.

Recent v0.19.x work also includes typed swarm planning metadata, conflict-group
scheduling, pluggable local/Qdrant code retrieval, MCP capability previews,
browser verifier cleanup, TUI version display, and auto-update controls.

---

## Commands

```bash
# Launch the interactive Ratatui TUI
phonton

# Run a goal non-interactively through plan/edit/verify/review
phonton goal "add input validation to config loading" --yes

# Run an exact prompt file, useful for benchmark and CI harnesses
phonton goal --prompt-file prompt.md --yes --permission-mode full-access --json

# Preview the task graph and GoalContract without editing files
phonton plan --json "refactor auth layer"

# Audit configuration, providers, store, trust, git, Cargo, and index backend
phonton doctor --provider

# Export evidence from the latest run
phonton benchmark export --latest --format json

# Inspect MCP capability proposals without invoking tools
phonton mcp capabilities <server-id> --json
```

---

## Configuration

Configure providers and the code index in `~/.phonton/config.toml`:

```toml
[provider]
name = "deepseek"
model = "deepseek-v4-flash"

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

Do not treat local-template runs, estimates, or incomplete artifact sets as
token-efficiency wins.

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

---

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

At your option.
