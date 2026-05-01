# Release Checklist

Use this before tagging a Phonton CLI release.

## Required

- [ ] README reflects the actual CLI command surface.
- [ ] CHANGELOG has a release entry.
- [ ] `cargo fmt --all -- --check` passes.
- [ ] `cargo clippy --locked --workspace --all-targets -- -D warnings` passes.
- [ ] `cargo test --locked --workspace` passes.
- [ ] `cargo build --locked --release -p phonton-cli` passes.
- [ ] `phonton doctor` runs from the release binary.
- [ ] `phonton doctor --provider` is tested with at least one hosted provider.
- [ ] `scripts/benchmark-plan.ps1 -ReleaseBinary` passes.
- [ ] Benchmark report is attached to the release notes if making efficiency claims.
- [ ] No benchmark output, screenshots, logs, or docs contain secrets.

## Recommended Manual Smoke Test

```powershell
.\target\release\phonton.exe version
.\target\release\phonton.exe doctor
.\target\release\phonton.exe plan --json "add input validation to config loading"
.\scripts\benchmark-plan.ps1 -ReleaseBinary
```

## Release Notes Policy

Release notes should say what changed, what was verified, and what is still limited. Avoid unsupported competitive claims.
