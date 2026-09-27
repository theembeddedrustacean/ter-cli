#!/usr/bin/env bash
# The crates that record and judge a run must not reach the network: the
# evaluator never sees the site or a model, and flashing and capture stay
# local. Only ter-cli and ter-remote talk to the outside world.
#
#   scripts/check-deps.sh [workspace-root]   check the rule
#   scripts/check-deps.sh --self-test        prove the check catches a break
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
guarded=(ter-check ter-telemetry ter-flash)
forbidden='^(ter-sdk|ter-remote|ter-cli|reqwest|hyper|hyper-util|h2|ureq|isahc|curl|surf|attohttpc)$'

check() {
    local root="$1" status=0 crate tree hits
    for crate in "${guarded[@]}"; do
        if ! tree="$(cargo tree --manifest-path "$root/Cargo.toml" -p "$crate" \
            -e normal,build --target all --prefix none --format '{p}' 2>&1)"; then
            echo "check-deps: cargo tree failed for $crate:" >&2
            echo "$tree" >&2
            return 2
        fi
        hits="$(awk '{print $1}' <<<"$tree" | grep -E "$forbidden" | sort -u || true)"
        if [[ -n "$hits" ]]; then
            echo "check-deps: $crate must not depend on:" $hits >&2
            status=1
        fi
    done
    return "$status"
}

self_test() {
    local src crate dep rc failures=0
    src="$(cd "$here/.." && pwd)"
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT

    copy() {
        rm -rf "$tmp/ws" && mkdir -p "$tmp/ws"
        cp "$src/Cargo.toml" "$src/Cargo.lock" "$tmp/ws/"
        local member
        for member in $(sed -n 's/^ *"\(ter-[a-z-]*\)",$/\1/p' "$src/Cargo.toml"); do
            cp -r "$src/$member" "$tmp/ws/$member"
        done
    }

    copy
    if ! check "$tmp/ws" >/dev/null 2>&1; then
        echo "self-test: the clean workspace should pass" >&2
        failures=1
    fi
    for crate in "${guarded[@]}"; do
        for dep in 'ter-sdk = { path = "../ter-sdk" }' 'reqwest = "0.13"'; do
            copy
            sed -i "/^\[dependencies\]/a $dep" "$tmp/ws/$crate/Cargo.toml"
            rc=0
            check "$tmp/ws" >/dev/null 2>"$tmp/err" || rc=$?
            if [[ "$rc" != 1 ]]; then
                echo "self-test: adding '$dep' to $crate was not caught (exit $rc)" >&2
                cat "$tmp/err" >&2
                failures=1
            fi
        done
    done
    if [[ "$failures" == 0 ]]; then
        echo "check-deps self-test: ok"
    fi
    return "$failures"
}

if [[ "${1:-}" == "--self-test" ]]; then
    self_test
else
    check "${1:-$(cd "$here/.." && pwd)}" && echo "check-deps: ok"
fi
