#!/bin/sh
# Usage: cpu.sh DATA RUNS BIN...  -- alternates binaries; prints min and median
# total CPU seconds (task-clock) and wall seconds for each.
set -e
DATA="$1"; RUNS="$2"; shift 2
OUT="${WORK:-/tmp/ss-bench}/cpu-out"
P="${PERF:-perf}"
T=$(mktemp -d)
for i in $(seq "$RUNS"); do
  for b in "$@"; do
    rm -rf "$OUT"
    DATA_DIR="$DATA" OUTPUT_DIR="$OUT" "$P" stat -x, -e task-clock -o "$T/s" "$b" >/dev/null 2>&1
    c=$(grep task-clock "$T/s" | cut -d, -f1)
    w=$(grep -i "elapsed" "$T/s" | head -1)
    echo "$(basename "$b") $c" >> "$T/r"
  done
done
for b in "$@"; do
  n=$(basename "$b")
  grep "^$n " "$T/r" | awk '{print $2/1000}' | sort -n | awk -v n="$n" '{a[NR]=$1} END {printf "%-16s cpu min %.3f median %.3f\n", n, a[1], a[int((NR+1)/2)]}'
done
rm -rf "$T"
