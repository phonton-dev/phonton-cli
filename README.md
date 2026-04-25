# Phonton

Phonton is a local-first Agentic Development Environment (ADE), built specifically for deterministic, verified code generation. Unlike general-purpose agents, it is built on three strict pillars: verified output (code is checked by Cargo before surfacing), cross-session memory (preserving decisions and rejected approaches), and semantic codebase indexing. It supports a bring-your-own-key (BYOK) multi-provider architecture for maximum privacy and cost flexibility.

## Architecture

```
phonton-cli ─────────────────────────────────────────────────
    └─ phonton-orchestrator
           ├─ phonton-planner ──── phonton-memory ─── phonton-store
           ├─ phonton-worker  ──── phonton-providers
           │       ├─ phonton-context
           │       ├─ phonton-index (tree-sitter + fastembed + usearch)
           │       ├─ phonton-diff  (git2)
           │       └─ phonton-sandbox
           └─ phonton-verify  (tree-sitter + cargo check/test)
phonton-types  (used by all)
phonton-desktop (Tauri — deferred)
```

## Prerequisites

- **Rust Stable** (MSRV 1.78+)
- **Cargo**
- **libgit2** system dependency (needed for `phonton-diff`)
- **tree-sitter CLI** (needed for `phonton-index` development)

## Running the TUI

```bash
cd phonton-dev
ANTHROPIC_API_KEY=sk-ant-... cargo run -p phonton-cli
```

## Running Tests

```bash
cargo test --workspace                              # all unit tests
cargo test --test smoke_test --features integration-tests  # integration
```

## Crate Completion Status

| Crate | Status | Key Gaps |
|---|---|---|
| phonton-types | ✅ Done | — |
| phonton-index | ✅ Done | Incremental re-index on file change (M2 checklist item) |
| phonton-providers | ✅ Done | Anthropic, OpenAI, Gemini, Ollama all implemented with real HTTP calls |
| phonton-verify | ✅ Done | 4-layer pipeline working. Integration tests cover syntax fail, type fail, test fail, and pass-through |
| phonton-store | ✅ Done | Full async + sync API, warm-crate cache, memory records, task history |
| phonton-memory | ✅ Done | Keyword-overlap query, async facade, wired into planner |
| phonton-worker | ✅ Done | All 5 tools implemented (Read/Write/Run/Bash/Network). Memory write-back on pass + failure |
| phonton-orchestrator | ✅ Done | DAG walk, retry/escalate, verify gate enforced, budget check |
| phonton-planner | ✅ Done | Regex + LLM decomposition, memory consultation, DAG cycle detection |
| phonton-context | ✅ Done | Sliding window, priority eviction, TiktokenCounter, provider-backed summarization, 9 tests |
| phonton-cli | ✅ Done | Ratatui task board, goal/task/ask modes, savings line, spinner, tests. **Ask mode uses stub** |
| phonton-diff | ✅ Done | git2 apply, stash-based rollback, `RollbackGuard` |
| phonton-sandbox | ✅ Done | OS-level isolation (Job Objects, unshare, sandbox-exec), ExecutionGuard routing, async timeout |
| phonton-desktop | ❌ Not started | Deferred (M10) |

## Design Documents

For architecture context and foundational principles, see `phonton-brain/`:
- `CLAUDE.md` — Hard rules for all contributions
- `00-context/roadmap.md` — Milestone map and timeline
- `01-architecture/` — Failure modes, structural patterns, and decisions

## Contributing

Before submitting PRs, be aware of the following principles:
- **Verification Gate:** All changes must pass `cargo check --workspace` and the test suite. 
- **No-Panic Rule:** Library crates must not panic. Use robust `Result` based error handling via `anyhow` or `thiserror` everywhere instead.
