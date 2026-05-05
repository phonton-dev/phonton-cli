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
- [ ] `phonton extensions list --json`, `phonton extensions doctor --json`, and `phonton mcp list --json` run from the release binary.
- [ ] `npm run test:npm-wrapper` reports the same version as `package.json`.
- [ ] No benchmark output, screenshots, logs, or docs contain secrets.
- [ ] GitHub release is created from a `vX.Y.Z` tag on `main`.
- [ ] npm publish is run by `.github/workflows/publish-npm.yml` using Trusted Publishing.

## Conditional

- [ ] `phonton doctor --provider` proves model discovery and a tiny completion call when the release notes claim provider/runtime validation.
- [ ] `scripts/benchmark-plan.ps1 -ReleaseBinary` passes when the release includes benchmark or efficiency claims.
- [ ] Benchmark report is attached to the release notes when making efficiency claims.

## Recommended Manual Smoke Test

```powershell
.\target\release\phonton.exe version
.\target\release\phonton.exe doctor
.\target\release\phonton.exe plan --json "add input validation to config loading"
.\target\release\phonton.exe extensions doctor --json
.\target\release\phonton.exe mcp list --json
npm run test:npm-wrapper
```

## Release Notes Policy

Release notes should say what changed, what was verified, and what is still limited. Avoid unsupported competitive claims.
