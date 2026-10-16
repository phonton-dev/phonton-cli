# Aider Polyglot slice

Runs exercises from the [Aider Polyglot benchmark](https://github.com/Aider-AI/polyglot-benchmark)
through three arms on the same local model and the same prompt, then scores
every run against the exercise's pristine tests:

| Arm | What runs | Its claim of success |
| --- | --- | --- |
| `phonton` | `phonton goal --local "<prompt>"`, then `goal --local apply` | a verified, review-ready candidate |
| `bare` | the model alone: one chat call per try, whole files back | any answer that contains the files |
| `opencode` | `opencode run --pure` with an isolated config, without the user's `~/.claude` instructions or skills | finishing without an error |

The headline numbers are the **pass rate** and the **false-pass rate**: of the
runs where the arm said it succeeded, how many fail the pristine tests.

## Protocol

- **Exercises.** `--per-lang N` takes N exercises per language in a seeded
  hash order (`--seed`, default `phonton-polyglot-1`), so a slice is
  reproducible and not alphabetical. `--exercises a,b` picks by name. The
  full benchmark is 225 exercises in six languages; this runner implements
  Python, JavaScript and Rust (113 exercises). Other languages need a test
  command in `testCommand()`.
- **Workspace.** Each run gets a fresh temporary git copy of the exercise
  without `.meta/` (it holds the reference solution). JavaScript copies share
  one `node_modules` junction with jest and babel from the exercise's
  `package.json`, plus that install's `package-lock.json`. Phonton leaves the
  junction out of its candidate copy and rebuilds `node_modules` from the
  lockfile with offline `npm ci` (no lifecycle scripts) before each check;
  it runs `node node_modules/jest/bin/jest.js --ci --json`.
- **Prompt.** `.docs/instructions.md` plus `instructions.append.md`, then
  Aider's benchmark addendum naming the solution files. Every arm gets the
  same text.
- **Tests.** `--tests visible` (default, as in Aider's benchmark) leaves the
  test files in the workspace with every skipped JavaScript test enabled.
  `--tests hidden` removes them, so no arm can run them.
- **Bare model tries.** With visible tests the bare arm gets Aider's two tries:
  the test output goes back once with Aider's "fix the code" message
  (`--bare-tries`). With hidden tests it gets one.
- **Scoring.** After the arm finishes, the original test files are written
  back (an arm that edits or deletes tests gains nothing; `tests_touched`
  flags it) and the language's tests run: `python -m unittest discover
  --pattern "*_test.py"`, jest with all tests enabled, `cargo test -- --include-ignored`.
  Exit code 0 is a pass.
- **Tokens.** Phonton: input and output tokens of every candidate in the
  local run evidence. Bare: the runtime's reported usage for every try.
  OpenCode: input, cache-read, output and reasoning tokens from its JSON
  step events.
- **Wall time** is the arm's own run, without workspace setup or scoring.
  The runner unloads the model from Ollama before every arm run, so each
  starts cold and pays its own model load.

## Run

Serve the model with Ollama (or any OpenAI-compatible server), with a context
length every arm can use:

```bash
OLLAMA_CONTEXT_LENGTH=16384 ollama serve
ollama pull qwen3.5:9b
```

Phonton needs that model calibrated and selected in the profile it runs with:

```bash
export PHONTON_HOME=/tmp/phonton-bench
phonton models endpoint http://127.0.0.1:11434
phonton models calibrate qwen3.5:9b && phonton models select qwen3.5:9b
```

Clone the benchmark at a pinned commit and run:

```bash
git clone https://github.com/Aider-AI/polyglot-benchmark ../polyglot-benchmark
git -C ../polyglot-benchmark checkout 7e0611e77b54e2dea774cdc0aa00cf9f7ed6144f
cargo build --release -p phonton-cli
node benchmarks/polyglot/run.mjs --polyglot ../polyglot-benchmark \
  --model qwen3.5:9b --per-lang 17 --runtime unverified
```

Before spending GPU time, check the scorer and the language toolchains with
the reference model: every run must pass, and with `--stub` only exercises
whose stub already works (such as the `ledger` refactoring) may pass. The
exception is the 7 Rust exercises with a `.meta/Cargo-example.toml`: their
reference solution needs a crate the exercise does not declare (`pig-latin`
uses `regex`), and arms may only edit the solution files.

```bash
node benchmarks/polyglot/reference-model.mjs ../polyglot-benchmark 11999 &
node benchmarks/polyglot/run.mjs --polyglot ../polyglot-benchmark --model reference \
  --endpoint http://127.0.0.1:11999/v1 --arms bare --per-lang 3
```

### On a free Kaggle GPU

`kaggle_job.py` runs the whole slice on a Kaggle notebook session (GPU T4 x2,
internet on, 12 h limit): it builds Phonton, installs Ollama 0.34.2, Node and
OpenCode at pinned versions, calibrates the model at 16384 tokens and runs one
runner process per language. Upload the source as a private dataset with
`phonton-src.tar.gz` (`git archive --format=tar.gz HEAD`) and `commit.txt`,
then push the job as a private script kernel with that dataset as a source,
`"enable_gpu": "true"`, `"enable_internet": "true"` and
`"machine_shape": "NvidiaTeslaT4"`. Set `PER_LANG` at the top of the script.
`kaggle kernels output` downloads `out/`, `setup.log` and `env.txt`; attaching
a finished run as a kernel source reuses its `prebuilt/phonton`.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--polyglot DIR` | required | polyglot-benchmark checkout |
| `--model TAG` | required | model for every arm |
| `--endpoint URL` | `http://127.0.0.1:11434/v1` | OpenAI-compatible base for bare and OpenCode |
| `--arms LIST` | `phonton,bare,opencode` | arms to run |
| `--langs LIST` | `python,javascript,rust` | languages |
| `--per-lang N` / `--exercises LIST` | `2` | slice size or explicit exercises |
| `--tests visible\|hidden` | `visible` | whether arms can see the tests |
| `--bare-tries N` | 2 visible, 1 hidden | bare model tries |
| `--runtime unverified` | off | Phonton may use an Ollama it did not start |
| `--bin PATH` | `target/release/phonton` | Phonton build |
| `--opencode PATH` | from `npm root -g` | OpenCode's `bin/opencode` script |
| `--timeout-seconds N` | `900` | per arm run |
| `--out DIR` / `--cache DIR` | `benchmarks/results/...` | artifacts / reusable test deps |

Each run writes `meta.json` (versions, commit, model, hardware, arguments),
`results.jsonl`, `summary.md`, and per exercise and arm: `prompt.md`,
`stdout.txt`, `stderr.txt`, `diff.patch`, `tests.txt`, plus Phonton's
`evidence.json` or OpenCode's config. `benchmarks/results/` is not committed;
copy a run's folder next to its write-up.

## Limits

- Comparisons hold only for one model, one runtime, one context length and one
  machine. The runtime's context default caps the bare and OpenCode arms;
  Phonton sets its calibrated context per request, which its memory estimate
  can hold well below the runtime's (4096 tokens for `qwen3.5:4b` on a 6 GB
  GPU). Record both in the write-up.
- Phonton's JavaScript checks need jest's packages in the local npm cache:
  its preparation runs `npm ci --offline`. The runner's first `npm install`
  fills the cache; a cache miss ends the run unverified, not failed.
- OpenCode reports no verdict, so finishing is its claim; that favours neither
  side on pass rate but makes its false-pass rate an upper bound.
- Python is scored with `unittest`, not pytest as in Aider's harness; the
  Exercism Python tests are `unittest` classes.
- Single runs vary. Report n and repeat before quoting a difference of a few
  exercises.
