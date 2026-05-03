# Changelog

All notable Phonton CLI release changes should be documented here.

This project follows pre-1.0 SemVer: minor versions may still include breaking changes while the public API and CLI surface settle.

## 0.3.1 - TUI Polish Patch

### Changed

- Replaced the native blinking terminal cursor in the input bar with a drawn, steady caret.
- Refined the splash ASCII logo to a tighter ANSI-shadow style closer to the current product mockups.
- Smoothed the animated logo gradient with slower ticks, eased color interpolation, and a softer glint.

## 0.3.0 - Extension Runtime Alpha

### Added

- Extension loader for user and workspace manifests, including skills, steering, MCP servers, and profiles.
- Worker prompt preamble injection for active text-based extensions.
- `phonton extensions` inventory and doctor commands, plus `phonton skills list` and `phonton steering list` aliases.
- MCP runtime with lazy stdio/HTTP server startup, tool discovery, tool calls, trust checks, approval policies, and event reporting.
- `phonton mcp list`, `phonton mcp tools`, and `phonton mcp call` commands.
- TUI approval modal for MCP operations, including approve, deny, keyboard navigation, and denial on quit.
- Worker-facing `MCP_TOOL_CALL` flow with capped MCP results and an end-to-end approval plus verified-diff test.
- Compact TUI splash logo and smoother gradient treatment.
- Cloudflare Workers AI provider alias for the OpenAI-compatible endpoint, defaulting to `@cf/moonshotai/kimi-k2.6`, plus an explicit Settings/config account ID field.

### Fixed

- Release clippy blockers in the extension trust inference, MCP client enum layout, and worker MCP error rendering.
- npm wrapper testing now runs the freshly built binary instead of a stale ignored vendored binary when checking local release readiness.

### Known Limitations

- Extension installation is not a package marketplace yet; 0.3.0 focuses on local manifest loading, visibility, diagnostics, and MCP execution.
- MCP server coverage depends on user/workspace configuration and trust policy.
- Benchmark reports remain planner estimates unless explicitly labeled otherwise.

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
- BYOK provider layer for Anthropic, OpenAI, OpenRouter, Gemini, Cloudflare Workers AI, AgentRouter, DeepSeek, xAI/Grok, Groq, Together, Ollama, and custom endpoints.
- Local store, memory, planner, worker, diff, sandbox, verification, context, index, and orchestration crates.
- README visuals and release-oriented documentation.
- Plan benchmark harness with Markdown and JSON output.
- CI workflow for format, clippy, tests, and release build.

### Known Limitations

- Pre-1.0 CLI behavior and crate boundaries may change.
- Public benchmark claims are not ready yet; current reports are planner estimates.
- Hosted/team workflows, editor extensions, and desktop packaging are not part of this release.
- Cross-repo context requires a `nexus.json` setup and is not enabled by default.
