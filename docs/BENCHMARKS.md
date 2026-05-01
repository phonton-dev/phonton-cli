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

## What It Does Not Prove Yet

The current harness does not prove end-to-end superiority over Codex, Claude Code, Cursor, or any other tool.

It does not yet measure:

- actual provider billable input/output tokens;
- cached-token behavior by provider;
- diff correctness after human review;
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

## Interpreting Results

The report includes an estimated reduction:

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
