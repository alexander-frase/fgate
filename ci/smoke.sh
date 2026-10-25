#!/bin/bash
# Uses fgate once on this system against a private folder: status, a gated command and its exit code, a --now
# command while a build holds the only slot, the history, and two checkouts taking turns in one target directory.
set -eu
cargo build --locked -q
F="$PWD/target/debug/fgate"
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

# A solo run waits for the running build, and a --now command that comes after it waits for the solo run.
O="$FGATE_DIR/order"
"$F" sh -c "sleep 3; echo build >> '$O'" &
sleep 1
"$F" --solo sh -c "echo solo-start >> '$O'; sleep 2; echo solo-end >> '$O'" &
sleep 1
"$F" --now sh -c "echo now >> '$O'"
wait
[ "$(tr '\n' ' ' < "$O")" = "build solo-start solo-end now " ] || { echo "solo run out of order: $(cat "$O")"; exit 1; }

# Output that goes to a pipe is passed on unchanged and kept for `fgate peek`, also after the run.
[ "$("$F" sh -c 'echo kept-line; echo err-line >&2' 2>/dev/null)" = "kept-line" ] || { echo "output not passed on"; exit 1; }
"$F" peek "$(basename "$PWD")" | grep -q "err-line" || { echo "peek showed no kept output"; exit 1; }

# Two checkouts of one workspace share a target directory: each build cleans the other's crates there
# (switch_clean) and keeps its own executable in its checkout (keep_bins).
W="$(mktemp -d)"
for c in a b; do
    mkdir -p "$W/$c/src"
    printf '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n' > "$W/$c/Cargo.toml"
    printf 'fn main() { println!("checkout %s"); }\n' "$c" > "$W/$c/src/main.rs"
done
build() { (cd "$W/$1" && CARGO_TARGET_DIR="$W/shared" "$F" cargo build -q 2>&1); }
ran() { "$W/$1/target/fgate-bin/debug/app"; }
build a
[ "$(ran a)" = "checkout a" ] || { echo "a's build kept no executable of its own"; exit 1; }
build b
[ "$(ran a)" = "checkout a" ] || { echo "b's build changed a's kept executable"; exit 1; }
[ "$(ran b)" = "checkout b" ] || { echo "b's build kept no executable of its own"; exit 1; }
build b | grep -q "(unchanged)" || { echo "an unchanged executable was copied again"; exit 1; }
echo "smoke test passed"
