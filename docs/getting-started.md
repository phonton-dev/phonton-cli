# Getting started

Install from npm or build from source. Configure a provider key in
`~/.phonton/config.toml`, then run `phonton doctor`.

See `phonton doctor --provider` for a live completion probe.

## Five-minute path

```bash
cd your-repo
phonton demo trust-loop    # local proof without spending tokens
phonton                     # TUI: submit a goal
phonton goal "fix the failing test" --yes   # headless
phonton review latest       # receipt + diffs
```

## Provider-only vs product-mode benchmarks

- **Provider-only:** set `PHONTON_DISABLE_LOCAL_SEEDS=1` — all slices use your LLM.
- **Syntax-preflight harness (Windows):** set `PHONTON_BENCH_PYTHON` to a Python 3
  executable if `python` / `py -3` are not on PATH (`phonton doctor` reports status).
- **Product-mode:** local templates may satisfy known benchmark slices with
  `local-template` (zero provider tokens). Do not confuse with model efficiency.

The TUI shows `execution: provider` or `local-template` on completed goals.

## Pause / resume

When a goal pauses on budget, resume with:

```bash
phonton goal --resume <task-id>
```

## Incremental index

```bash
phonton index watch
```

## Claims

Public comparisons require fixture artifacts with `token_claim_eligible: true`.
See [Benchmarks](https://phonton.dev/benchmarks.html).
