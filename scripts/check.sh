#!/usr/bin/env bash
# The whole local check: formatting, lints, tests and the crate dependency
# rule. Run it before pushing.
#
#   scripts/check.sh                 everything that runs offline
#   scripts/check.sh --live          also the tests against the live site
#                                    (needs TER_TOKEN for a test account)
#   scripts/check.sh --install-hook  install the git pre-commit hook
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

if [[ "${1:-}" == "--install-hook" ]]; then
    ln -sf ../../scripts/pre-commit .git/hooks/pre-commit
    echo "installed .git/hooks/pre-commit"
    exit 0
fi

set -x
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
scripts/check-deps.sh
scripts/check-deps.sh --self-test
if [[ "${1:-}" == "--live" ]]; then
    cargo test --workspace --locked -- --ignored
fi
