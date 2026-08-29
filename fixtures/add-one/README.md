# Golden path fixture: `add-one`

One failing unit test. Phonton’s job is to make `add_one` return `n + 1` so `cargo test` passes.

This is the smallest verifiable loop: goal → cheap model → verify → receipt. Do not use it to claim savings versus another product.

## Run

From a published or local `phonton` binary (0.21.1+):

```bash
cd fixtures/add-one
git init   # if this folder is not already a git repo
cargo test # expected: FAIL on adds_one

phonton doctor --provider
phonton goal "Make add_one return n + 1 so the unit tests in src/lib.rs pass." --yes
phonton review latest
phonton review latest --json
```

Desktop: open this folder, run the same goal, and read the strip plus Receipt tab. Cost and frontier come from `cost_receipt`.

## Labeled receipt

[`example-receipt.json`](./example-receipt.json) is a **labeled live receipt** from CLI 0.21.1 (DeepSeek, 2026-08-29). Frontier dollars use the published Opus-class list rate (`FRONTIER_REFERENCE_PRICING`, $15 / $75 per MTok). It is not a competitor comparison.

The source in this folder is kept failing on purpose so you can rerun the loop. After your run, `phonton review latest --json` is the receipt for *your* key and models.

