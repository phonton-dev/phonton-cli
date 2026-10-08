# Changelog

All notable Phonton CLI release changes should be documented here.

This project follows pre-1.0 SemVer: minor versions may still include breaking changes while the public API and CLI surface settle.

## 0.22.1 - Diffs that land

### Fixed

- [fixed] Workers' diffs no longer fail on miscounted `@@` line ranges. Phonton
  recomputes each hunk's counts from its body and, when the stated line is
  wrong, moves the hunk to where its unchanged and removed lines match the
  current file (nearest match if several). Hunks that match nowhere still fail
  verification. Asked directly, DeepSeek flash sent applicable ranges in 6 of
  13 diffs; all 13 apply after this. On the new `ledger` smoke suite (4 tasks,
  3 runs each, same model) acceptance stayed 12/12 while tokens fell 50%, cost
  63% and wall time 55%, because fewer attempts were retries.
- [fixed] Files over 24 KB reach workers as excerpts around the names the
  subtask mentions instead of signatures only, so edits deep in a large
  module no longer have to guess the surrounding lines.
- [fixed] A goal containing the word "release" (for example "release the
  stock") was treated as broad, multi-agent work and split into several LLM-
  planned subtasks. LLM plans now use the fewest subtasks that cover the goal
  and add test subtasks only when the goal asks for tests.
- [fixed] `phonton models` progress prints stages without byte counts as plain
  stages instead of `? / ? bytes`.

### Removed

- [removed] Plan preflight special cases for goals mentioning chess and for a
  `src/receipt.js` file. Both were tuned to old benchmark fixtures.

### Added

- [added] `phonton models setup MODEL` runs setup, install, calibrate and
  select in one command and stops at the first failure. Plain
  `phonton models setup` prints that next step.
- [added] `fixtures/ledger` (a 339-line module) and `fixtures/shop` (a 28 KB
  module, past the whole-file limit), each with four smoke tasks scored by
  hidden tests.

## 0.22.0 - Product Hunt beta

### Added

- [added] `benchmarks/smoke`: runs fixed goals on fresh copies of
  `fixtures/todo-api` and scores them with hidden acceptance tests written
  after the run (`node benchmarks/smoke/run.mjs --arm cloud|local`).

- [added] `PHONTON_HOME` (absolute path) relocates config, store, trust and
  model state, for clean-room testing or several independent setups.
- [added] Headless `phonton goal` prints a receipt: changed files with line
  counts, checks that passed, tokens, estimated cost and the next command.
  Failures print the reason.

### Changed

- [changed] `phonton goal` with a local provider and a calibrated model runs
  through the local harness, matching the TUI, and prints the candidate diff,
  check results, and the apply command instead of the raw JSON receipt
  (`phonton goal --local show RUN_ID` keeps the full receipt). `goal --local`
  accepts flags before the goal text, and `apply`/`rollback` print one-line
  results.
- [changed] Local goals in the TUI work with an Ollama that Phonton did not
  start (macOS and Linux, where managed setup is unavailable, or a
  self-installed runtime on Windows). The plan review says the runtime is
  unverified and may relay repository context; approving the plan is
  consent for that goal, and the receipt labels the runtime origin unknown.
  Headless runs still need `phonton goal --local --allow-unverified-runtime`,
  and the refusal now names that flag.
- [changed] The TUI's local plan review lists files, checks, model, and
  budget without per-file hashes or model digests (they stay in the run
  receipt), and drops the "requires host approval" note once checks are
  allowed for the session.
- [changed] `phonton doctor` checks only the toolchains the project uses
  (cargo for Cargo projects, node for package.json projects), drops the
  benchmark-Python, missing-Nexus, default-model, and probe-skipped
  warnings, prints paths without the `\\?\` prefix, and points a missing
  key at `phonton models setup` as the no-key alternative.
- [changed] An Ollama provider with no configured model uses the model
  Phonton calibrated instead of `llama3.2:3b`, and Ollama errors include the
  HTTP status and response body.
- [changed] `phonton review reject` undoes the task: every file its
  checkpoints changed goes back to its pre-task content in the worktree and
  index. It refuses, changing nothing, if any of those files were edited
  afterwards. Approved tasks can no longer be rejected.
- [changed] Goals may add or edit tests. New test files run with the
  candidate; if a candidate edits existing tests, the original versions must
  also pass against its source changes, so a model cannot certify itself by
  rewriting assertions. Manifest edits (`package.json`, `Cargo.toml`, lock
  files) still need independent checks.
- [changed] When a worker cannot produce a usable diff, the subtask escalates
  to the next model tier instead of failing outright. Checks that cannot run
  do not escalate.
- [changed] First run with no usable model opens Settings instead of starting
  a goal that cannot run; the goal text is kept. A pasted API key is routed to
  Settings with its provider guessed. With no provider configured, Phonton
  picks one from provider key environment variables, then a selected local
  model.
- [changed] Phonton no longer installs updates on exit; it prints the npm
  command instead.

- [changed] Workers now see the exact current source of the files a subtask
  edits (bounded: 4 files, 24 KB each, 48 KB total) instead of only symbol
  signatures, so unified-diff context matches on the first attempt.
- [changed] Finished subtasks are carried forward in the goal context as a
  one-line `Done/Changed` note instead of the full prompt and diff. The system
  prompt is no longer duplicated into that context, and retriever slices the
  dispatcher already selected are not sent twice.
- [changed] TUI welcome shows the goal → plan → edit → verify → review →
  remember loop and generic starter goals; footer hints adapt to terminal
  width; help overlay no longer clips rows.
- [changed] `phonton --help` opens with a quick start.

### Fixed

- [fixed] `phonton review reject` restores each file to its state just before
  the task, recorded when the task first edits it, not to `HEAD`: edits you
  had not committed and untracked files the task changed are kept instead of
  being reset or deleted.
- [fixed] `phonton goal` with an unknown provider name in config stops with
  the list of valid names instead of planning and failing with "no provider
  API key configured".
- [fixed] Local goal scope keeps a file whose text matches the goal at least
  as well as the best symbol match. "Reduce the maximum todo title length"
  selected only `src/store.js` (via `TodoStore`) and missed `MAX_TITLE` in
  `src/validate.js`, so a 3B model "passed" by truncating titles.
- [fixed] OpenAI-compatible and Anthropic requests retry twice on a dropped
  connection, 429 or 5xx before failing; one network blip used to fail the
  whole goal.
- [fixed] Local goals start the installed managed runtime when it is not
  running (after a reboot, for example) instead of falling back to a hosted
  route that failed with "ollama request failed"; Ollama connection errors
  now include their cause.
- [fixed] Memory is scoped to the repository it came from. Previously every
  repository's rejected approaches and decisions (goal text and file paths)
  were added to prompts for any other repository and sent to the provider.
  Records from older stores stay visible everywhere.
- [fixed] Rejected-approach memory no longer stores the memory preamble
  itself, which nested a copy of all earlier memory into every new record.
- [fixed] File paths quoted from memory no longer count as files the worker
  must edit; a run could fail three attempts per tier, or add a no-op edit,
  because an unrelated past failure mentioned `src/store.js`.
- [fixed] Goals like "Add a count() method to TodoStore that ..." no longer
  become a subtask "Implement method `to`": the planner reads `name()` and
  backticked names before the kind word, ignores words like "to" and "that"
  after it, and every planned subtask keeps the full goal text so the
  worker sees what the symbol must do.
- [fixed] A diff the worker already checked is not built and tested again
  when the orchestrator verifies the identical patched tree; the verdict is
  reused within the run (timeouts and missing tools are retried). On a
  141-crate Rust workspace a warm goal went from 223 s to 159 s in one run
  each.
- [fixed] Applying a local run that was already applied and rolled back
  says so instead of reporting "different candidate evidence".
- [fixed] Verification caches for workspaces unused for 7 days are deleted,
  so the shared Cargo target cache does not grow without bound in the temp
  directory.
- [fixed] Goal token totals no longer double-count a worker's tokens (progress
  reports and the final total were added together) or drop earlier
  attempts, and `why-tokens` and the receipt sum usage across retries and
  escalations instead of showing only the last attempt.
- [fixed] Escalation no longer targets model ids that do not exist
  (`claude-sonnet-4-5-20251001`, `claude-opus-4-7-20260115`,
  `gpt-5.2-preview`, `gemini-2.0-flash`, `grok-2`, `grok-2-mini`). Tier
  defaults now use current models checked against models.dev on 2026-10-07:
  Anthropic Haiku 4.5 / Sonnet 5.5 / Opus 5.5, OpenAI GPT-6 Luna / 6.1 Sol /
  6 Astra, OpenRouter GPT-6 Luna / Claude Sonnet 5.5 / Opus 5.5, Gemini
  Flash-Lite / Flash / 3.1 Pro preview, xAI Grok Build 0.1 / 4.3 / 4.7, Groq
  gpt-oss-120b / Qwen3.8 27B, Together DeepSeek V4.1 Flash / GLM-5.3 /
  Kimi K3.
- [fixed] OpenAI-compatible requests allow 16,384 output tokens (was
  4,096) so reasoning models do not spend the whole budget thinking.
- [fixed] Provider transport errors include the underlying cause (DNS, TLS,
  timeout) instead of only "error sending request".
- [fixed] Local goals no longer overflow the stack on Windows debug builds;
  the CLI runs on a 16 MiB main thread and runtime workers.
- [fixed] A piped `phonton models setup` returns once the managed runtime is
  ready instead of hanging while the runtime holds the caller's pipe.
- [fixed] `phonton models` with no subcommand prints status instead of
  panicking. Setup refused over a moved runtime folder now names the stale
  receipt to delete.
- [fixed] Verification copies link the project's `node_modules` instead of
  omitting it. Without dependencies, a model could "fix" a missing-module
  error by deleting the dependency and still pass. Cleanup removes only the
  link.
- [fixed] Verification reuses a per-workspace Cargo target directory in the
  system temp folder, so compiled dependencies survive between attempts and
  runs instead of rebuilding in every fresh copy. A small serde/anyhow crate
  fix went from 83s to 29s on a warm cache.
- [fixed] DeepSeek requests allow 32k output tokens. Its models reason before
  answering, and at 4096 harder edits came back empty.
- [removed] Hardcoded local-template answers for benchmark tasks (chess rules,
  config, receipt). They bypassed the model and could overwrite matching user
  files.
- [fixed] Keep Python bytecode and pytest cache directories out of local source
  snapshots, so warmed test runs do not fail the baseline source-change guard.
  Source edits and root-level runner bytecode remain checked.
- [fixed] Refresh compatible Rust dependencies for published advisories in
  Rustls, h2, Crossbeam Epoch, Quinn, Anyhow and CXX.
- [fixed] Verify Node test failure diagnostics with an explicit TAP fixture
  across Node versions instead of depending on the default reporter's layout.
- [fixed] Exercise model-operation admission with platform-appropriate storage
  fixtures; CI now reports all failing workspace test targets in one run.
- [fixed] Update async-trait's generated future annotations for strict Clippy
  compatibility with Rust 1.99.
- [fixed] Gate Windows-only sandbox diagnostics and managed-runtime metadata
  on their supported platform so strict Linux linting avoids unused declarations.
- [fixed] Every calibrated local TUI goal requires review of its exact frozen
  scope, model and commands before execution. Only an explicit Y approves;
  repeated keys, Enter, paste and a dropped approval channel cannot approve.
- [fixed] Local token totals include strategy proposals. The run record preserves
  unknown runtime origins, uses the original route and waits for a durable final
  receipt. Concurrent CLI/Desktop writers no longer lose aggregate updates.

- [fixed] Esc and Ctrl+C at the top level now ask for a second press before
  quitting, matching the documented quit behavior.
- [fixed] Ctrl+C no longer opens the clarification questionnaire when the
  prompt is empty.
- [fixed] TUI goals now ask once per session before running project checks on
  the host; previously every verified TUI run failed as "verification
  unavailable".
- [fixed] Ollama and custom endpoints use the configured model on every tier;
  Standard/Frontier subtasks no longer fall back to a hardcoded `llama3.2:3b`.
- [fixed] Local edits keep regex escapes like `` that small models write
  with a single JSON backslash, instead of decoding them to a backspace byte.
- [fixed] Local-run Apply works on Windows: the saved-review hash no longer
  depends on the OS path separator.
- [fixed] `phonton why-tokens` and `phonton proof export`, advertised on
  phonton.dev, are wired again. `why-tokens` reports provider usage per
  verified subtask and the selected code-context estimate.

## Unreleased - Local harness preview

### Fixed

- [fixed] Direct pytest verification recognizes ordinary ANSI-colored passing
  summaries. Python interpreter parsing now stops at a script or inline-code
  operand, so a later `-m pytest` argument cannot impersonate the runner.
- [fixed] Passing Python `unittest`, pytest, or direct script checks no longer
  qualify an edited `.py` candidate unless a bounded process-reported execution
  trace shows every source edited across the complete candidate loaded. An
  unrelated passing test keeps its command result but adds Not run inclusion
  evidence. Missing or malformed traces, isolated interpreter flags, and a
  conflicting `sitecustomize` fail closed. Plans warn about this limit; older
  Python reviews require a current run before Apply.
- [fixed] Desktop model operations now require the endpoint and managed storage
  folder from the displayed status. The engine checks both under its state
  lease before admitting setup, download, calibration, selection, deselection,
  or removal, so a separate CLI settings change cannot redirect a stale click.
- [fixed] Passing direct Node TAP tests no longer qualify an edited JS/TS
  candidate when process-reported V8 coverage does not show every source edited
  across the full candidate, including inherited repairs and mixed creation.
  An unrelated test keeps its pass result but adds Not run inclusion evidence;
  plans warn about unsupported checks and transformed source identity. Passing
  checks save matched paths, and older JS/TS reviews require a current run
  before Apply.
- [fixed] A malformed local edit no longer strands the last allowed model call
  when a two-call strategy restart cannot fit. Search retries once from the
  captured baseline within the remaining budget; duplicate rejected output
  still stops, and mixed creation retains its multi-call reserve.
- [fixed] Selected local Go checks now require a terminal passing package result for
  every edited `.go` source in the root module. An unrelated package's passing
  test cannot certify an uncompiled edit; missing or ambiguous package
  inclusion adds separate Not run evidence while preserving the command result.
  Assembly and cgo source remain Not run until file-level inclusion can be attested.
- [fixed] Local Cargo verification now matches candidate-local compiler
  dependencies with the selected package's active Rust module path for every
  edited path. A passing unrelated test cannot make an omitted feature-gated
  file review-ready; missing or ambiguous inclusion adds separate Not run
  evidence. Non-Rust mixed edits may also remain unverified by Cargo.
- [fixed] Local search normalizes JSON-escaped Windows candidate paths in
  failed-check output. An unchanged assertion after repair now restarts from
  baseline instead of spending another call on the same branch.
- [fixed] Selected Go test checks now reject conditional Go package source,
  including assembly, tagged files, ignored names, and cgo imports, when a
  broad `./...` check could otherwise pass on another file. This conservative
  guard also rejects some files that an explicit check could compile.
- [fixed] Direct local Cargo test checks now refuse command-line `--config`
  overrides and applicable Cargo configuration that supplies a target runner
  or includes uninspected files. Cargo resolves to an executable outside the
  candidate and runs by absolute path, so a candidate-local `cargo` cannot
  replace the selected check. Original and candidate checks fail closed before
  execution when this cannot be established.
- [fixed] Hosted verified hunks now materialize relative to the selected
  working directory. Git staging, side-ref checkpoints, and review approval
  translate those paths to the repository root, preserving nested projects.
- [fixed] Hosted goals keep retrieved memory in the worker prompt without
  copying it into new completion memories, architectural decision detection,
  review descriptions, or checkpoint labels. `phonton review` also replaces
  legacy memory-prefixed rollback labels with the reviewed task description.
  Empty handoff launch commands now say they concern application launch, not
  the separate verification checks. Multi-paragraph tasks keep all paragraphs
  during legacy memory separation and local-template matching.
- [fixed] Model installs retain up to eight exact retry tags and endpoints
  before admission, including requests that stop before transfer. Status
  reconciles each request with runtime inventory; partial layers never count
  as an installed model. State writes also tolerate a stale temporary file
  left by an earlier process with the same PID.
- [fixed] Calibration saves incomplete probe evidence in local state after each
  finished probe. Cancellation, memory pressure, or an engine exit leaves an
  inspectable attempt without making it selectable or replacing an earlier
  passing profile.
- [fixed] Local strategy and edit generation recheck the shared wall-time
  budget after context and runtime admission. An expired budget stops before
  another model reservation or chat; edit timeout uses the dispatch remainder.
- [fixed] Direct pytest checks refuse captured configuration or command-line
  `-o` overrides that point
  `testpaths`, `pythonpath`, or `addopts` outside the candidate, or enables
  installed-package selection through `addopts --pyargs`. A real passing pytest
  footer from the original repository cannot qualify those candidates for Apply.
  Explicit `-c` files are inspected, while config selection follows the common
  ancestor of selected test paths so unrelated nested configs do not block a root check.
- [fixed] Reopening a completed local goal now rechecks its saved baseline,
  selected candidate bytes, and canonical diff. Missing or changed evidence
  clears review selection and blocks Apply rather than showing stale checks as
  verification of the current candidate.
- [fixed] Local search now stops before another model call when the remaining
  approved-host check budget cannot cover every selected candidate check and
  dependency preparation. It retains the earlier failed-check evidence instead
  of generating an uncheckable repair.
- [fixed] Local restart search no longer stops solely because a new reviewed
  path or source span uses the same mechanism wording as an earlier proposal.
  The same mechanism at the same target remains a duplicate.
- [fixed] Interrupted local verification retains the materialized candidate's
  pre-check diff and hash in the reopened receipt, marks unfinished checks
  Unavailable, and updates elapsed time on orderly cancellation. No interrupted
  candidate becomes Apply-ready.
- [fixed] Direct shared-runtime model removal now refuses ambiguous or
  incomplete installed inventories before contacting Ollama's DELETE endpoint.
  Malformed names also remain visible as inventory warnings for preflight and
  post-delete confirmation.
- [fixed] Windows hardware detection reads physical RAM through the OS API;
  a failed or slow optional CIM probe no longer makes model fit and calibration
  unavailable when the native reading succeeds.
- [fixed] Equivalent Ollama default-library names retain calibrated model
  readiness across calibration completion, status, selection, resident reuse,
  and local goal admission. Final calibration still rejects changed weights,
  installed size, or ambiguous aliases.
  Failed recalibration clears the matching selection; duplicate installed or
  resident aliases remain ambiguous and block reuse.
- [fixed] `models status --json` exposes the complete saved calibration profile
  fingerprint used by local goal admission, allowing Desktop to catch same-model
  recalibration before dispatch.

### Changed

- [changed] `models catalog --snapshot` includes the browse-time hardware
  reading used for fit and first-try guidance. Plain `models catalog` keeps
  its existing model-array JSON shape.
- [changed] `models deselect` clears the active local model even while the
  runtime is offline, preserving installed weights and calibration. An active
  model can then be removed with a separate explicit command.
- [changed] Calibration can retry with the exact installed model already loaded
  when cold loading no longer fits. It requires 1.5 GiB free host RAM and
  rechecks memory, context, resident identity, allocation and unload time
  before each probe; automatic context still needs a reported model limit.
- [changed] `models catalog` now includes optional per-model pre-setup storage
  planning from the shared disk-admission rules, so CLI users can choose a
  suitable managed drive before runtime setup fixes that location. Desktop
  reads the same plan from the catalog snapshot API.

### Fixed

- [fixed] Baseline and candidate repair feedback retains the tail of verbose
  failed checks, budgeting stdout and stderr separately; receipts mark omitted
  middle output instead of silently losing trailing assertions.
- [fixed] Read-only goal discovery and bounded source retrieval match
  camelCase/snake_case variants of parsed symbols, with exact names preferred.
- [fixed] Mixed Apply now reports a creation-path recheck failure separately
  from an already-published target, retaining its prepared recovery journal.
- [fixed] Successful `cargo run -- test` no longer counts as a passing Cargo
  test. Verification recognizes the actual subcommand before test arguments;
  non-test Cargo commands retain diagnostic Not run evidence.
- [fixed] Adaptive local search tracks newly passing checks and failed-check
  identity/output, so a new pass or shifted generic failure is not discarded;
  losing a pass without either signal restarts from baseline.
- [fixed] Exit-zero version, status and build-only commands no longer qualify a
  local candidate as verified. Unsupported checks retain output and exit code
  as diagnostic Not run evidence; supported test runners and explicit
  candidate-local scripts still work.
- [fixed] Local goal verification commands refuse paths that escape the
  candidate copy, including parent-relative paths, rooted arguments, file URLs,
  and space-separated option values. Go package targets must be
  candidate-relative. A check cannot pass by directly targeting untouched
  original files or only a standard-library package. Go runner replacement
  flags are refused for verification. Pytest `--pyargs` and unittest selectors
  outside the candidate are also refused.
- Local goal plan review now confirms the selected model's installed digest,
  runtime version and context metadata. A stale or unavailable selection stays
  null with a warning; Run rechecks it before writing attempt evidence.
- Successful `python -m unittest` checks containing only expected failures now
  report **Not run**. A mixed run still passes when it includes an ordinary
  passing test, so an expected-failure-only suite cannot make a candidate
  review-ready.
- Model removal now confirms absence from the runtime's installed inventory
  after DELETE. A still-listed model or incomplete inventory leaves the saved
  calibration intact and reports an unconfirmed removal.
- Model install now requires a terminal pull success event and confirms the
  requested tag in installed inventory with a usable digest and size before
  reporting completion. Results include that runtime-reported identity.
- Explicit `python -m pytest` and `python -m unittest` checks now refuse
  repository-local runner modules that could forge a passing summary. Attached
  module flags and Windows windowed/versioned launchers receive the same guard;
  attached inline-code flags and interpreter command wrappers are refused.
- Inferred Python verification now adds direct `unittest` discovery for nested
  non-package test directories that root discovery skips. Pytest plans add a
  separate explicit-file check when default filename discovery or project
  configuration could omit recognized tests; that check clears config `addopts`
  that could redirect collection. Current pytest config filenames are protected
  from model edits, and runner-shadowing modules withhold inference. Oversized
  scopes require explicit commands rather than accepting a passing partial suite.
- A managed model retry can credit complete blobs already in its verified
  Ollama store after checking their manifest size and SHA-256. Sparse partial
  files remain uncredited; low-space errors report the additional space needed,
  and an interrupted pull asks users to refresh installed models before retry.
- Inferred Go verification now checks each edited package separately instead
  of accepting a passing test elsewhere in the module as evidence for an
  untested edited package. Plans spanning over four Go packages request
  explicit checks. Build-constrained files, files Go ignores by name, and cgo
  sources also need explicit checks. A candidate cannot add those conditions
  under a package-scoped check and claim
  verification. Saved receipts retain the package-scope caveat.
- Reviewed multi-file SearchReplace plans now scale the implicit call,
  check and output reservations to cover a best-case one-edit-per-path
  sequence within existing caps, and warn when explicit budgets or caps
  cannot cover it.
- Untagged local model names now resolve to Ollama's `:latest` inventory name
  across install, calibration, selection and removal. The operation result and
  saved calibration use that same explicit tag instead of failing after a
  successful shorthand install.
- Model removal now resolves the installed identity, refuses ambiguous aliases
  and protects the active model across equivalent spellings. Recalibration
  replaces the previous profile for that identity.
- Local model status now distinguishes managed recovery that blocks coding
  goals from a missing launch receipt that still permits an explicitly
  consented unverified-runtime goal. Managed identity and free-space errors
  cannot be overridden by the goal consent checkbox.
- Local calibration now tries explicit `think:false` edit and creation
  requests before probing the runtime default when either format lacks a pass.
  It prefers a mode with measured new-file creation, records that mode in a
  schema-2 profile, and reuses it for tool-call and coding requests. Older
  engines reject the new profile; current engines retain legacy schema-1
  profiles in their original runtime-default mode.
- Go verification recognizes `-C .` before `test`, so successful list-only or
  dry-run commands cannot bypass named-test evidence and claim Passed. Other
  `-C` targets remain Not run because they may verify another tree.
- Python plans now choose pytest for pytest-style tests/configuration instead
  of incorrectly proposing `unittest discover`. Successful pytest commands
  need a final summary with a passed test; collection-only, all-skipped and
  missing-summary exits remain Not run with their output retained. A missing
  pytest module is Unavailable rather than a failed project test, and late
  duplicate summaries cannot turn skipped-only output into a pass. Wrapped
  pytest commands are diagnostic-only on successful exit because they can
  change the runner or flags behind the reviewed command.
- The local engine now records a bounded recent-run index at admission and
  exposes `local.run.list` for Desktop recovery after **New goal**. Bounded
  fallback discovery includes older receipt-only evidence; full receipts and
  Apply journals remain separately readable by run ID.
- CLI users can now list and show saved local runs before deciding whether to
  Apply. `show` reports whether a terminal record exists, keeps unfinished
  evidence provisional, and rejects a receipt whose embedded run ID differs
  from the requested ID. Apply-journal reads also verify their run ID.
- `goal --local --plan` and other local-goal CLI commands no longer overflow
  the Windows main thread stack before parsing. `goal --local --help` now
  prints its command forms and exits successfully.
- Local planning and candidate verification now support complete repository
  snapshots up to 1 GiB / 20,000 files. A small scoped edit no longer fails
  because of an unrelated asset over 64 MiB; the asset remains in isolated
  check copies and their identity hash. Hashing streams bytes, and Windows
  disk admission still checks the required copies before inference. Snapshot
  copies now yield during large files so cancel and wall-time limits can stop
  the copy without waiting for an entire asset.
- Local calibration no longer applies its 512-token probe limit to every
  coding edit. The worker can reserve up to one quarter of the calibrated
  context, within the goal budget, for a complete new file or edit. The worker
  retains half the remaining budget for a later permitted call, so creating a
  file does not starve its required follow-up edit. Truncated responses still
  fail closed.
- Local Rust candidate verification now rejects edits that suppress an
  unchanged inline test tail through outer attributes, crate configuration or
  macro/import scope changes. Macro-generated, function-local and parameterized
  test attributes also remain protected, while ordinary production edits inside
  an earlier inline module remain editable.
- A temporary installed-model metadata timeout no longer hides saved
  digest/version-valid calibration in status. It remains diagnostic, and model
  selection or goal execution revalidates metadata before inference.
- Model status now exposes the same managed runtime installation reserve used
  by setup, so Desktop can identify insufficient space before downloading.
- Managed setup now reports a stale or damaged launch receipt as a recovery
  error when Ollama still answers on the default port, rather than claiming an
  existing runtime is ready while install and verified-local goals are blocked.
  External-run evidence alone no longer falsely counts as a prior managed
  installation when its chosen folder has no launch receipt.
- A low-disk managed-runtime setup now fails before creating a runtime folder
  or claiming a chosen storage location. An idle setup lock alone no longer
  strands the storage chooser; active setup and partial files still block moves.
  Detecting an external runtime no longer claims unrelated files as managed.
- Local coding goals now require a verified Phonton-started runtime or fresh
  consent for an unverified loopback service that may relay repository context.
  Process, listener, and endpoint identity are checked around every model chat;
  receipts label the observed origin. Model status distinguishes loopback
  transport from verified managed-local inference.
- New local coding runs now save repository snapshots and check evidence in the
  chosen local storage folder, instead of filling the legacy state drive after
  models were moved. Older receipts still reopen from their original path.
  A reused run ID is refused across both locations so old evidence cannot hide
  a new receipt.
  Windows admission checks minimum snapshot and check space before inference;
  its estimate does not cap build outputs.
- Managed setup and downloads now refuse a chosen folder whose Windows
  directory or volume identity changed. Live launch receipts also recheck the
  runtime, model and blob directories; a previously used folder without a
  verifiable managed launch cannot fall back to an unchecked default-endpoint
  download.
- Hosted Rust verification now resolves touched packages from the selected
  repository, runs their tests against the candidate, and records the Test
  layer only when a completed test is shown. Library-only Cargo handoffs no
  longer suggest `cargo run`.
- Local plan review now binds the selected calibrated model and full profile
  fingerprint. A changed selection or calibration is refused before a run
  attempt starts; Desktop shows the reviewed model instead of a live replacement.
  `--reviewed-plan FILE --sha256 HASH --yes` checks the complete saved plan
  against an independently recorded file hash across CLI invocations, with
  host-check approval required again.
- Final review now rechecks the saved baseline after host-approved candidate
  checks. Changed or unreadable baseline bytes clear selection instead of
  reporting a review-ready candidate that Apply would reject.
- Local Apply and Rollback now require the current repository to match the
  saved run's canonical repository before acquiring state or changing files.
  CLI commands use the current directory; Desktop queries canonical matching
  before showing actions and sends the selected project again on mutation.
- Managed model downloads now bind to the original live Ollama process and its
  unredirected blob store, then require a fresh manifest estimate plus disk
  reserve before pulling. Stale managed receipts refuse the pull; explicitly
  configured external runtimes remain labeled unverified.
- Model status now reports whether this engine supports managed runtime setup,
  so Desktop can hide unsupported actions without inferring the OS from a browser
  user agent. Managed setup still supports Windows x64 only.
- Managed setup now measures its own runtime volume, serializes startup with
  installation, and verifies its spawned child owns the loopback listener when
  the version probe succeeds. An already responding service is labeled as an
  unverified origin rather than a Phonton-started runtime.
- Managed setup refuses an occupied but nonresponding default port before the
  large download and rejects linked model-store or log paths at startup.
- Local strategy and edit calls now refresh runtime version, selected-model
  digest and installed size, and re-estimate context-specific cold-load fit from
  current hardware. If cold loading stops fitting, inference requires the exact
  resident model, stable allocation when already pinned, and host headroom.
  Failed admission records a receipt state before reserving a call.

### Added

- The Desktop sidecar exposes `goal.active`, a read-only inventory of live
  legacy goal sessions. A reloaded Desktop view can check server-side work
  before a project switch restarts its engine.
- Windows managed setup now lets CLI and Desktop choose an existing empty
  local-drive folder for runtime and model files before installation. Status
  shows the planned path and drive space; old or interrupted managed stores
  are preserved instead of being silently moved.
- Calibration now prefers an editing format that also passes its new-file
  fixture when multiple edit formats pass; each passing edit format gets a
  creation probe, and the raw outcomes remain inspectable.
- `PHONTON_CONFIG_PATH` selects a per-process config file for isolated provider
  testing without changing the normal Phonton config.
- CLI request files require `--allow-host-checks` on the current invocation
  for host setup/checks; saved approval cannot silently authorize a later run.
- Hosted headless goals now carry the same explicit host-check decision into
  the worker's first verification and the orchestrator's independent pass.
  Previously the worker could fail closed before the approved pass ran.
- Referenced test files and verification manifests no longer become required
  edit targets merely because a goal asks for a source fix and test pass.
- Semantic indexing now uses an absolute user cache for embedding weights;
  relative or repository-local cache overrides refuse indexing against the
  actual indexed repository, even when the process starts elsewhere.
- Host verification refuses case aliases of manifests and Node-discovered test
  filenames before running candidate-authored checks.
- DeepSeek defaults, tier routes and model discovery preference now use the
  current `deepseek-flash` and `deepseek-v4-pro` API IDs.
- Installed-model inventory now omits unusable Ollama rows individually and
  exposes bounded diagnostics in status; valid models remain available.
- DeepSeek budget estimates now price Flash and V4 Pro separately at current
  peak cache-miss rates, including escalation. Unknown model pricing cannot
  silently count as free under a dollar cap; cost displays say estimate.
- Unified-diff edits can now match ordinary model diff lines against a CRLF
  source file while preserving its CRLF endings in the candidate.
- Catalog manifest lookups now run concurrently with stable row order and
  per-model errors; a slow registry entry no longer serializes the others.
- New-file plans with no `--files` search for matching existing source and
  show it as read-only context. A no-match plan keeps the target explicit and
  warns that the creation prompt has no repository excerpt.
- Explicit `goal --local rollback RUN_ID --yes` and Desktop original-file
  restore for schema-1/2 Apply journals. It validates the saved candidate,
  original-byte backups, source set and Git index, then journals a resumable
  sequential rollback. Unknown edits or damaged evidence refuse the operation;
  on Windows an applied single-created-file journal with a retained identity
  anchor can remove its exact published file. Applied schema-4 mixed changes
  now restore original existing source before removing the witnessed new file,
  with exact partial-state recovery. Old byte-only and non-Windows creation
  journals remain manual.
- Failed edit calibration now returns CLI exit code 2 after saving and printing
  its probe JSON. Model status exposes same-endpoint saved probe evidence
  separately from the valid-only selectable profile.
- A live-manifest-backed, resource-only first model to try in CLI/Desktop
  catalog output. It withholds the hint when fit evidence is unavailable or
  memory is insufficient; download, calibration and selection stay explicit.
- Mixed local goals can create one explicit file and edit up to three reviewed
  existing files in a staged, shared-budget candidate. Checks and selection
  require the combined snapshot; schema-4 Apply journals creation and source
  replacements for exact-byte roll-forward after interruption.
- Root npm test plans with a captured registry lockfile now show bounded offline
  `npm ci --ignore-scripts` preparation. Setup is separately journaled and
  budgeted; unavailable dependencies cannot count as passing verification.
- Baseline restarts now reserve a source-anchored strategy call before
  another edit. Receipts retain its raw reply, context, usage and rejection;
  strategy and edit both reserve from the same model-attempt budget.
- Read-only planning discovers JavaScript/TypeScript module extensions and
  proposes `go test -json -count=1 ./...` for scoped Go source in a root Go
  module. Go test files and dependency manifests are protected from model edits.
- Inferred checks now follow editable source scope. Nested Go/Node packages
  and mixed-language edits require an explicit check; Rust members get an
  owning-manifest check. Conventional Node test filenames and `test/` paths
  are protected even when they contain only top-level assertions.
- Recoverable Windows managed-runtime download and extraction stages with a
  runtime-root lease, pinned archive and extracted-file-tree verification, and
  no-replace publication. Linked archive paths, network/device roots and unknown
  or legacy incomplete directories are refused or preserved for inspection.
- `models endpoint [ORIGIN]` and a matching Desktop RPC for guarded loopback
  Ollama configuration. Switching origins clears the active model and keeps
  calibration evidence keyed to its original endpoint.
- Per-model automatic calibration context from current memory and the installed
  model's reported ceiling. Status labels the context actually estimated;
  unknown readings yield no recommendation, and explicit overrides stay exact.
- A separate selected-transport new-file calibration probe with raw output, usage and
  status; creation readiness no longer inherits an existing-file edit result.
- Model status withholds stale profiles after a digest, endpoint or runtime
  change and explains why recalibration is required.
- One explicitly named `--new-file` target in a local goal. The plan records
  absence; candidate review shows a `/dev/null` diff. Existing scoped files
  are read-only context. Schema-3 guarded apply journals a same-parent
  temporary and publishes only if the target remains absent, untracked and
  unchanged; interrupted creation has exact-byte recovery.
- A calibrated model already resident in Ollama at the exact digest and required
  context can be reused under the cold-load memory estimate while at least
  1.5 GiB host RAM remains available. Ollama must report at least 30 seconds
  before unload. Residency is checked before each generation and immediately
  before chat, and recorded in the receipt when it affects admission.
- Live-manifest-backed Qwen3.5 4B model browsing. Editing compatibility still
  requires measured calibration before selection.
- Shared hardware observations, registry manifests, managed Windows Ollama setup,
  real model download progress, calibration and digest-bound model selection.
- `goal --local --request` candidate execution with explicit source scope,
  immutable checks, serial search budgets, canonical diffs and persistent receipts.
- Separate Passed, Failed, Not run and Unavailable states in local run evidence.
- Bounded source retrieval within explicit scope, with exact excerpts, hashes,
  selection reasons and schema-aware context admission in candidate receipts.
- Compact parsed functions can be replaced as exact blocks, preserving surrounding
  source; line choices remain available within the calibrated context limit.
- Module-style JavaScript/TypeScript files (`.mjs`, `.cjs`, `.mts`, `.cts`) use
  parsed function spans too. Named failing-check evidence ranks the matching
  symbol and its constrained edit choice ahead of other scoped functions.
- Evidence-driven repair lineage, rejected-edit feedback and complete baseline
  diffs for child candidates; repeated identities and unavailable checks stop search.
- Identical rejected raw proposals that never materialize a checkable candidate
  stop after the second occurrence instead of spending the remaining generations.
- Shared read-only scope/check previews and persisted GoalContracts. Text goals
  work through `goal --local`; `--plan` previews without inference, `--yes` accepts
  scope, and host execution still needs its own flag. Reviewed source hashes must match.
- Explicit `goal --local apply RUN_ID --yes` for the complete selected, verified
  changed-file set. Source, candidate and Git index identity are rechecked;
  per-file backups and a schema-2 prepared journal support bounded roll-forward
  after interruption. Existing schema-1 one-file journals remain readable.

### Fixed

- Apply now requests a directory sync after writing each recovery journal and
  newly published backup or creation anchor, and syncs the run-directory parent
  before recording Apply state. It requests sync of changed project directories
  before acknowledging Apply or rollback. A failed post-publication sync leaves
  a nonterminal journal for recovery; full power-loss recovery is not claimed.
  Mixed Apply syncs its new-file directory before editing existing files, and
  mixed rollback syncs restored source directories before its deletion marker.

- macOS hardware detection now estimates available RAM from validated `vm_stat`
  page counts, so fit and calibration admission need not stay Unknown solely
  because the platform supplied only total RAM. Unreadable or impossible
  counts still leave fit Unknown; inactive pages may need paging.
- Local candidate prompts now use the same slash-normalized scoped path as
  their edit constraints when CLI or request-file input uses Windows separators.

- Exit-zero Cargo test checks with missing or malformed completed-test summaries
  now remain Not run instead of counting as passing verification.
- `models status --json` now works as an explicit alias for its existing JSON
  output, and `models status --help` shows usage instead of parsing as context.
- Inferred Rust checks now target the edited Cargo package instead of consuming
  the local run's check and wall budget on unrelated workspace packages.
- Saved local receipts now disclose when selected Cargo package checks omit
  dependent packages, even when the run bypasses plan preview.
- Completed local runs now save a durable end receipt before exposing review
  readiness through the CLI service. Reopening a saved success retains its
  selected candidate; a review without an end record cannot be applied.
- The default local check budget now covers baseline and candidate verification
  for up to four selected checks plus offline preparation. Planning warns about
  a smaller explicit budget, and approval refuses it before host checks run.
- Local goals no longer persist a review-ready selection before final model,
  source, candidate and Git-index identity checks. Reopening an unfinished or
  legacy prematurely ready receipt without an end record shows Interrupted.
- Rust source with a separable inline-test tail is editable before that tail;
  candidate test bytes stay immutable, while ambiguous layouts remain protected.
- Model selection and removal now finish before reporting their result when
  cancellation is requested; setup, install and calibration report an uncertain
  partial outcome and direct users to refresh status. Installed-model metadata
  reads share a five-second budget, prioritize the selected model and preserve
  per-row failures instead of accumulating one timeout per model.
- A managed setup retry waits for an abandoned archive hash worker to release
  its Windows file handle before removing a Phonton-owned download stage.
- Replaying a legacy one-file Apply journal now refuses a missing or corrupt
  original-byte backup before reporting an already-applied candidate.
- Existing UTF-8 source without a final newline is now discoverable and
  editable through both local edit formats. Candidate materialization preserves
  its exact EOF state, including a literal terminal carriage return; review
  diffs mark unterminated lines explicitly.
- Hosted checkpoint commits now include only verified paths on top of the
  previous checkpoint, excluding unrelated staged files. Checkpoint failure
  fails the task instead of leaving an applied change review-ready.
- Legacy checkpoint rollback now refuses the destructive reset that could
  discard unrelated work. The unused stash-and-drop transaction API was removed.
  The TUI presents checkpoint history without a rollback shortcut; local
  Apply-journal rollback remains separate.
- `review approve` now requires a Reviewing task with a verified change
  payload whose checkpoint, staged files and worktree still match in the
  current Git repository. Failed, running, empty, missing-checkpoint and
  drifted tasks cannot be marked Done. Hosted reject explicitly reports that
  already staged edits remain for manual handling.
- Headless hosted goals require explicit `--allow-host-checks` for host verification; the previously ignored `--permission-mode` now fails with guidance.
- Resuming a hosted goal now requires its original workspace, including when host checks are explicitly approved.
- New-file and mixed Apply recovery now requires a retained same-volume file
  identity anchor as well as selected bytes. The anchor is journaled before a
  project temporary exists, and publication uses that anchor. An identical
  replacement and older byte-only journals refuse automatic roll-forward;
  repositories on another volume can use an ordinary local `.git` anchor.
- Guarded Apply on Windows holds every changed source parent against rename or
  reparse-point substitution during path-based replacement, including mixed
  create-and-edit goals. Sequential batch recovery and file-level race limits
  remain explicit.
- Windows approved checks now start suspended, join their cleanup Job Object
  before running project code, and close that job as soon as the direct check
  exits. Cancellation and completion no longer leave an early or pipe-holding
  descendant outside the supervised process tree.
- Failed new-file and mixed create/edit candidates can now repair the created
  source inside the reviewed candidate scope and rerun complete checks. A run
  that spends its last model/check budget without a passing candidate reports
  budget exhaustion instead of implying further repair occurred. Repair keeps
  the created source in context, and all changed source is screened for test
  definitions before project checks.
- Guarded Apply now accepts a verified npm candidate with separately passing
  offline preparation, while refusing missing, failed or mismatched setup.
  A staged creation whose follow-up source context cannot fit ends with a
  saved stop reason before another model request.
- Successful Python `unittest` runs with only skipped cases or no runner summary
  now record **Not run** rather than passing verification. Mixed passing/skipped
  cases keep their executed-test evidence; expected-failure-only cases are now
  classified **Not run**.
- Direct `node --test` checks, including commands proposed from simple root npm
  scripts, no longer count skipped-only, empty or unmatched-filter runs as
  passing verification.
  An explicit TAP reporter is required because spec output can contain forged
  test-like lines from the code under test. Inferred direct Node checks request
  TAP; supported npm scripts propose the direct Node command because npm can
  substitute the script shell or executable. Explicit `npm test` remains
  diagnostic-only on exit 0. Checks without a usable TAP count remain
  **Not run** with output and exit code preserved. An inferred root `npm test`
  with no supported runner is withheld, and an explicitly selected one stays
  **Not run** on exit 0 even if it prints a TAP-looking summary. Root npm
  recognition is limited to a literal
  Node runner or simple `npm run` delegation; hooks and shell chains stay opaque.
- Successful Cargo test commands that only compile or list tests, or report
  zero executed tests, now record **Not run** with their actual output and exit
  code; they cannot make a candidate review-ready by themselves.
- Successful Go checks now require uncached JSON evidence of a completed named
  leaf test. Plain summaries, skipped-only suites, cached results, empty
  packages, and list, compile-only or dry-run checks record **Not run** with
  command output preserved; they cannot make a candidate review-ready.
- Local runs now compare the original Git-visible tracked and untracked file
  inventory as well as captured bytes after baseline checks, before generation
  and at final review. A changed or unreadable original inventory clears review
  readiness. Managed Ollama startup reuses an existing recorded installation
  before applying the fresh-install disk reserve.
- Desktop `goal.start` now requires stored workspace trust, uses deny-by-default
  MCP approval, publishes early planning failures to goal status, and closes its
  event writer after draining without a routine five-second wait. Opening a
  project no longer grants trust automatically.
- Generic receipt-refactor goals retain the planner's task and stack-aware
  checks instead of being replaced with a hardcoded Node benchmark plan.
- Selected npm-family scripts, their lifecycle scripts and chained package
  scripts are protected without freezing unrelated scripts; unresolved workspace
  dispatch and Corepack are refused. Ordinary `RegExp.test()` calls remain editable.
- Local run scope validation now rejects direct and npm-transitive check scripts,
  extensionless module aliases, conventional test/build configuration, embedded
  tests in source, and opaque shell or inline-code check launchers. Passing checks
  remain bounded evidence, not a proof of independent test definitions. Absolute
  check paths into the original repository are refused so candidate checks cannot
  accidentally run against the original tree.
- Desktop goal titles no longer panic when truncation crosses a Unicode character;
  the CLI service now keeps whole characters in the shortened display text.
- Goals that end before their first full receipt now retain a small attempt
  record, so restart recovery shows the actual preflight refusal or an
  interrupted-before-receipt state. Later deterministic failures are labeled
  separately from cancellation. Client-selected run IDs survive a lost start
  acknowledgement; unsafe evidence paths inside the repository are refused
  before any local-state write.
- CLI download output coalesces ordinary byte updates without hiding stage changes,
  completed layers, backwards progress or unknown totals.
- Original Git index identity is observed separately from source hashes. Changes
  or unavailable observations stop local runs without automatically restoring it.
- Repair feedback prioritizes failed checks over copied edits and retains the
  last checkable failure after repeated unchanged or malformed proposals.
- Repository discovery caps Git output during reading, so an oversized listing
  cannot consume unbounded memory before inventory validation.
- Inferred npm verification uses the Windows command shim on Windows, matching
  the existing verifier.
- Search recognizes unchanged Node failures across candidate directories and
  timing differences, while preserving raw check output and assertion values.
- Local unittest checks that report zero collected tests are marked Not run,
  retaining the process exit code and output without treating collection as coverage.
- Compact function replacements preserve their surrounding line separators and
  remain available after an unchanged proposal, keeping retries on complete targets.
- Source indexing truncates UTF-8 on character boundaries instead of panicking
  when a symbol signature contains multibyte text at the byte limit.
- The Sandbox executor fails closed when containment is unavailable;
  explicitly approved host checks are labeled separately from isolation.
- Filesystem guards resolve existing ancestors and refuse traversal; workers
  cannot execute approval-required tools by treating them as ordinary allowed calls.
- Desktop RPC binds to loopback, checks browser origins/hosts, and keeps background
  model and local-run operations alive after the starting request completes.
- Legacy Cargo/npm/browser verification now requires execution authority and uses
  the shared executor; static-only checks and unavailable processes cannot pass.
- Unavailable and not-run verification stops without model escalation and keeps
  its evidence in the handoff. Non-Git apply uses the same strict patch rules as
  Git apply and reports write errors instead of silently losing content.
- Browser smoke helpers stay outside candidate source; installed Chrome/Edge can
  supply the browser when Playwright's bundled executable is unavailable.
- Diff verification and application reject invented old-side text, inconsistent
  hunk counts and fuzzy context; insertion hunks preserve existing file contents.

## 0.21.2 - Pixel marks

### Changed

- README and npm package logos use the pixel CLI mark. The TUI splash is still ANSI Shadow.

## 0.21.1 - CostReceipt on the serve line

### Added

- `CostReceipt` and `RouteStep` on `GlobalState` and `HandoffPacket` so Desktop and `phonton review` show actual cost vs a labeled frontier counterfactual.
- TUI savings line reports spend vs frontier when a receipt is present.
- `phonton serve` JSON-RPC (`goal.status`, `review.get`) serializes the new receipt fields.

### Changed

- Escalation uses per-tier model ids. A configured cheap model is not reused as Standard/Frontier.
- Missing provider keys fail closed. Stub diffs are not a verified success.
- Local-template seeds match only the subtask text after a memory preamble. A failed seed falls through to the configured provider.

## 0.21.0 - Desktop serve RPC and CORS

### Added

- JSON-RPC methods for Phonton Desktop: `tasks.list`, `tasks.get`, `workspace.info`, `config.get`, `config.save`, `config.path`, `trust.list`, `trust.grant`, `extensions.list`, `extensions.read`, `extensions.write`, `extensions.validate`.
- CORS headers on all `phonton serve` routes for Tauri webview compatibility.

## 0.20.1 - Tier 2 CLI, benchmark harness, provider routing

### Added

- Persisted pause/resume: `paused_runs` in `phonton-store`, `phonton goal --resume <task-id>`, TUI footer hint when paused.
- `classify_task_confidence` heuristics; `phonton index watch` for incremental semantic indexing.
- DeepSeek default pricing in budget guard; `phonton doctor` bench Python check.

### Fixed

- `syntax-preflight-v1` capture resolves Python on Windows (`PHONTON_BENCH_PYTHON`, `py -3`, etc.).
- Receipt refactor and chess acceptance slices: stronger preflight plans and paired file context.
- Verify layer surfaces first tree-sitter syntax line in worker retry prompts.

### Notes

- Provider-only RunIndex 38 report: `benchmarks/reports/deepseek-2026-06-01-v0.20.1.md` (benchmark workspace).

## 0.20.0 - Merge-gate messaging and honest execution labels

### Added

- Workspace-aware Ask (`ask_context`) with goal and repo path context.
- TUI `execution:` line: `provider`, `local-template`, or `mixed` from flight log models.
- Receipt block prioritized when a handoff exists (before in-flight worker noise).
- Brain `CURRENT.md`, rewritten vision/positioning, `phonton-dev` getting-started + concepts.
- DeepSeek provider-only benchmark report (`deepseek-2026-06-01-phonton-v0.19.7.md`).

### Changed

- Public README and site hero emphasize merge gate, not token leaderboard copy.
- Website `/desktop/` waitlist page (MVP scope only).

### Notes

- Tier 2 backlog: persisted pause/resume, LLM `classify_task`, incremental index.
- Chess `chess-web-v1` provider-only: see benchmark report after RunIndex 37 capture.

## 0.19.7 - Local Template Dispatch And Launch Benchmark Harness

### Fixed

- Restored `phonton-worker` local template dispatch for existing-Vite chess rules seed, syntax preflight repair, and receipt refactor slices. Templates under `phonton-worker/src/templates/` are applied with zero provider tokens unless `PHONTON_DISABLE_LOCAL_SEEDS=1`.
- Chess rules test seed no longer imports `vitest`; self-executing assertions keep `npm test` deterministic during verification.
- Added `phonton_memory::tests::benchmark_latency_concurrent` so `C:\benchmark-phonton` memory-latency capture can run `cargo test` successfully.

### Notes

- Provider-only benchmark runs must set `PHONTON_DISABLE_LOCAL_SEEDS=1`; those runs are reliability evidence, not token-comparison evidence.
- Public efficiency claims still require `token_claim_eligible` artifacts per suite.

## 0.19.6 - Interactive Clarification Questionnaire

### Added

- **Interactive Clarification Questionnaire**: Introduced `Mode::Clarify` to guide users step-by-step in answering outstanding requirements questions when a goal's confidence is under `70%`.
- **Dynamic Prompts & Rerun**: Users can answer clarification questions directly in the TUI, which appends answers to the goal description and triggers a clean planning rerun (wiping stale checkpoints and flight logs).
- **Unit Test Coverage**: Added a comprehensive TUI test verifying keyboard event routing, buffer editing, answer stacking, and re-queue intent generation.

## 0.19.5 - Confidence Gate for Under-Specified Goals

### Added

- **Goal Contract Confidence Gate**: Implemented a robust confidence and clarification questions gate in `phonton-orchestrator`'s `run_task` loop. If a goal's confidence is under `70%` or carries unanswered clarification questions (such as broad, stackless goals like `"make chess"`), the orchestrator immediately halts execution, surfaces the clarification questions under the `Problems` focus, and outputs a clear `TaskStatus::Failed` to protect provider token budgets.

### Fixed

- **Token Waste Prevention**: Stopped the worker loop from executing broad under-specified placeholders blindly. The regression fixture covers an observed 12,456-token placeholder loop; do not use that fixture result as a broad public token-efficiency claim.

## 0.19.4 - Double-Line Splash Logo & RAII Browser Verifier Polish

### Added

- **RAII Browser Check Guard**: Implemented a robust `BrowserCheckGuard` struct in `phonton-verify/src/browser.rs` to guarantee clean deletion of temporary files (`phonton-server.js` and `phonton-playwright.js`) and prompt shutdown of background static server child processes across all exit paths, eliminating zombie background Node process leaks on the host system.

### Fixed

- **Reverted Splash Logo to Double-Line Wordmark**: Reverted the TUI splash logo to the classic 7-line box-drawing shadow glyph wordmark (`██████╗` / `╚═╝`) as requested.
- **Removed Shimmer Animation**: Set the scanline shimmer phase to a static `0.0` inside `render_splash`, completely removing animation refresh glitches while preserving the premium color gradient.

## 0.19.3 - Solid-Block TUI Splash & Silent Embedder Downloads

### Fixed

- Replaced double-line box-drawing characters in the TUI splash logo with a robust, solid block-character wordmark to resolve visual font rendering bugs and vertical line overlaps.
- Added strict suppression to standard output progress indicators during sentence-embedding ONNX model downloads to prevent input line corruption in TUI sessions.

## 0.19.2 - TUI Version Display & Auto-Update Engine

### Added

- Added Phonton CLI version number display to the TUI sidebar "System" panel.
- Added general configuration setting `general.enable_auto_update` (also aliased to camelCase `enableAutoUpdate` to mirror Gemini CLI).
- Added an asynchronous background auto-update engine that queries the npm registry at startup and schedules a global detached background npm update upon clean TUI exit (solving Windows file lock issues).

## 0.19.1 - Shadow Logo Restore

### Fixed

- Restored the standard ANSI Shadow Phonton ASCII splash logo with the full 7-line block-border wordmark and `░▒▓█...▓▒░` glow strip.
- Upgraded the `logo_line` shader to dual-wave shimmer with separate styling for block borders (`╔═╗║╝╚`), shadow glyphs (`░▒▓`), and solid fills.

## 0.19.0 - Swarm Plans, Index Backends & MCP Capability Preview

### Added

- Added typed swarm planning metadata to `PlannerOutput` through `PlanGraph`, `SubtaskAssignment`, and conflict-group records. Plan preview JSON now includes swarm mode, swarm reason, assignments, conflict groups, and the selected index backend.
- Added broad-goal swarm activation. Provider-backed planning can emit swarm metadata for large goals; deterministic fallback planning records why swarm execution is disabled when no provider-backed decomposer is available.
- Added conflict-aware dependency normalization in the orchestrator so subtasks with overlapping expected touch scopes are serialized while independent subtasks still run through the existing concurrent executor.
- Added a `CodeRetriever` abstraction in `phonton-index`, preserving local HNSW as the default and adding an optional Qdrant HTTP backend for code and symbol retrieval.
- Added `[index]` config with `backend = "local-hnsw"` by default and optional `backend = "qdrant"`, `qdrant_url`, and `qdrant_collection` fields.
- Added `phonton mcp capabilities <server-id> [--json] [--yes]` to preview initialize metadata, tool descriptors, and proposed sandbox permission rules without invoking tools or writing config.
- Added MCP capability discovery telemetry and shared typed records for `McpCapabilitySnapshot`, `McpToolDescriptor`, and `McpPermissionProposal`.

### Changed

- `phonton doctor` now reports the configured index backend and probes Qdrant connectivity when Qdrant is selected. Phonton does not start or manage Qdrant containers.
- Outcome/context evidence now carries plan graph and selected index backend data where available. SQLite keyword memory remains the authoritative decision memory store; Qdrant is only a code retrieval backend.
- README release language now describes v0.19.0 shipped commands only and avoids unbacked numeric or competitor benchmark claims.

### Notes

- v0.19.0 is an alpha slice of the swarm/index/MCP direction. It does not add branch racing, automatic Qdrant lifecycle management, external memory-record migration, or silent MCP permission scaffolding.
- Public efficiency claims still require complete reproducible benchmark artifacts: pinned fixtures, exact prompts, tool versions, provider token usage where available, raw logs, final diffs, verification logs, and handoff evidence.

## 0.18.0 - Playwright Verification, Surgical Repairs & Memory Provenance

### Added
- **Playwright Verification Integration:** Spawns a zero-dependency non-interactive background Node static server and headless browser script to assert rendering, console errors, and simulate DOM interactions (e.g. counter click, chess pieces click) with screenshot-evidence summaries.
- **Surgical Missing-Criteria Repair (Low-Token Guard):** Engineer a sub-1k token target-exceeded repair prompt feeding only exact failing files, line numbers, verifier diagnostics, and failed criteria on retry to avoid expensive full-context repairs.
- **Memory Rules & Provenance:** SQLite active enforcement of custom constraints and conventions during verification (`verify_decisions`) and planning (`decompose_with_memory` system prompt) with task ID provenance.
- **Widen SubtaskResult Cost Tracking:** Model and provider token usage tracked end-to-end to align BudgetGuard USD calculations with actual EWMA model metrics.

## 0.17.1 - DeepSeek-TUI Benchmark Enrolment

### Added
- Officially integrates and supports **Hunter Bown's DeepSeek-TUI (v0.8.39)** directly in the benchmark setup and capture harness across all active benchmark suites.

### Fixed
- Hardened formatting boundaries across Rust changed files to prevent CI checklist checker regression errors.

## 0.17.0 - Dynamic Keys, Credentials Autopilot & HNSW Vector Memory

### Added
- **Dynamic Key Map:** Configure multiple provider API keys inside `config.toml` under the `[provider.keys]` section.
- **Dynamic Key Resolver:** Dynamically resolves `{PROVIDER}_API_KEY` environment variables (e.g. `DEEPSEEK_API_KEY`, `ANTHROPIC_API_KEY`) and maps provider tiers to model defaults.
- **Credentials Autopilot:** Command `phonton providers import-opencode` scans standard app folders, parses credentials, and safely merges them into config.
- **High-Speed HNSW Semantic Search:** Local vector similarity queries in under 160µs query latency using `usearch` and `fastembed` with `all-MiniLM-L6-v2`.
- **AST Syntax Preflight Check:** Layer 1 tree-sitter AST validation checking changed files for syntax errors before committing.

## 0.16.2 - Benchmark-Honest Headless Runs

### Added

- Headless `phonton goal --prompt-file <path> --json --yes` runs now capture bounded baseline test evidence before editing when the prompt asks to run tests first. The evidence is appended to the dispatched goal as repair context without depending on TUI paste, clipboard, or PTY automation.
- Goal contracts now extract prompt-mentioned source files and public API signatures such as `src/receipt.js` and `buildReceipt(run)` into likely files, expected artifacts, acceptance criteria, and concrete verify-plan commands when the prompt names test commands.

### Fixed

- Headless baseline capture now uses Windows command shims for Node package tools such as `npm.cmd`, preventing false "program not found" baseline evidence on Windows.
- Verified diffs no longer discover and checkpoint a parent git repository when the current workspace folder is not itself a git root. In nested benchmark work folders, Phonton now falls back to direct workspace-local hunk application instead of staging unrelated parent-repo files.
- Benchmark export now marks mixed local-template/provider runs as `token_claim_eligible: false` even when provider token usage is available. Mixed runs remain useful product evidence, but they are not provider-token efficiency wins.
- Handoff verification summaries now prefer concrete planned commands such as `npm test passed` or `npm test failed` over trailing prompt prose when benchmark prompts are submitted through `--prompt-file`.

### Notes

- v0.16.2 is a benchmark reliability release. Do not claim Phonton beats Cursor, Claude Code, Codex, Gemini CLI, Hermes, BridgeSpace, or other ADEs unless the published artifact set is complete and each included run has `token_claim_eligible: true`.

## 0.16.1 - Extension Installer

### Added

- Added `phonton goal [--prompt-file <path>|--stdin|<goal>]` for noninteractive goal execution through the same planner, worker, verifier, handoff, memory, extension, MCP, and OutcomeLedger path used by the TUI. It supports `--json`, `--yes`, `--permission-mode <mode>`, `--timeout-seconds <n>`, and `--task`.
- Added `phonton extensions install <source>` for installing `.phonton` extension packs from GitHub, local paths, or built-in open-source MCP catalog ids. The installer supports `--scope workspace|user`, `--ref <ref>` for GitHub packs, `--dry-run`, `--force`, and `--json`.
- Added `phonton extensions catalog` to list open-source MCP manifest recipes for GitHub, Context7, Chrome DevTools, Playwright, Firecrawl, Supabase, MongoDB, and Figma/Framelink.
- Added `phonton extensions new <path> [skill|steering|mcp-server|profile]` for scaffolding small auditable `.phonton` extension packs.
- Added `phonton extensions validate` as a Gemini-style alias for `phonton extensions doctor`.
- Added `.phonton/mcp.d/*.toml` loading so catalog installs can write one MCP manifest per extension instead of overwriting a shared `mcp.toml`.
- Added benchmark export comparability fields: `execution_mode`, `token_usage_source`, `provider_call_count`, `token_claim_eligible`, `benchmark_warnings`, and `cache_creation_tokens`.
- Added file-backed cargo verification locking under `target/.phonton-locks` so concurrent Phonton processes share the same cargo-target guard instead of relying only on in-process mutexes.

### Fixed

- `phonton benchmark export` now preserves failed provider identity and local-template runs instead of dropping them or treating zero-token local-template successes as provider-token wins.
- Memory retrieval now uses light stemming, engineering-term synonym expansion, stopword filtering, and IDF-weighted overlap instead of raw keyword intersection.
- Windows sandbox job-object assignment failures now produce an explicit nested-job fallback warning while keeping direct-child `kill_on_drop` behavior.

### Notes

- v0.16.1 keeps extension installs local and inspectable. Installing an MCP entry writes configuration only; MCP servers still require workspace trust and approval before tools can affect a run.
- `phonton goal --prompt-file` is intended for benchmark and CI harnesses where exact multiline prompts must not depend on terminal paste behavior.
- Public token-efficiency claims should only use verified provider runs with `token_claim_eligible: true`; estimated, local-template, mixed, unavailable, and failed runs are still useful evidence but not headline token comparisons.

## 0.16.0 - Addressable Context And Extension Evidence

### Added

- Added typed `@...` context mention records for files, directories, symbols, MCP servers, and MCP tools. TUI goal runs now resolve mentions before dispatch and show resolved, missing, and approval-gated rows in `/context` and the Context focus surface.
- Added local mention token estimates using the tokenizer-backed context counter when available, with directory mentions represented as bounded summaries instead of recursive dumps.
- Added explicit `phonton why-tokens --by-source` output lines for local prompt estimates, provider-reported input/output/cache usage, provider usage source, attribution-only `@...` mention totals, and `no provider call` local-only runs. Prompt manifest events also carry the resolved mention rows for audit surfaces without adding them to the total twice.

### Changed

- MCP mention resolution uses configured extension metadata without starting or calling servers. Networked or mutating MCP tools remain approval-gated.
- `phonton extensions doctor` now treats networked and mutating MCP trust levels as approval-gated even when the server has not declared explicit permissions.
- README install examples and release markers now target v0.16.0.

### Notes

- v0.16.0 positions Phonton as a proof-carrying ADE built around visible context, verification, and handoff evidence. It does not claim superiority over Cursor, Claude Code, Codex, HermesAgent, BridgeSpace, or other ADEs without reproducible benchmark artifacts.

## 0.15.5 - TUI Prompt And Ask Formatting Hotfix

### Fixed

- Bare prompt-bar letters such as `f`, `d`, `p`, and `r` now type normally instead of triggering focus, diff, problems, or retry shortcuts before the user can start a sentence. Those shortcuts now use `Alt+F`, `Alt+D`, `Alt+P`, and `Alt+R`.
- The prompt bar now renders a static caret rectangle in the input area while keeping the native terminal cursor hidden, avoiding the previous blinking cursor regression without leaving the input position invisible.
- Ask-mode answers now render common inline markdown styling for bold, italic, and inline code spans instead of displaying raw `**bold**`, `*italic*`, and `` `code` `` markers.

## 0.15.4 - Vite Chess Quality Gate Hotfix

### Fixed

- Existing Vite/React chess App shell seeds now render an accessible named-piece legend for king, queen, rook, bishop, knight, and pawn, so the diff-based playable chess quality gate sees explicit piece evidence instead of failing after a locally verified zero-token UI seed.
- The seeded App test now asserts named chess piece evidence, preventing regressions where the local App shell is playable at runtime but too implicit for Phonton's review-quality diagnostics.

## 0.15.3 - Vite Chess App Test Hotfix

### Fixed

- Existing Vite/React chess UI seeds now replace stale `src/App.test.*` placeholder assertions when those tests already exist, preventing local zero-token App shell seeds from failing verification against old heading expectations.
- The seeded App test uses React server rendering plus Vitest assertions, so it does not require Testing Library, jsdom, or custom matcher setup.

## 0.15.2 - Vite Chess Token And Cursor Hotfix

### Fixed

- Existing Vite/React chess App/UI slices now seed `src/App.tsx`, `src/App.css`, and `src/vite-env.d.ts` from a locally verified playable chess shell instead of spending a provider call on fragile generated CSS hunks.
- Repeated existing Vite chess UI slices become zero-token no-ops once the local shell is already current, avoiding noisy remove/add copies of the same files.
- The TUI no longer requests a native terminal cursor during frame renders, removing the blinking prompt/header bar while keeping the animated compact `phonton` gradient.

## 0.15.1 - TUI And Vite Chess Hotfix

### Fixed

- Existing Vite/React chess rules seeds now declare a real Vitest suite instead of relying on self-executing assertions in `src/chessRules.test.ts`. This fixes Vitest failures that reported `No test suite found in file`.
- The compact `phonton` header gradient animates while a goal is active again.
- The Active panel focus tabs use compact labels for Commands, Context, and Tokens so the Receipt `d diff` shortcut hint does not wrap onto a stranded line at common terminal widths.
- Flight Log scrolling now works from tail mode with `PgUp`, arrow keys, and the mouse wheel instead of staying pinned to the newest event.

## 0.15.0 - Summary-First Proof Candidate

### Added

- Added deterministic proof summary types for Plan, Work, Verification, Failure, Token, Context, and Handoff evidence. These summaries are derived from typed GoalContract, OutcomeLedger, ContextManifest, PermissionLedger, VerifyReport, and HandoffPacket data instead of another model call.
- Expanded the Active panel focus cycle to Plan, Receipt, Problems, Code, Commands, Context, Tokens, and Log so broad work, context influence, and token spend are visible from the main TUI surface.
- `phonton proof export --latest --format json`, `phonton review --json latest`, and benchmark export now expose deterministic summary bundles alongside the underlying typed evidence.
- Added `/plan <goal>` and `/approve` in the TUI so broad work can be previewed as a GoalContract before execution.

### Changed

- OutcomeLedger persistence now carries context bucket evidence, selected index slices, MCP permission records, command-run evidence, and derived summaries for history, review, proof export, and future benchmark exports.
- Runtime/browser verification plans that do not produce runtime proof now become explicit verification findings and known gaps in the HandoffPacket instead of being implied as successful by syntax/build/test checks.
- `phonton run latest` records sandboxed command execution evidence back into the task PermissionLedger, including command, cwd, exit status, and duration.

### Notes

- v0.15.0 does not publish a numeric token-savings or competitor-superiority claim. Token-efficiency work is exposed through manifests, buckets, summaries, and tests; public efficiency claims still require reproducible benchmark artifacts.

## 0.14.1 - Generated Web Failure Diagnostics

### Fixed

- Problems focus now shows the changed-file excerpt that matches the verifier diagnostic path. A failure such as `[typescript syntax] src/App.tsx:1:1 invalid syntax` now jumps to the `src/App.tsx` diff instead of showing the first unrelated changed file.
- Generated web syntax diagnostics that include line/column suffixes such as `src/App.tsx:1:1` are normalized back to artifact paths before retry policy runs. This keeps generic repair contexts on the generated-web fast-fail path instead of spending another broad provider repair.
- The existing Vite/React chess rules test seed no longer imports `vitest`. The seeded test file uses self-executing assertions, so it does not require Phonton to repair `package.json` before local rules verification can pass.

## 0.14.0 - Non-Interactive Node Verification

### Fixed

- Generated Vite/React chess seeds no longer fail with `npm test timed out after 180s`. Node verification now rewrites stock `"test": "vitest"` scripts to `npm test -- --run` (and `"test": "jest"` to `npm test -- --watchAll=false`), so Vitest and Jest never enter watch mode during verification.
- Verification subprocesses always set `CI=1`, `NPM_CONFIG_YES=true`, `NPM_CONFIG_FUND=false`, `NPM_CONFIG_AUDIT=false`, and related non-interactive env vars before spawning `npm`. Stock scaffolds no longer hang waiting for TTY prompts.
- `test:ci` and `test:run` scripts in `package.json`, when present, are preferred over the default `test` script — projects can opt into a deterministic Phonton verification command without changing their interactive `test` workflow.

### Changed

- Node verification failure receipts now show the exact `npm` command Phonton attempted (e.g. `npm test -- --run failed: ...`, `npm run build failed: ...`), so users see what to repair without re-running.
- Node test-step timeouts now classify as `test harness timeout — likely interactive/watch mode` and include compact repair guidance (add `test:ci`/`test:run` or invoke the runner non-interactively). Non-test step timeouts (install, build) keep the plain timeout message; they have a different root cause.

### Added

- `phonton_verify::select_node_test_command` and `phonton_verify::npm_verification_env` are now public surface, so future verifiers and downstream tooling can reuse the same deterministic Node command/env shape.

### Notes

- Browser/runtime verification, missing-criteria-only repair, and richer HandoffPacket/OutcomeLedger proof export remain in flight and are not part of this release.
- Token/cost claims continue to require reproducible artifacts; no benchmark superiority claim is made by this release.

## 0.13.5 - Local Chess Rules Seed

### Fixed

- Existing Vite/React chess benchmark runs now seed `src/chessRules.ts` and `src/chessRules.test.ts` with a locally verified rules/test template before provider UI slices.
- Reruns inside a partially failed chess workspace now replace invalid partial rules/test artifacts locally instead of spending another provider call on TypeScript syntax repair.
- Existing-workspace chess token estimates now exclude the local rules seed slice, making the displayed budget match the lower-token execution path.
- Release binary builds no longer restore cached cargo binaries, avoiding broken macOS release jobs caused by stale cargo/rustup shims.

## 0.13.4 - Existing Vite Source-First Chess Slices

### Fixed

- Existing Vite/React chess benchmark runs now use a separate source-first contract instead of reusing the empty-workspace scaffold contract.
- The first existing-workspace slice now targets `src/chessRules.ts` and `src/chessRules.test.ts`, not `package.json` or `index.html`, avoiding cross-file stale hunks from weak diff models.
- Existing-workspace rules slices no longer request `chess.js`, so the first provider call has no reason to edit package dependencies.

## 0.13.3 - Complete Vite Chess Artifact Context

### Fixed

- Tightened the existing Vite/React chess path so the scaffold slice advertises every file it may touch, including `src/App.tsx`, `src/main.tsx`, `index.html`, and the chess rules/test files.
- This makes the worker include the current App file snapshot even on the first scaffold slice, preventing stale first-line hunks before repair is even considered.

## 0.13.2 - Existing Vite Chess Preflight

### Fixed

- Fixed existing-project chess prompts that say to use the current stack but do not literally mention Vite/React/TypeScript. Phonton now detects Vite/React workspace markers and keeps those goals on the compact Vite chess acceptance-slice contract.
- App UI slices now carry `Artifact: src/App.tsx`, which makes the worker include the actual current App file snapshot in the first provider prompt and avoids stale first-line hunks against placeholder fixtures.

## 0.13.1 - Generated App Token Guard

### Fixed

- Fixed the chess benchmark path where a global old install could still run v0.12.6 behavior; v0.13.1 keeps explicit Vite/React chess goals on compact acceptance slices even when rerunning inside a partial npm workspace.
- Classified generated-app acceptance slices as high-risk generated app work so worker context budgets and routing policies stay low-token.
- Stopped automatic provider repair after a first-attempt generated web-app syntax failure in TSX/JSX/HTML/JavaScript/TypeScript outputs. Phonton now fails with verifier evidence instead of spending another repair call on the same weak route.

## 0.13.0 - Workspace Ask And Diff QoL

### Added

- Added bounded workspace-aware Ask context for TUI Ask and `phonton ask`, including explicit `@file` mentions, current goal diagnostics, workspace facts, file maps, and lexical file excerpts under a 1200-token target.
- Added `phonton ask --no-workspace` to preserve the old stateless Ask path and `phonton ask --json` for scriptable answer/context metadata.
- Added `phonton diff [latest|<task-id>]` to export only verified `SubtaskReviewReady` unified diffs from the local task store.
- Added `phonton diff --stat`, `phonton diff --name-only`, and `phonton diff --json` for compact review and tooling surfaces.
- Added `/diff` and `/code` slash commands plus the `d` TUI shortcut to jump directly to the Code focus.

### Changed

- Ask mode now shows a compact `ctx:` summary line so users can see how much workspace context was selected.
- The compact `phonton` TUI header no longer shimmers rapidly after a goal exists; active task spinners and savings flashes still animate.
- Code focus now shows file count plus added/removed line totals at the top of the diff view.
- Help text and README release docs now include the first-class diff surfaces.

## 0.12.6 - Provider Contract Hardening

### Fixed

- Treat DeepSeek as a first-class OpenAI-compatible route in provider diagnostics instead of anonymous `openai-compatible`.
- Disable DeepSeek V4/reasoner thinking mode for diff-only worker calls so provider responses return final `message.content` instead of reasoning-only output.
- Version the provider-health cache key so stale v0.12.5 canary passes are ignored under the corrected request contract.
- Fail reasoning-only OpenAI-compatible replies with a precise provider contract error.
- Bound Settings and provider-doctor diff canaries with a 20s timeout so broken routes do not hang setup.

## 0.12.5 - Provider Reliability

### Added

- Added a Models.dev-backed provider catalog and `phonton providers list|sync|doctor|import-opencode` commands.
- Added first-class OpenCode and OpenCode Go routes through `OPENCODE_API_KEY` or explicit `provider.api_key_source = "opencode"`.
- Added a provider/model diff canary so readiness proves the exact unified-diff contract Phonton workers need.

### Changed

- Startup model detection, Settings connection tests, `phonton doctor --provider`, and goal dispatch now validate parseable-diff output instead of a generic JSON reply.
- Goal dispatch blocks unverified hosted provider/model routes before spending implementation tokens unless `provider.allow_unverified_model = true`.

### Fixed

- Empty OpenAI-compatible responses are now rejected as provider contract failures instead of being retried as syntax repair failures.
- Worker provider contract failures stop before broad repair, avoiding the multi-thousand-token empty-output failure shown by weak provider routes.

## 0.12.4 - Autonomy Loop Hotfix

### Changed

- Worker retry policy now stops after two matching verifier/parser diagnostic signatures instead of spending a third blind attempt.
- Orchestrator retry diagnostics are now included in the next worker's first prompt and semantic-context query, so repair attempts start from the actual verifier evidence.
- Stale hunk diagnostics now add explicit repair guidance to avoid repeating the same patch and prefer full-file replacement for small generated artifacts.
- Flight Log now emits a `repair` event before redispatch, making bounded repair/replan behavior visible.
- Long verifier diagnostics are compacted in Flight Log rendering while the raw event still carries the full error list.

### Fixed

- Small generated artifact full-file replacement hunks are applied directly when safe, avoiding libgit2 patch failures caused by stale removed-line context.
- Failure receipts no longer say a worker is escalating after exhausting its own retry budget when the worker is actually stopping before another blind retry.

## 0.12.3 - Chess Benchmark Stale Hunk Fix

- Fixed the next generated chess benchmark failure mode where a rules slice
  could update `src/chessRules.test.ts` without receiving the current test file
  snapshot, producing stale removed-line hunks during TypeScript verification.
- Vite/React chess rules and rules-test slices now carry paired current
  artifacts: `src/chessRules.ts` with `src/chessRules.test.ts`, and vice versa.
- Repair attempts now add the exact current file named in verifier diagnostics
  to context, so a failed hunk repair sees the real file it must patch instead
  of retrying with only compact error text.

## 0.12.2 - Chess Benchmark Test Slice Fix

- Fixed generated Vite/React chess scaffolds so the first acceptance slice asks
  for a starter `src/chessRules.ts` rules boundary and
  `src/chessRules.test.ts` smoke test instead of leaving Vitest with zero test
  files.
- Node verification now waits to run Vitest/Jest-style file-discovery scripts
  until a `.test.*` or `.spec.*` file exists, while still running custom npm
  test scripts and running Vitest/Jest once generated tests are present.
- Added regressions for the exact early-slice failure where `npm test` failed
  with no discovered test files before the planned rules-test slice could run.

## 0.12.1 - Chess Benchmark Runtime Fix

- Fixed the playable chess benchmark route for prompts that explicitly require
  Vite, TypeScript, and React in an empty workspace. Phonton now scaffolds a
  Vite/React npm app contract instead of simplifying the goal to static
  `index.html`.
- Added benchmark-specific acceptance slices for a chess.js-backed rules
  boundary, rules tests, React board UI, interactions, status/history/reset,
  and concrete `npm install`, `npm test`, `npm run build`, and `npm run dev`
  commands.
- Worker prompts now include the current generated artifact snapshot for later
  acceptance slices, so repair and follow-up slices patch the file that
  actually exists instead of guessing stale hunk coordinates.
- Benchmark acceptance-slice subtasks now use compact goal labels instead of
  repeating the full pasted benchmark prompt in every worker call.
- Successful worker slices now store compact change summaries in shared
  context instead of the full prior diff body, reducing prompt carryover
  tokens on broad generated-app runs.
- Node/Vite generated projects now run npm verification from a temporary
  post-diff workspace when `package.json` or web source files are touched, so
  failing `npm test` or `npm run build` blocks review-ready status.
- Fixed no-git direct patch application to reconstruct existing files from
  old-side hunk coordinates and context/removal lines instead of splicing only
  additions at new-side offsets.

## 0.12.0 - Verified Success Per Token

### Added

- Added structured intent classification with task class, ambiguity, blast
  radius, runtime risk, token risk, and recommended action before worker
  dispatch.
- Added acceptance slices and token policy fields to `GoalContract`, including
  generated web/game safeguards for broad goals such as HTML chess.
- Added `phonton benchmark export --latest --format json` for exporting
  real OutcomeLedger evidence with provider-reported tokens, context buckets,
  verification, quality gates, and final benchmark status.
- Added top-level `phonton why-tokens --by-source` for source-attributed token
  buckets outside the TUI.
- Added `phonton proof export --latest --format json` for exporting the latest
  proof bundle from the OutcomeLedger.
- Added `phonton context eval <fixture>` and
  `phonton context diff --indexed --non-indexed <fixture>` for deterministic
  context-selection fixture checks.
- Added optional Playwright-based browser runtime verification hooks for
  generated web artifacts.

### Changed

- Generated app/game goals now require preflight-style contracts, runtime proof
  hooks, capped first attempts, and surgical repair policy instead of broad
  automatic repair.
- Generated app/game contracts now decompose into sequential acceptance-slice
  subtasks, and worker prompts enforce task-class context budgets, smaller
  semantic top-k retrieval, capped repo maps, capped MCP result context, and
  lower provider output ceilings.
- Plan preview now surfaces intent, risk, acceptance slices, and token policy.
- `/retry` repair prompts include missing or unverified acceptance criteria so
  repairs are narrower and easier to review.

## 0.11.1 - Benchmark Hotfix

### Fixed

- Fixed chess quality gates so symbol/abbreviation piece maps such as
  `K/Q/R/B/N/P` and Unicode chess glyphs count as valid piece evidence instead
  of falsely failing `named chess pieces`.
- Prevented automatic broad chess quality repair after an already-expensive
  attempt crosses 8k provider tokens; Phonton now fails honestly and tells the
  user to run `/retry` for an explicit compact repair.
- Honored explicit empty-workspace HTML chess goals by targeting a static
  `index.html` artifact instead of silently defaulting to Python terminal
  chess.

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
