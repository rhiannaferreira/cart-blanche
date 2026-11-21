#!/bin/sh
# Usage: bench.sh DATASET_DIR [RUNS]  -- times the native binary with stage laps.
set -e
cd "$(dirname "$0")/.."
OUT="${WORK:-/tmp/ss-bench}/out"
for i in $(seq "${2:-2}"); do
  rm -rf "$OUT"
  s=$(date +%s.%N)
  TIMING=1 DATA_DIR="$1" OUTPUT_DIR="$OUT" ./target/release/shopping-sessions 2>&1 | tr '\n' ' '
  e=$(date +%s.%N)
  echo " | wall $(echo "$e - $s" | bc)"
done
