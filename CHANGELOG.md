# Changelog

All notable Phonton CLI release changes should be documented here.

This project follows pre-1.0 SemVer: minor versions may still include breaking changes while the public API and CLI surface settle.

## 0.6.2 - Sandbox And Prompt Hotfix

### Fixed

- Worker filesystem tools now honor sandbox approval decisions before reading or writing files.
- Sandbox path evaluation now normalizes parent traversal before root and blocked-path checks, closing lexical `..` escapes.
- Deleted or cleared paste artifact chips no longer submit hidden pasted content with the next prompt.
- `/run` parsing now requires a standalone `/run` command and routes single-ampersand shell commands through approval-gated bash handling.

## 0.6.1 - Cloudflare Provider Hotfix

### Fixed

- Cloudflare Workers AI responses are now parsed through a tolerant adapter that accepts both strict OpenAI-compatible chat completions and Cloudflare-style result envelopes.
- Cloudflare upstream error envelopes now surface their actual error message instead of being hidden behind `missing choices[0].message.content`.
- Cloudflare chat completion requests now send `max_completion_tokens` and disable provider-side thinking for worker calls, matching the current Workers AI schema for Kimi K2.6 while keeping worker output diff-focused.

## 0.6.0 - Command UX And Trust Demo Loop

### Added

- Restored first-class TUI slash commands through a shared command registry used by prompt submission, Tab completion, the command palette, and the command drawer.
- Added `/settings` and `/config` back as stable settings shortcuts, plus `/status`, `/review`, `/memory`, `/permissions`, `/model`, `/commands`, `/goal`, `/task`, `/ask`, `/clear`, `/delete`, `/quit`, and `/exit`.
- Added `/model set <name>` for fast model preference changes without digging through the settings form.
- Added a prompt-adjacent command drawer when the input starts with `/`, making command discovery visible while typing.
- Added `phonton init` to create the default config path for first-run setup.
- Added `phonton demo trust-loop`, a compact first-run evidence-trail walkthrough centered on GoalContract, verification, review receipt, and memory.

### Fixed

- Unknown slash commands now show a suggestion and do not get queued as agent goals.
- `/run <cmd>` and `!<cmd>` continue to route through sandboxed command execution while coexisting with normal slash commands.

## 0.5.0 - Prompt, Commands, And Quality Gates

### Added

- Long or multiline TUI pastes now collapse into prompt artifacts like `[paste: 18 lines, 3.4k chars]` while preserving bounded full content for the submitted goal.
- Added Windows clipboard import with `Ctrl+V`, including content selected via Windows clipboard history (`Win+V`) when the terminal does not emit bracketed paste directly.
- Added `/run <cmd>` and `!<cmd>` prompt-bar command execution with sandbox routing, command status, exit code, duration/output previews, and Flight Log evidence.
- Added prompt-section token manifests in the Flight Log to expose approximate system, goal, memory, attachment, MCP, and retry-context costs per provider call.
- Added stack-aware preflight for `package.json`, `Cargo.toml`, and `Makefile` workspaces so contracts include concrete verification and run commands when detectable.

### Changed

- The worker no longer duplicates the system prompt inside rendered user context.
- Generic completion memories such as `completed: make chess` are filtered from future memory preambles.
- Broad chess goals now require playable-game acceptance criteria and fail the quality gate before review when the result is only a placeholder.
- Prompt editing gained `Ctrl+U`, `Ctrl+K`, history navigation, and slash-command completion QoL.

## 0.4.8 - TUI Polish

### Fixed

- The Active panel now shows the real worker subtask label when memory context is attached, instead of leaking the raw `# Prior context from memory` preamble.
- The PHONTON splash wordmark keeps the same ASCII art and gradient styling but no longer animates the full-logo color phase, avoiding Windows terminal shimmer artifacts.

### Changed

- Exit confirmation now shows an in-TUI session summary with goal counts, token totals, estimated savings, and resume behavior before closing.

## 0.4.7 - Cloudflare Account Persistence

### Fixed

- Settings saves now persist the Cloudflare Account ID, keeping the Workers AI endpoint configuration stable across new goals and CLI restarts.
- Goal runs and Settings saves now share the same Settings-to-config sync path to avoid provider-field drift.

## 0.4.6 - Cloudflare Diagnostics

### Fixed

- Settings connection tests now report missing Cloudflare Account ID or Workers AI base URL instead of incorrectly saying `cloudflare` is an unknown provider.
- Failed goal details are now shown in the Active panel so configuration failures remain visible after a goal stops.

## 0.4.5 - Provider Config Panic Fix

### Fixed

- Goal runs no longer panic when the selected provider cannot build a run configuration, such as Cloudflare without an Account ID or Workers AI base URL.
- The TUI now marks the goal failed with an actionable provider setup message instead of tearing down the terminal.
- Real worker dispatch now derives per-tier provider configs from a validated template, preserving custom endpoints while still honoring configured models.

## 0.4.4 - Shadow Logo Restore

### Changed

- Restored the normal animated ANSI Shadow Phonton splash logo, compact header glyphs, Braille spinner, and unicode token-savings bar.
- Kept the v0.4.3 terminal-corruption fix: semantic-index model downloads remain silent while the Ratatui TUI owns the terminal.

## 0.4.3 - Terminal-Safe TUI

### Fixed

- Disabled fastembed/Hugging Face model download progress output while the Ratatui TUI is active, preventing `model.onnx` progress bars from corrupting the input area.
- Switched the TUI splash wordmark and spinner to ASCII-safe glyphs so Windows terminal font fallback does not smear the startup screen.
- Routed semantic-index setup failures through tracing instead of writing directly to stderr during an active TUI session.

## 0.4.2 - Session Resume

### Added

- `phonton -r` / `phonton --resume` now restores the latest saved interactive TUI session for the current workspace.
- Confirmed quit flow for the TUI: `Ctrl+C` or top-level `Esc` opens an exit confirmation instead of ending immediately.
- Session exit receipts now print saved-session totals, including actual tokens used, estimated naive baseline tokens, estimated saved tokens, and best observed savings percentage.
- Durable per-workspace session snapshots in the local store so visible goals, ask state, Flight Log data, and token totals survive CLI restarts.
- Restored the normal ANSI Shadow Phonton splash logo and added a muted TUI version label.

## 0.4.1 - Trust Surface Patch

### Fixed

- `phonton plan --json` now exposes `goal_contract` at the top level of the plan preview report, so release smoke tests and external tooling can validate the advertised v0.4 accountability surface directly.
- npm wrapper release testing now runs a real `phonton plan --json --no-memory` smoke check and fails if the GoalContract surface is missing or malformed.

## 0.4.0 - Accountability Handoff Alpha

### Added

- Prompt file mentions in the TUI goal bar. Users can reference workspace files with `@path`, `@"path with spaces.md"`, or `@[path with spaces.md]`.
- Bounded text attachment context and image attachment metadata/payload plumbing for compatible providers.
- First-slice v0.4 accountability types: `GoalContract`, `HandoffPacket`, `OutcomeLedger`, context manifests, permission ledgers, verification reports, and handoff summaries.
- Planner-generated goal contracts that capture acceptance criteria, assumptions, likely files, and attachment influence.
- Review-ready TUI handoff receipts with result headline, changed files, verification, run commands, known gaps, token usage, and rollback context.
- Durable `outcome_ledgers` store table so completed task evidence survives the TUI session.
- History and review surfaces now consume persisted handoff data when available.

### Changed

- Orchestrator final state now includes a deterministic handoff packet derived from verified subtasks, diff hunks, checkpoints, and token usage.
- Store task history joins outcome ledgers so review/history commands can show evidence beyond raw status JSON.

### Known Limitations

- `ContextManifest` and `PermissionLedger` are persisted as minimal/default records in this slice; deeper source attribution and approval replay are planned next.
- Image payloads are only sent natively to providers with compatible request formats. Other providers receive deterministic image metadata.
- Run-command inference is conservative and may be absent until task-class quality gates mature.

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
