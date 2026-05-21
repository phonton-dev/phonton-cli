# Phonton CLI Benchmarks

Phonton benchmark claims must be reproducible. This repo should not publish broad "saves X percent" claims from a single lucky run.

## What The Current Harness Measures

`scripts/benchmark-plan.ps1` measures the planning layer:

- goal text;
- generated subtask count;
- estimated Phonton task tokens;
- planner naive-baseline tokens;
- estimated reduction versus the naive baseline;
- wall-clock runtime;
- pass/fail status.

This is useful for checking whether Phonton is producing compact plans and whether the planner's context strategy is moving in the right direction.

`scripts/score-benchmark-runs.ps1` scores end-to-end run folders when they
exist. The primary score is:

```text
verified_success_per_10k_tokens = verified_successes / (provider_reported_tokens / 10000)
```

This is the benchmark direction for v0.11.0 and later: not "fewest tokens"
alone, and not "looks done", but the least provider-reported tokens per
verified, reviewable result.

## What It Does Not Prove Yet

The current harness does not prove end-to-end superiority over Codex, Claude Code, Cursor, or any other tool.

It does not yet measure:

- actual provider billable input/output tokens unless each run writes
  `token-usage.json`;
- cached-token behavior by provider unless the provider exposes it;
- diff correctness after human review unless each run writes
  `quality-review.json`;
- time-to-merged-change;
- quality compared with a competitor on the same task;
- full autonomous edit success rate.

Treat current benchmark numbers as internal release evidence, not public marketing claims.

## Run The Benchmark

From the repo root:

```powershell
.\scripts\benchmark-plan.ps1
```

Use a custom set of goals:

```powershell
.\scripts\benchmark-plan.ps1 -Goals @(
  "add input validation to config loading",
  "improve provider auth error messages",
  "write tests for rollback failure handling"
)
```

Write reports somewhere else:

```powershell
.\scripts\benchmark-plan.ps1 -OutDir tmp\benchmarks
```

Score completed end-to-end run folders:

```powershell
.\scripts\score-benchmark-runs.ps1 `
  -RunsDir benchmarks\runs `
  -OutJson benchmarks\reports\score.json `
  -OutMarkdown benchmarks\reports\score.md
```

Export the latest real Phonton run from the local OutcomeLedger:

```powershell
phonton benchmark export --latest --format json
```

For reproducible headless Phonton runs, use the same goal spine without TUI
paste, clipboard, or PTY automation:

```powershell
phonton goal --prompt-file prompt.md --json --yes --timeout-seconds 900
```

As of v0.16.2, prompts that ask to run tests first capture bounded baseline
test evidence before editing. Prompt-mentioned paths and API signatures are
also copied into the GoalContract so small refactor benchmarks can keep files
such as `src/receipt.js` and APIs such as `buildReceipt(run)` in the repair
surface. On Windows, Node package-manager baselines use the correct command
shims such as `npm.cmd`. If a benchmark work folder is nested under a larger
git repo but is not itself a git root, Phonton applies verified hunks directly
inside the work folder and reports that rollback checkpoints were unavailable.

The export now preserves estimated, local-template, failed, and provider-backed
runs, but labels comparability explicitly. Consumers must check
`token_usage_source`, `execution_mode`, `provider_call_count`,
`token_claim_eligible`, and `benchmark_warnings` before scoring. Only verified
provider runs with `token_claim_eligible: true` belong in
`verified_success_per_10k_tokens`; product-mode/local-template wins belong in a
separate reliability table. Mixed local-template/provider runs are also
ineligible for provider-token efficiency claims, even when they have provider
usage metadata.

v0.15.0 adds richer proof inputs for future end-to-end benchmark runs. OutcomeLedger records now include deterministic summary bundles, context bucket evidence, selected index sources, permission records, command-run evidence, verification findings, and HandoffPacket known gaps. These records make a run easier to audit, but they still do not prove token savings or quality by themselves.

Inspect source-attributed prompt/context buckets for the latest run:

```powershell
phonton why-tokens --by-source
```

Export the proof bundle attached to the latest run:

```powershell
phonton proof export --latest --format json
```

Evaluate context-selection fixtures before a benchmark batch:

```powershell
phonton context eval fixtures/context.json --format json
phonton context diff --indexed --non-indexed fixtures/context.json --format json
```

v0.12 also has pre-call savings controls. Generated app/game goals are split
into acceptance-slice subtasks before worker dispatch; simple/docs/test work
uses smaller task-class context targets; generated-app repairs use a sub-1k
context target; semantic top-k, repo-map entries, MCP result context, and
provider output ceilings are lower by default. These controls should show up in
`context_buckets` and provider-reported token usage, not just in estimates.

v0.12.1 tightens the chess generated-app benchmark path specifically. Empty
workspace prompts that explicitly request Vite, TypeScript, and React now get a
Vite/React npm GoalContract, chess.js-backed rules/test slices, current
artifact snapshots between slices, and npm install/test/build verification from
a temporary post-diff workspace.

v0.12.2 fixes an early-slice verification regression from that path. Vite/React
chess scaffold slices now request a starter rules module plus smoke test, and
the npm verifier waits to run Vitest/Jest discovery scripts until generated
test files exist. This prevents a scaffold-only slice from burning repair
attempts on "no test files found" before the planned rules-test slice runs.

v0.12.3 fixes stale hunk context for the same benchmark path. Rules and
rules-test slices now include paired current artifacts, and repair attempts add
the exact current file named in verifier diagnostics. This keeps the context
small while preventing repeated TypeScript reconstruction failures on generated
test files.

Expected per-run files:

```text
benchmarks/runs/<suite>/<tool>/<task>/<run>/
  metadata.json        # tool, task_id, status, versions
  token-usage.json     # total_tokens or input/output token buckets
  quality-review.json  # verified/success/passed boolean
```

## Interpreting Results

The plan report includes an estimated reduction:

```text
1 - (estimated_total_tokens / naive_baseline_tokens)
```

This number is only as good as the planner's baseline estimate. It is still useful because the same formula can be tracked across commits and tasks.

Good release evidence should include:

- at least 10 real repo tasks;
- raw JSON report;
- Markdown summary;
- exact commit hash;
- exact Phonton version;
- provider/model where live model calls are used;
- verification command results;
- failures, not just wins.
- the full exported fields for `final_status`, `execution_mode`,
  `token_usage_source`, `token_claim_eligible`, and `benchmark_warnings`.

The end-to-end score report ranks tools by verified success per 10k tokens.
A tool that uses fewer tokens but fails verification should score lower than a
tool that spends more tokens and passes.

## Public Claim Rules

Allowed before broader data:

- "Designed for context efficiency."
- "Includes benchmark tooling for plan-token estimates."
- "Measures compact plans against a naive baseline."

Avoid until there is repeatable evidence:

- "Saves 5x tokens."
- "Cheaper than Claude Code/Codex/Cursor."
- "Best ADE."
- "Fully autonomous."

## Next Benchmark Milestones

1. Add end-to-end task benchmark support for goal -> diff -> verification -> review.
2. Capture actual provider usage when providers expose token counts.
3. Compare Phonton against a documented baseline workflow on the same repo and task.
4. Store benchmark fixtures under `benchmarks/fixtures/`.
5. Publish raw reports with every release candidate.
