#!/bin/sh
# Usage: ab.sh DATASET RUNS BIN_A BIN_B ...  -- alternates binaries, prints min/median wall time.
set -e
DATA="$1"; RUNS="$2"; shift 2
OUT="${WORK:-/tmp/ss-ab}/out"
for b in "$@"; do : > "$OUT.$(basename "$b").t" 2>/dev/null || { mkdir -p "$(dirname "$OUT")"; : > "$OUT.$(basename "$b").t"; }; done
for i in $(seq "$RUNS"); do
  for b in "$@"; do
    rm -rf "$OUT"
    s=$(date +%s.%N)
    DATA_DIR="$DATA" OUTPUT_DIR="$OUT" "$b" > /dev/null 2>&1
    echo "$(date +%s.%N) - $s" | bc >> "$OUT.$(basename "$b").t"
  done
done
for b in "$@"; do
  sort -n "$OUT.$(basename "$b").t" | awk -v n="$(basename "$b")" '{a[NR]=$1} END {printf "%-14s min %.3f  median %.3f\n", n, a[1], a[int((NR+1)/2)]}'
done
