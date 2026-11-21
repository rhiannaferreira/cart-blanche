#!/bin/sh
# Correctness gate: 36 fixtures, then optional exact diffs.
#   DEV=dir DEV_ANSWER=dir     the 1M dev set and its published answer
#   BIG=dir BIG_REF=dir        a large generated set and a trusted reference output
set -e
cd "$(dirname "$0")/.."
WORK="${WORK:-/tmp/ss-check}"
cargo build --release 2>&1 | grep -E "^(error|warning)|Finished"
python3 scripts/run_fixtures.py fixtures -- ./target/release/shopping-sessions | tail -1
if [ -n "$DEV" ]; then
  rm -rf "$WORK/dev" && DATA_DIR="$DEV" OUTPUT_DIR="$WORK/dev" ./target/release/shopping-sessions
  python3 scripts/compare.py "$WORK/dev" "$DEV_ANSWER" | tail -1
fi
if [ -n "$BIG" ]; then
  rm -rf "$WORK/big" && DATA_DIR="$BIG" OUTPUT_DIR="$WORK/big" ./target/release/shopping-sessions
  python3 scripts/compare.py "$WORK/big" "$BIG_REF" | tail -1
fi
