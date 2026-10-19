#!/bin/bash
# Uses fgate once on this system against a private folder: status, a gated command and its exit code, a --now
# command while a build holds the only slot, and the history.
set -eu
cargo build --locked -q
F=target/debug/fgate
export FGATE_DIR="$(mktemp -d)"
printf 'build_slots = 1\nmin_free_gb = 0\nnight_min_free_gb = 0\n' > "$FGATE_DIR/config.txt"

"$F" status

set +e
"$F" sh -c 'echo inside; exit 3'
code=$?
set -e
[ "$code" = 3 ] || { echo "expected exit code 3, got $code"; exit 1; }

"$F" sh -c 'sleep 5' &
sleep 2
start=$(date +%s)
"$F" --now sh -c 'echo now-ran'
took=$(( $(date +%s) - start ))
[ "$took" -le 2 ] || { echo "--now waited ${took}s"; exit 1; }
wait

"$F" status
grep -q "	now	" "$FGATE_DIR/history.log" || { echo "no now run in the history"; exit 1; }
echo "smoke test passed"
