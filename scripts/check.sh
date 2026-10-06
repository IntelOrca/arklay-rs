#!/usr/bin/env bash
# The local CI gate: exactly the commands the workflows run, minus the
# artifact packaging. Run from anywhere; `--full` also runs the ignored
# real-asset suite when ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK are set.
#
#   ./scripts/check.sh          fmt, clippy, debug + release tests, docs
#   ./scripts/check.sh --full   the above plus the ignored real-asset suite
set -euo pipefail

cd "$(dirname "$0")/.."

full=0
if [[ "${1:-}" == "--full" ]]; then
    full=1
fi

echo "==> cargo fmt --check"
cargo fmt --check

echo "==> cargo clippy --all-targets --all-features -- -D warnings"
cargo clippy --all-targets --all-features -- -D warnings

echo "==> cargo check --no-default-features"
cargo check --no-default-features

echo "==> cargo check --manifest-path fuzz/Cargo.toml"
cargo check --manifest-path fuzz/Cargo.toml

echo "==> cargo test (debug)"
cargo test

echo "==> cargo test --release"
cargo test --release

echo "==> cargo doc --no-deps"
cargo doc --no-deps

if [[ "$full" == "1" ]]; then
    if [[ -n "${ARKLAY_RE1_ROOT:-}" && -n "${ARKLAY_RE1_PACK:-}" ]]; then
        echo "==> cargo test --release -- --ignored (real assets)"
        cargo test --release -- --ignored
    else
        echo "==> skipped the real-asset suite: set ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"
    fi
fi

echo "check.sh: all green"
