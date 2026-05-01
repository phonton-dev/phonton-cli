# Contributing

Phonton CLI is early. The most useful contributions are narrow, verified, and easy to review.

## Development Setup

```bash
git clone https://github.com/phonton-dev/phonton-cli.git
cd phonton-cli
cargo build -p phonton-cli
```

Run the CLI from source:

```bash
cargo run -p phonton-cli -- doctor
cargo run -p phonton-cli -- plan "add input validation to config loading"
```

## Required Checks

Before opening a PR:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo build --locked --release -p phonton-cli
```

On Windows you can run the bundled release gate:

```powershell
.\scripts\release-check.ps1
```

## Contribution Principles

- Keep changes focused. Avoid unrelated refactors.
- Prefer explicit errors over panics in library crates.
- Do not print secrets, API keys, provider responses containing secrets, or full user config.
- Keep verification visible. If a generated diff can reach review, the verification status must be clear.
- Benchmark claims need raw reports. Do not add marketing numbers without reproducible evidence.

## PR Checklist

- Explain the user-facing change.
- Include tests for behavior changes.
- Run the required checks.
- Call out any skipped checks and why.
- Update README/docs when the command surface changes.

## Reporting Issues

Good issue reports include:

- OS and shell.
- `phonton version`.
- `phonton doctor` output with secrets removed.
- The exact command run.
- Expected behavior.
- Actual behavior.
- Minimal repo/task that reproduces the issue, if possible.
