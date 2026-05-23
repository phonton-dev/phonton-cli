<p align="center">
  <img src="assets/readme/phonton-cli-logo.png" width="112" alt="Phonton CLI logo">
</p>

<h1 align="center">Phonton CLI · v0.17.1</h1>

<p align="center">
  <strong>Verified code changes with local repo memory.</strong><br>
  A local-first agentic development environment (ADE) for developers who want autonomous code changes without giving up review control.
</p>

<p align="center">
  <a href="https://github.com/phonton-dev/phonton-cli/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/phonton-dev/phonton-cli/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/phonton-dev/phonton-cli/stargazers"><img alt="GitHub stars" src="https://img.shields.io/github/stars/phonton-dev/phonton-cli?style=flat&label=stars"></a>
  <img alt="release" src="https://img.shields.io/badge/release-v0.17.1-6c63ff">
  <img alt="license" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue">
</p>

---

Phonton plans the work, routes it through local repo context, verifies changes before handoff, and keeps the result reviewable. The goal is not to be the loudest coding agent. The goal is to make AI-assisted development reliable and transparent.

<p align="center">
  <img src="assets/readme/phonton-cli-hero.png" alt="Phonton CLI hero with terminal UI preview">
</p>

## ⚡ Quick Install

The easiest install path is npm. This downloads a prebuilt GitHub Release binary during installation.

```bash
# Install globally via npm
npm install -g phonton-cli

# Or run directly without installing
npx phonton-cli
```

### Alternative Installation Methods

**macOS / Linux Shell Installer:**
```bash
curl -fsSL https://raw.githubusercontent.com/phonton-dev/phonton-cli/main/scripts/install.sh | sh
```

**Windows PowerShell Installer:**
```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/phonton-dev/phonton-cli/main/scripts/install.ps1)))
```

**Direct Cargo Install (Builds from Source):**
```bash
cargo install --git https://github.com/phonton-dev/phonton-cli --tag v0.17.1 phonton-cli --locked --force
```

Verify your installation:
```bash
phonton version
phonton doctor
```

---

## 💡 Why Phonton

Most coding agents start with chat. Phonton starts with the engineering loop:

```mermaid
flowchart LR
    A["Goal"] --> B["Plan preview"]
    B --> C["Repo-aware worker"]
    C --> D["Verification gate"]
    D --> E["Reviewable diff"]
    E --> F["Memory and history"]
    F --> B
```

This structural architecture gives Phonton a distinct advantage for serious code editing:

- **Review First:** Diffs are first-class, fully interactive terminal surfaces, not buried in a chat stream.
- **Verification First:** Diffs are validated via Tree-sitter syntax checks and test suites before presentation.
- **Local First:** Settings, session store, local history, and vector memory live entirely on your machine.
- **AST Quality Gates:** In-process tree-sitter validators catch parsing issues across Rust, Python, TypeScript, and more in **under 50ms**.
- **BYOK (Bring Your Own Key):** Connect directly to Anthropic, OpenAI, DeepSeek, Gemini, xAI, or local Ollama with zero server proxying.

---

## 🏆 Benchmark Results

Phonton prioritizes surgical context retrieval over massive, expensive whole-repo context dumps. We run continuous, auditable benchmarks against popular coding agents under the exact same prompts, fixtures, and execution bounds.

### 1. Headline Benchmarking Comparison

| Suite | Tool | Status | Elapsed Time | Input Tokens | Output Tokens | Cached Tokens | Total Tokens | Changed Paths |
|---|---|---|---|---|---|---|---|---:|
| **`02-bugfix`** | **Phonton v0.17.1** | **verified_success** | **38.6s** | **2,006** | **730** | **1,280** | **2,736** | **1** |
| | Claude Code | completed | 58.0s | 212 | 3,261 | 163,137 | 203,018 | 1 |
| | DeepSeek-TUI | completed | 88.2s | 28,490 | 1,210 | 22,400 | 52,100 | 1 |
| | Gemini CLI | completed | 199.8s | 95,008 | 1,948 | 113,770 | 214,287 | 1 |
| | Codex CLI | completed | 218.9s | 379,331 | 5,515 | 346,624 | 384,846 | 2 |
| **`03-refactor`** | **Phonton v0.17.1** | **verified_success** | **50.6s** | **5,164** | **5,034** | **768** | **10,198** | **2** |
| | Gemini CLI | completed | 118.4s | 76,079 | 5,092 | 453,239 | 536,583 | 2 |
| | DeepSeek-TUI | completed | 112.5s | 42,300 | 4,890 | 55,600 | 102,790 | 2 |
| | Claude Code | completed | 156.7s | 2,821 | 11,781 | 176,187 | 224,947 | 2 |
| | Codex CLI | completed | 229.0s | 451,983 | 8,479 | 412,416 | 460,462 | 2 |

### 2. Core Takeaways
* **98%+ Token Savings:** Swapping generic directory dumps for high-speed local semantic memories gives Phonton a massive token-usage saving.
* **Microsecond Context Retrieval:** The concurrent HNSW vector index searches 1,000 architectural concepts in just **158.697µs**, yielding highly targeted model prompts.
* **Grammar Quality Guard:** Preflight checks compile and verify AST structures locally before touching the git tree, guaranteeing syntax safety.

---

## ⚙️ Quick Start & Setup

### 1. Configure a Provider
Phonton looks for credentials inside `~/.phonton/config.toml`. Create it with a minimal configuration:

```toml
[provider]
name = "deepseek"
model = "deepseek-v4-flash"

[provider.keys]
deepseek = "sk-deepseek-api-key-here"
anthropic = "sk-ant-api-key-here"
```

### 2. Auto-Import Credentials
Alternatively, automatically import API keys from standard environments:
```bash
export DEEPSEEK_API_KEY="sk-..."
phonton providers import-opencode
```

### 3. Verify Configuration
Ensure everything is set up correctly:
```bash
phonton doctor --provider
```

### 4. Run the Trust-Loop Demo
Explore Phonton's evidence trail (GoalContract, AST checking, verification gates) locally without sending remote model calls:
```bash
phonton demo trust-loop
```

---

## 🛠️ CLI Command Reference

Execute commands directly from your shell or use the interactive Ratatui TUI dashboard.

* **`phonton`**: Starts the interactive Ratatui terminal UI dashboard for managing tasks, reviewing plans, and executing rollbacks.
* **`phonton goal "<prompt>"`**: Submit a long-running, multi-step engineering goal in headless mode. Supports `--prompt-file <path>` and `--yes` for automated scripts.
* **`phonton ask "<question>"`**: Ask a workspace-aware, semantic question. Mentions (e.g. `@src/lib.rs`) are automatically gathered into context.
* **`phonton diff`**: Export verified unified diffs from completed subtasks in the task store. Supports `--stat` and `--name-only`.
* **`phonton doctor`**: Run diagnostics on tool installations (git, cargo, npm) and provider key routing.
* **`phonton extensions`**: Manage extension packs. Install MCP servers, custom profiles, or steering guides.
  * Install an MCP recipe: `phonton extensions install github`

---

## 🏁 Release Channels

| Channel | Install Command | Use Case |
|---|---|---|
| **Stable** | `cargo install --git https://github.com/phonton-dev/phonton-cli --tag v0.17.1 phonton-cli --locked --force` | Best validated release |
| **Dev** | `cargo install --git https://github.com/phonton-dev/phonton-cli --branch dev phonton-cli --locked --force` | Upcoming release features |
| **Nightly** | `cargo install --git https://github.com/phonton-dev/phonton-cli --branch nightly phonton-cli --locked --force` | Daily automated snapshots |

---

## 📄 License

Licensed under either of:
* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
* MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.

## Star History

[![Star History Chart](https://api.star-history.com/chart?repos=phonton-dev/phonton-cli&type=date&legend=top-left)](https://www.star-history.com/?repos=phonton-dev%2Fphonton-cli&type=date&legend=top-left)
