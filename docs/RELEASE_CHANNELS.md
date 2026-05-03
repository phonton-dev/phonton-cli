# Release Channels

Phonton uses Git branches and GitHub Releases for install channels.

This is the Rust/GitHub equivalent of package-manager channels:

| Channel | Source | Install | Intended use |
|---|---|---|---|
| Stable | `v*` tags and GitHub Releases | `cargo install --git https://github.com/phonton-dev/phonton-cli --tag v0.2.1 phonton-cli --locked --force` | Best available public alpha |
| Dev | `dev` branch | `cargo install --git https://github.com/phonton-dev/phonton-cli --branch dev phonton-cli --locked --force` | Next-release integration |
| Nightly | `nightly` branch and moving `nightly` prerelease | `cargo install --git https://github.com/phonton-dev/phonton-cli --branch nightly phonton-cli --locked --force` | Daily snapshots; expect regressions |
| Main | `main` branch | `cargo install --git https://github.com/phonton-dev/phonton-cli --branch main phonton-cli --locked --force` | Current stable branch tip |

## Branch Policy

- `main` is the public release branch. It should stay green and close to the latest stable/pre-release tag.
- `dev` is the integration branch for the next release.
- `nightly` is generated from `dev` by the nightly workflow.
- Hotfixes should land on `main` first, then be merged or cherry-picked into `dev`.

## Release Automation

- CI runs on `main`, `dev`, `nightly`, and pull requests.
- `release-binaries.yml` builds GitHub Release assets for `v*` tags.
- `nightly.yml` syncs `dev` to `nightly`, builds assets, and publishes a moving `nightly` prerelease.

## Promotion Rhythm

The recommended alpha cadence is:

1. Merge normal work into `dev`.
2. Let nightly snapshots expose issues early.
3. Promote `dev` to `main` when CI and manual smoke tests pass.
4. Tag `main` as `vX.Y.Z` and publish a GitHub pre-release or release.

Do not promote nightly builds directly to stable without rerunning release checks.
