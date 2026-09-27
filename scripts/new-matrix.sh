#!/usr/bin/env bash
# Build a project from `ter new` for every board xiao-generate supports.
#
#   scripts/new-matrix.sh [--all-hals] [--include-std] [--include-xtensa]
#                         [--board B] [--keep]
#
# For each board (from `ter new --list-boards`) it generates a project on
# the board's default HAL (every HAL with --all-hals), installs its Rust
# target with `ter install targets`, and runs `cargo build --release`.
# xiao-generate is run with --no-install: its backend (esp-generate for
# the ESP boards) must already be installed.
# Needs no hardware. Slow: every project downloads and builds its HAL.
#
# Left out unless asked, because they install or build a lot:
#   --include-std     the esp-idf (std) HAL, which builds the whole ESP-IDF
#   --include-xtensa  the ESP32-S3 boards, which need espup's toolchain
#
# One line per project: PASS, FAIL (log path printed) or SKIP (why).
# Exits non-zero if any project fails.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
ALL_HALS=0
INCLUDE_STD=0
INCLUDE_XTENSA=0
ONLY=""
KEEP=0
while [ $# -gt 0 ]; do
  case "$1" in
    --all-hals) ALL_HALS=1 ;;
    --include-std) INCLUDE_STD=1 ;;
    --include-xtensa) INCLUDE_XTENSA=1 ;;
    --board) ONLY="$2"; shift ;;
    --keep) KEEP=1 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
  shift
done

cargo build -q -p ter-cli --manifest-path "$ROOT/Cargo.toml"
TER="$ROOT/target/debug/ter"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/ter-new-matrix.XXXXXX")
[ "$KEEP" = 1 ] || trap 'rm -rf "$WORK"' EXIT

# Boards and HALs are read from the generator, never listed here.
boards() {
  "$TER" new --list-boards | awk '/^  xiao-/ {print $1}'
}
hals() {
  # HAL lines are indented two spaces; their detail lines more.
  "$TER" new --list-hals --board "$1" | awk '/^  [a-z]/ {print $1 (/\[default\]/ ? " default" : "")}'
}

pass=0; fail=0; skip=0
for board in $(boards); do
  [ -z "$ONLY" ] || [ "$board" = "$ONLY" ] || continue
  while read -r hal default; do
    [ "$ALL_HALS" = 1 ] || [ "$default" = default ] || continue
    row="$board $hal"
    if [ "$hal" = esp-idf ] && [ "$INCLUDE_STD" = 0 ]; then
      echo "SKIP  $row  (std: --include-std)"; skip=$((skip + 1)); continue
    fi
    if [[ "$board" == xiao-esp32s3* ]] && [ "$INCLUDE_XTENSA" = 0 ]; then
      echo "SKIP  $row  (Xtensa: --include-xtensa)"; skip=$((skip + 1)); continue
    fi
    dir="$WORK/$board-$hal"
    log="$WORK/$board-$hal.log"
    if "$TER" new --board "$board" --hal "$hal" --name matrix --out "$dir" --no-install >"$log" 2>&1 \
      && "$TER" install targets --dir "$dir" >>"$log" 2>&1 \
      && (cd "$dir" && cargo build --release) >>"$log" 2>&1; then
      echo "PASS  $row"; pass=$((pass + 1))
    else
      echo "FAIL  $row  ($log)"; fail=$((fail + 1)); KEEP=1; trap - EXIT
    fi
  done < <(hals "$board")
done

summary="$pass passed, $fail failed, $skip skipped"
[ "$KEEP" = 0 ] || summary="$summary (work: $WORK)"
echo "$summary"
[ "$fail" = 0 ]
