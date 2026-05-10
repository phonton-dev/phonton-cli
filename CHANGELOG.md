# Changelog

All notable Phonton CLI release changes should be documented here.

This project follows pre-1.0 SemVer: minor versions may still include breaking changes while the public API and CLI surface settle.

## 0.11.0 - Context Engine

### Added

- Added typed `ContextPlan` data so worker prompt context is budgeted and
  auditable before every provider call.
- Added a deterministic context compiler in `phonton-context` that keeps
  ranked repository slices under a target budget and records omitted code
  tokens.
- Added benchmark scoring for `verified_success_per_10k_tokens`.
- Added attempt-level prompt accounting for first attempts, repair attempts,
  context/artifact buckets, and verifier retry diagnostics.
- Added `/ask <question>` plus scrollable Ask answers with lightweight
  markdown-style rendering.

### Changed

- Worker prompts now include compact repo-map orientation plus only the
  selected context slices.
- `/context`, `/why-tokens`, and Flight Log prompt manifests now expose
  context target, target-exceeded status, repo-map tokens, selected code
  tokens, omitted candidate code tokens, and attempt buckets.
- Providers now use dynamic output ceilings instead of one large fixed
  completion limit, reducing runaway generated-code outputs while preserving
  headroom for broad tasks.
- Broad generated-code repair attempts now keep adequate output headroom
  instead of collapsing to the smallest repair budget.
- Receipt and Markdown review output now include a deterministic brief summary
  without spending another model call.

### Fixed

- Benchmark token scoring no longer double-counts provider aliases such as
  `input_tokens`/`prompt_tokens` or `output_tokens`/`completion_tokens`.
- Context manifests now state when the target was exceeded because at least one
  required slice had to be included.

## 0.10.0 - Verification And Failure QoL

### Added

- Added a multi-language syntax verifier registry covering Rust, Python,
  JavaScript/TypeScript, JSON, TOML, YAML, HTML, and CSS changed files before
  review-ready status.
- Added the TUI Problems focus view, `/problems`, `/diagnostics`, `/retry`,
  `/repair`, and `/why-tokens` commands.
- Added failed/unverified Markdown review receipts that include verifier and
  subtask diagnostics.

### Changed

- Worker verifier retry prompts now use compact diagnostics instead of feeding
  back large previous error/output blobs.
- Failed selected goals default to Problems focus and expose a short failure
  type such as `syntax`, `quality`, `provider`, or `command` in goal lists.

## 0.9.3 - Python Verification Hotfix

### Fixed

- Generated whole-file Python diffs are now parsed by the syntax verifier
  before review-ready status, preventing invalid files such as an
  unterminated `chess.py` from being reported as verified.
- Empty or non-Cargo workspaces no longer allow Python generation to fall
  through to a misleading `VerifyLayer::Test` pass when no Python syntax check
  has run.

## 0.9.2 - Quality Gate Repair Hotfix

### Fixed

- Quality-gate failures now feed back into the worker once as repair context
  instead of immediately failing the whole task after syntax/build/test
  verification passes.
- Chess benchmark runs that miss a specific contract requirement, such as
  reset/new-game behavior, now get one targeted repair pass before Phonton
  reports a terminal failure.

## 0.9.1 - npm Wrapper Cache Hotfix

### Fixed

- Fixed the npm wrapper so cached `npm/vendor` binaries are version-pinned to
  the installed package and refreshed when stale.
- Added npm-wrapper coverage for stale vendor metadata, preventing `npx` or
  cached installs from running an older Phonton binary after a package update.

## 0.9.0 - Token Budget, History, And Workspace Trust

### Added

- Added structured workspace trust records with per-workspace permission mode,
  source, trusted-at, and last-seen metadata.
- Added `/trust current`, `/trust list`, and `/trust revoke-current` surfaces
  for inspecting and revoking workspace trust from the TUI.
- Added resumable prompt history to saved session snapshots.
- Added in-place filtering and selected-row details to the TUI History view.

### Changed

- Worker first-attempt prompts now omit bulky diff examples unless retry errors
  indicate the model needs diff-format guidance.
- Worker repo context now deduplicates overlapping planner/semantic slices and
  reports deduped tokens in the prompt manifest.
- Prompt manifests now expose repo-code tokens, budget limit, auto-compacted
  tokens, and deduped tokens in the Flight Log and `/context` output.

## 0.8.2 - Artifact Scroll And Image Chips

### Added

- Added mouse-wheel and `PgUp` / `PgDn` scrolling for the Active receipt/code
  surface so large review-ready diffs remain readable in the TUI.
- Added image path paste/drop artifacts. Pasting an image file path now creates
  an `[image: name.png]` chip and submits the path as an image artifact instead
  of plain goal text.

### Changed

- Prompt artifact chips now get stable accent colors instead of rendering as
  plain white text in the prompt bar, sidebar, and Active goal header.

## 0.8.1 - Paste Burst Hotfix

### Fixed

- Fixed Windows/VS Code terminal paste fallback when bracketed paste is not
  delivered by the terminal: rapid multiline key bursts are now collapsed into a
  single paste artifact instead of queueing each line as a separate goal.
- Increased the TUI input channel capacity for large paste bursts.

## 0.8.0 - Prompt Artifact Paste System

### Added

- Enabled bracketed-paste support for the TUI build so terminal paste arrives as one paste event instead of repeated Enter keys.
- Allowed clipboard paste directly into Settings fields so API keys can be entered without leaking through the Goal bar.

### Changed

- Long or multiline clipboard content remains collapsed as a paste chip until the user intentionally presses Enter.
- Windows and Unix pasted line endings are normalized before creating paste artifacts.

### Fixed

- Blocked credential-looking pasted blocks from becoming goal/model context and redirected single API-key pastes to Settings.

## 0.7.4 - Goal Switching And Focus QoL

### Added

- Added stable numeric goal indexes plus `Alt+Up`, `Alt+Down`, and `Alt+1` through `Alt+9` for faster multi-goal switching.
- Added `/goals` and `/switch` for a searchable goal switcher drawer.
- Added Active panel focus tabs: Receipt, Code, Commands, and Log. Review-ready goals with diff hunks default to Code focus.
- Added `f` to cycle focus views and `[` / `]` to move through changed files or command runs when the prompt is empty.
- Added `/focus code|commands|receipt|log`, `/copy`, `/rerun`, `/stats`, and `/compress` as an alias for `/compact`.

### Changed

- Command run summaries now stay collapsed unless the Commands focus view is selected, where Phonton shows status, exit code, duration, and stdout/stderr previews.
- Code focus renders review-ready diff hunks directly when available, falling back to changed-file summaries.

## 0.7.3 - Context And Permission Controls

### Added

- Added `/context` to show the latest prompt-section token manifest and session prompt totals from inside the TUI.
- Added `/compact` to request a worker context-compression pass for the selected running goal and reset the local context meter.
- Added `/stop` to cancel the selected planning/running goal through the orchestrator control channel.
- Added persisted permission modes: `ask`, `read-only`, `workspace-write`, and `full-access`, with `/permissions set <mode>` and System panel visibility.

### Fixed

- Goal submission now sends an immediate Planning state before attachment, memory, provider, and preflight setup, so Enter does not look frozen while background work starts.
- Hosted providers now fail before dispatch when no API key is resolved instead of silently falling back to the stub dispatcher.

## 0.7.2 - Goal Dispatch Hotfix

### Fixed

- Goal-mode chess requests now dispatch immediately instead of stopping at a clarification state.
- Empty-workspace chess goals now default to a concrete terminal Python target with `chess.py`, `python -m py_compile chess.py`, and `python chess.py` in the visible contract.
- Short chess goals no longer inherit the generic "What exact behavior or artifact should Phonton produce?" clarification question.

## 0.7.1 - Clarification Hotfix

### Fixed

- Stackless broad goals such as `make chess` now stop at a visible clarification state instead of dispatching a worker and spending provider tokens on an under-specified contract.
- Submitting a goal in the TUI now starts goal setup in the background so the prompt returns control immediately while planning and local context setup continue.

## 0.7.0 - Trust Loop Receipts

### Added

- `phonton plan` text output now shows the visible GoalContract, including acceptance criteria, expected artifacts, likely files, verification plan, run plan, quality floor, assumptions, and clarifying questions.
- `phonton demo trust-loop --json` now emits a deterministic fixture-style trust demo for reproducible onboarding and release evidence.
- `phonton review --markdown` now exports review receipts with changed files, verification, run commands, known gaps, rollback, tokens, and influence/memory sections.
- `phonton run [latest|<task-id>]` now executes receipt-suggested structured run commands through the existing sandbox and reports exit code, duration, and output previews.

### Changed

- Shared stack-aware contract preflight between the TUI and `phonton plan` so npm, Cargo, and Makefile workspaces expose the same inferred verification and run plans before execution.
- First-run trust-loop docs now point users toward contract preview, Markdown receipts, and running receipt commands rather than benchmark claims.

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
