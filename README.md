<p align="center">
  <img src="assets/readme/phonton-cli-logo.png" width="112" alt="Phonton CLI logo">
</p>

<h1 align="center">Phonton CLI - v0.19.0</h1>

<p align="center">
  <strong>Local-first agentic development with visible plans, verified diffs, and inspectable memory.</strong><br>
  Phonton is an agentic development environment (ADE) built around the loop: goal -> plan -> edit -> verify -> review -> remember.
</p>

<p align="center">
  <a href="https://github.com/phonton-dev/phonton-cli/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/phonton-dev/phonton-cli/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/phonton-dev/phonton-cli/stargazers"><img alt="GitHub stars" src="https://img.shields.io/github/stars/phonton-dev/phonton-cli?style=flat&label=stars"></a>
  <img alt="release" src="https://img.shields.io/badge/release-v0.19.0-6c63ff">
  <img alt="license" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue">
</p>

---

Phonton plans work before broad edits, routes workers through bounded repo context, verifies changed files before review, and records typed handoff evidence. The v0.19.0 release is an alpha slice of the next architecture: typed swarm planning, selectable code-index backends, and preview-only MCP capability negotiation.

<p align="center">
  <img src="assets/readme/phonton-cli-hero.png" alt="Phonton CLI hero with terminal UI preview">
</p>

## Quick Install

The easiest install path is npm. The package downloads a prebuilt GitHub Release binary during installation.

```bash
npm install -g phonton-cli
npx phonton-cli
```

Alternative installers:

```bash
curl -fsSL https://raw.githubusercontent.com/phonton-dev/phonton-cli/main/scripts/install.sh | sh
```

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/phonton-dev/phonton-cli/main/scripts/install.ps1)))
```

```bash
cargo install --git https://github.com/phonton-dev/phonton-cli --tag v0.19.0 phonton-cli --locked --force
```

Verify the install:

```bash
phonton version
phonton doctor
```

## ADE Loop

```mermaid
flowchart LR
    A["Goal"] --> B["Plan preview"]
    B --> C["Diff-only workers"]
    C --> D["Verification gate"]
    D --> E["Reviewable handoff"]
    E --> F["Memory and history"]
    F --> B
```

Phonton is designed for context efficiency and auditability. Public token, cost, or benchmark claims should be backed by reproducible fixture repos, exact prompts, raw logs, provider usage, final diffs, verification logs, and handoff evidence.

## What's New in v0.19.0

- Typed swarm plan metadata: `PlannerOutput` includes a `PlanGraph` sidecar with subtask role, expected touch scope, dependencies, conflict group, and verification intent.
- Broad-goal swarm activation: broad goals can use the configured provider-backed decomposer; when no provider is available, the plan preview records a deterministic fallback reason.
- Conflict-aware worker scheduling: overlapping file scopes are serialized by dependency insertion, while isolated subtasks still run through the existing concurrent executor.
- Code index backends: `phonton-index` now exposes a `CodeRetriever` abstraction with default `local-hnsw` retrieval and optional Qdrant HTTP retrieval for larger codebases.
- MCP capability preview: `phonton mcp capabilities <server-id> [--json] [--yes]` captures initialize metadata, tool descriptors, and proposed permission rules without calling tools or writing config.
- Evidence plumbing: plan preview JSON, flight-log events, context manifests, outcome ledgers, and doctor output now surface swarm, index, and MCP capability evidence where available.

## Core Commands

```bash
phonton
phonton goal "<engineering goal>" --yes
phonton plan --json "<engineering goal>"
phonton ask "<workspace question>"
phonton diff --stat
phonton doctor --provider
phonton review latest
phonton run latest
```

Extension and MCP inspection commands are read-only unless a tool call is explicitly approved:

```bash
phonton extensions list
phonton extensions doctor
phonton extensions skills
phonton extensions steering
phonton extensions mcp
phonton extensions profiles
phonton mcp list
phonton mcp capabilities <server-id> --yes
phonton mcp tools <server-id> --yes
phonton mcp call <server-id> <tool-name> '{"arg":"value"}' --yes
```

## Configuration

Create `~/.phonton/config.toml`:

```toml
[provider]
name = "deepseek"
model = "deepseek-v4-flash"

[provider.keys]
deepseek = "sk-deepseek-api-key-here"
anthropic = "sk-ant-api-key-here"

[index]
backend = "local-hnsw"
```

Optional Qdrant backend:

```toml
[index]
backend = "qdrant"
qdrant_url = "http://127.0.0.1:6333"
qdrant_collection = "phonton-code"
```

Phonton does not start or manage Qdrant containers. SQLite keyword memory remains the authoritative decision memory store; Qdrant is only used for code and symbol retrieval.

## Crate Map

- `phonton-cli`: Ratatui terminal UI, headless commands, doctor, plan preview, and release surfaces.
- `phonton-planner`: goal decomposition, memory-aware planning, and typed swarm plan metadata.
- `phonton-orchestrator`: task DAG execution, conflict-aware dependency normalization, verification, retries, and handoff.
- `phonton-worker`: diff-only worker call loops and context selection.
- `phonton-index`: local HNSW retrieval and optional Qdrant code retrieval.
- `phonton-mcp`: MCP manifest loading, approval-gated tool calls, and capability preview.
- `phonton-memory`: local SQLite memory for conventions, constraints, and decisions.
- `phonton-verify`: syntax, build, test, and browser/runtime verification gates.

## Release Channels

| Channel | Install command | Use case |
|---|---|---|
| Stable | `cargo install --git https://github.com/phonton-dev/phonton-cli --tag v0.19.0 phonton-cli --locked --force` | Current validated release |
| Dev | `cargo install --git https://github.com/phonton-dev/phonton-cli --branch dev phonton-cli --locked --force` | Upcoming release features |
| Nightly | `cargo install --git https://github.com/phonton-dev/phonton-cli --branch nightly phonton-cli --locked --force` | Daily automated snapshots |

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.

## Star History

[![Star History Chart](https://api.star-history.com/chart?repos=phonton-dev/phonton-cli&type=date&legend=top-left)](https://www.star-history.com/?repos=phonton-dev%2Fphonton-cli&type=date&legend=top-left)
