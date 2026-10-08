# Smoke benchmark

A small, repeatable check that Phonton turns plain goals into accepted
changes. It is not a comparison with any other tool.

## Method

- Fixtures (Node, no dependencies, `node --test`): `fixtures/todo-api`, four
  small files; `fixtures/ledger`, one 339-line module so edits land mid-file;
  `fixtures/shop`, one 28 KB module, larger than the worker's whole-file limit.
- Tasks: `tasks.mjs`. Each goal is a sentence a user might type.
- Every run starts from a fresh git copy of the fixture in the temp
  directory, so runs are independent.
- After Phonton finishes, hidden acceptance tests from `tasks.mjs` are
  written into the copy and the whole suite runs. A run is **accepted** only
  if every original and hidden test passes. Phonton never sees the hidden
  tests.
- Cloud arm: `phonton goal --yes --allow-host-checks --json "<goal>"`. Tokens
  and cost come from the run's own receipt (provider-reported usage; cost
  only when pricing for every model used is known).
- Local arm: `phonton goal --local "<goal>" --yes --allow-host-checks`, then
  `phonton goal --local apply RUN_ID --yes` when the candidate is review-ready.
  Local token counts are the runtime's reported counts; API cost is zero.

## Run

```bash
cargo build --release -p phonton-cli
node benchmarks/smoke/run.mjs --arm cloud --runs 3
node benchmarks/smoke/run.mjs --arm local --runs 3
node benchmarks/smoke/run.mjs --arm cloud --tasks ledger-parens,ledger-csv-quote
node benchmarks/smoke/run.mjs --arm cloud --bin other/phonton.exe --out benchmarks/results/other
```

`--bin` runs another build the same way, for before/after comparisons.

The provider and keys come from your environment or config; set
`PHONTON_HOME` to use an isolated profile. Results, raw stdout/stderr, diffs
and acceptance output are written to `benchmarks/results/smoke-*` (not
committed). `summary.md` reports accepted runs, and median seconds, tokens
and USD per task.

## Limits

- Twelve small tasks on three fixtures; medians of three runs. Token use varies
  run to run (up to ~3x on the same goal in our runs), so single runs say
  little.
- Wall time includes model latency and the fixture's own checks on this
  machine.
- Hidden tests check only what each goal states.
