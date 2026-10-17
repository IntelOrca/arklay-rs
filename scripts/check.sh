#!/usr/bin/env bash
# The local CI gate: exactly the commands the workflows run, minus the
# artifact packaging. Run from anywhere; `--full` also runs the ignored
# real-asset suite when ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK are set, and
# `--soak` adds the full-game soak on top of that.
#
#   ./scripts/check.sh          fmt, clippy, debug + release tests, docs
#   ./scripts/check.sh --full   the above plus the ignored real-asset suite
#   ./scripts/check.sh --soak   --full plus the full-game soak
#
# The release test run uses the `test-release` profile (release code without
# thin LTO) so the gate does not pay for the link pass a shipped binary needs.
# The soak takes half an hour or more, so it is a release-time check, not a
# routine one, and stays behind its own flag.
set -euo pipefail

cd "$(dirname "$0")/.."

full=0
soak=0
while (($#)); do
    case "$1" in
        --full) full=1 ;;
        --soak) full=1; soak=1 ;;
        *) echo "check.sh: unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

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

echo "==> cargo test --profile test-release"
cargo test --profile test-release

echo "==> cargo doc --no-deps"
cargo doc --no-deps

if [[ "$full" == "1" ]]; then
    if [[ -n "${ARKLAY_RE1_ROOT:-}" && -n "${ARKLAY_RE1_PACK:-}" ]]; then
        echo "==> cargo test --profile test-release --test real -- --ignored (real assets)"
        cargo test --profile test-release --test real -- --ignored
        if [[ "$soak" == "1" ]]; then
            echo "==> cargo test --profile test-release --test soak_real -- --ignored --nocapture (full-game soak)"
            ARKLAY_SOAK=1 cargo test --profile test-release --test soak_real -- --ignored --nocapture
        else
            echo "    (the full-game soak was skipped; pass --soak to include it)"
        fi
    else
        echo "==> skipped the real-asset suite: set ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"
    fi
fi

echo "check.sh: all green"
