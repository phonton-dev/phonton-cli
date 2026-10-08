# Phonton CLI Benchmarks

Phonton benchmark claims must be reproducible from this repository. No broad
"saves X percent" claims from a single lucky run.

## Smoke benchmark (end to end)

`benchmarks/smoke/run.mjs` runs real goals and scores them the way a reviewer
would:

1. Copy a fixture from `fixtures/` into a fresh git repository.
2. Run `phonton goal --yes --allow-host-checks --json "<goal>"` (cloud arm) or
   `phonton goal --local ...` (local arm) on it.
3. Write hidden tests the run never saw, then run `node --test`. Acceptance is
   that exit code, not Phonton's own verdict.
4. Record status, wall time, provider-reported tokens and cost, the diff, and
   raw stdout/stderr under `benchmarks/results/` (git-ignored).

Fixtures:

- `todo-api`: four small files; add methods, sort, change a limit.
- `ledger`: one 339-line module; edits land mid-file.
- `shop`: one 955-line, 28 KB module, larger than the worker's whole-file
  limit, so workers see excerpts.

```bash
cargo build --release -p phonton-cli
node benchmarks/smoke/run.mjs --arm cloud --runs 3
node benchmarks/smoke/run.mjs --arm local --runs 3 --tasks todo-count
node benchmarks/smoke/run.mjs --arm cloud --bin path/to/other/phonton.exe --out benchmarks/results/other
```

Set `PHONTON_HOME` to an isolated profile so runs do not touch your own
memory or settings. Tasks and their hidden tests live in
`benchmarks/smoke/tasks.mjs`; every hidden test fails on the untouched fixture
and passes with a reference fix.

What it does not prove: anything about tasks unlike these, other machines, or
other tools. n = 3 per task is enough to catch regressions and big wins, not
small differences. Token use for the same goal can vary 2-3x between runs;
report medians or totals over all runs, never the best run.

## Plan benchmark (planner only)

`scripts/benchmark-plan.ps1` measures the planning layer: subtask count,
estimated tokens against a naive baseline, and runtime. It makes no model edits
and proves nothing about end-to-end success.

```powershell
.\scripts\benchmark-plan.ps1 -OutDir tmp\benchmarks
```

## Public claim rules

Allowed with artifacts from this harness:

- Acceptance, wall time, tokens and cost on the named fixtures, model, and
  version, with n stated.
- Before/after comparisons of two Phonton builds run the same way.

Not allowed without new evidence:

- "Saves N% tokens" in general, or against another tool.
- "Cheaper/better than Claude Code, Codex, Cursor" or "best ADE".
- "Fully autonomous."

Results from Phonton 0.16 to 0.21 are not used for claims: those builds
contained hardcoded answers for some benchmark fixtures (removed in 0.22.0).
