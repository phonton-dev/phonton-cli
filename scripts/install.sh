#!/usr/bin/env sh
set -eu

repo="https://github.com/phonton-dev/phonton-cli"
channel="${PHONTON_CHANNEL:-stable}"
dry_run=0

usage() {
  cat <<'EOF'
Install Phonton CLI from GitHub source.

Usage:
  install.sh [--channel stable|dev|nightly|main] [--dry-run]

Environment:
  PHONTON_CHANNEL  Default channel when --channel is omitted.
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --channel)
      channel="${2:-}"
      shift 2
      ;;
    --dry-run)
      dry_run=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo is required. Install Rust from https://rustup.rs/ and rerun this script." >&2
  exit 1
fi

case "$channel" in
  stable)
    ref_args="--tag v0.1.0"
    ;;
  dev)
    ref_args="--branch dev"
    ;;
  nightly)
    ref_args="--branch nightly"
    ;;
  main)
    ref_args="--branch main"
    ;;
  *)
    echo "unknown channel: $channel" >&2
    echo "valid channels: stable, dev, nightly, main" >&2
    exit 2
    ;;
esac

cmd="cargo install --git $repo $ref_args phonton-cli --locked --force"
echo "$cmd"

if [ "$dry_run" -eq 0 ]; then
  # shellcheck disable=SC2086
  cargo install --git "$repo" $ref_args phonton-cli --locked --force
fi
