# Changelog

All notable Phonton CLI release changes should be documented here.

This project follows pre-1.0 SemVer: minor versions may still include breaking changes while the public API and CLI surface settle.

## 0.2.0 - Public Alpha

### Added

- Persistent memory wiring for live CLI goal runs, worker decision records, and verify decision checks.
- `phonton memory` commands for list, edit, delete, pin, and unpin.
- Review payloads with token buckets, provider/model cost summaries, checkpoint lists, and persisted review decisions.
- Provider doctor checks that validate both model discovery and a tiny completion call through the configured run adapter.

### Fixed

- Generic planning goals now preserve the original request instead of collapsing to lossy names like `feature input`.
- Orchestrator tests now run against temporary workspaces instead of mutating tracked fixtures.
- Release checks now fail if `cargo test --locked --workspace` leaves the workspace dirty.

## 0.1.0 - Public Alpha

Initial release target for the `phonton-dev/phonton-cli` repository.

### Added

- Ratatui TUI for goal/task/ask workflows.
- `phonton doctor` diagnostics for config, provider keys, store, trust, git, cargo, and Nexus config.
- `phonton plan` preview for task DAGs.
- `phonton review` commands for review payloads, approval, rejection, and rollback.
- BYOK provider layer for Anthropic, OpenAI, OpenRouter, Gemini, AgentRouter, DeepSeek, xAI/Grok, Groq, Together, Ollama, and custom endpoints.
- Local store, memory, planner, worker, diff, sandbox, verification, context, index, and orchestration crates.
- README visuals and release-oriented documentation.
- Plan benchmark harness with Markdown and JSON output.
- CI workflow for format, clippy, tests, and release build.

### Known Limitations

- Pre-1.0 CLI behavior and crate boundaries may change.
- Public benchmark claims are not ready yet; current reports are planner estimates.
- Hosted/team workflows, editor extensions, and desktop packaging are not part of this release.
- Cross-repo context requires a `nexus.json` setup and is not enabled by default.
