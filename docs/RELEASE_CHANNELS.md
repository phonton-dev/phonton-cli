# Release Channels

| Channel | Install | Use |
|---|---|---|
| Stable | `npm install -g phonton-cli`, or a binary from [GitHub Releases](https://github.com/phonton-dev/phonton-cli/releases/latest) | Everyday use |
| Main | `cargo install --git https://github.com/phonton-dev/phonton-cli --branch main phonton-cli --locked --force` | The commit after the latest release; may be ahead of npm |

`main` is the public release branch and stays green. Each `v*` tag on `main`
builds the release binaries (`release-binaries.yml`); the npm package is
published from the same tag (`publish-npm.yml`).

There is no nightly or dev channel. The `nightly` prerelease and the `dev` and
`nightly` branches date from May 2026 and are not updated.
