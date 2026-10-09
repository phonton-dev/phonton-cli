# Getting started

## Install

```bash
npm install -g phonton-cli
```

## Pick a model

Either set a provider key in your environment (Anthropic, OpenAI, DeepSeek,
OpenRouter, Gemini, Groq, Together, xAI):

```bash
export DEEPSEEK_API_KEY=...      # or ANTHROPIC_API_KEY, OPENAI_API_KEY, ...
```

or run a local model with no key (managed runtime on Windows x64; elsewhere
install [Ollama](https://ollama.com/download) first):

```bash
phonton models catalog                    # models that fit this machine
phonton models setup qwen2.5-coder:3b     # runtime, download, calibrate, select
```

Then check the setup from your project folder:

```bash
cd your-repo
phonton doctor             # key, store, trust, the project's toolchains
phonton doctor --provider  # adds a live completion probe
```

## First goal

```bash
phonton                    # TUI: type a goal, press Enter
```

The first launch in a folder asks you to trust it. The first goal asks whether
Phonton may run the project's own checks (build, tests) on this machine;
without that, diffs are still written but cannot be marked verified.

Without the TUI, `phonton goal "fix the failing test"` asks the same two
questions in a terminal. In scripts and CI, answer them with flags:

```bash
phonton goal "fix the failing test" --yes --allow-host-checks
phonton review latest            # receipt and diffs
phonton review approve latest    # keep the change
phonton review reject latest     # undo the task's edits
```

Local-model runs land nothing until you apply them:

```bash
phonton goal --local apply RUN_ID --yes
phonton goal --local rollback RUN_ID --yes
```

## Pause and resume

When a goal pauses on budget, resume it from the same repository:

```bash
phonton goal --resume <task-id> --allow-host-checks
```

## Keep the index fresh

```bash
phonton index watch
```

## Claims

Public comparisons need reproducible artifacts; see
[Benchmarks](BENCHMARKS.md).
