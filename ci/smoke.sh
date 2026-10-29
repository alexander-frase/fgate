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

# A dev build and a release build of two checkouts, side by side in that target, both cleaning first: each keeps its
# executable, as each profile folder has a record of its own (one shared record could lose either line).
printf 'build_slots = 2\nnight_build_slots = 2\nmin_free_gb = 0\nnight_min_free_gb = 0\n' > "$FGATE_DIR/config.txt"
build a > "$W/a.log" &
(cd "$W/b" && CARGO_TARGET_DIR="$W/shared" "$F" cargo build -q --release 2>&1) > "$W/b.log" &
wait
grep -q "fgate: kept" "$W/a.log" || { cat "$W/a.log"; echo "a's dev build beside a release build kept nothing"; exit 1; }
grep -q "fgate: kept" "$W/b.log" || { cat "$W/b.log"; echo "b's release build beside a dev build kept nothing"; exit 1; }
[ "$("$W/b/target/fgate-bin/release/app")" = "checkout b" ] || { echo "b's release build kept another's code"; exit 1; }

# Builds into one shared target take turns per profile folder, as cargo's own locks do. With two build slots, a dev
# build waits for the dev build running there without taking the free slot, and a release build behind it goes past.
# A stand-in cargo finds no workspace, and "builds" by noting when it starts and ends.
printf 'build_slots = 2\nnight_build_slots = 2\nmin_free_gb = 0\nnight_min_free_gb = 0\n' > "$FGATE_DIR/config.txt"
FC="$(mktemp -d)"
cat > "$FC/fake.sh" <<'EOF'
case "$1" in locate-project|metadata) exit 1 ;; esac
echo "$FAKE-start" >> "$ORDER"; sleep "$FAKE_SECONDS"; echo "$FAKE-end" >> "$ORDER"
EOF
case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*)
        C="$FC/cargo.cmd"
        printf '@"%s" "%s" %%*\r\n' "$(cygpath -w "$BASH")" "$(cygpath -w "$FC/fake.sh")" > "$C" ;;
    *)
        C="$FC/cargo"
        printf '#!/bin/bash\nexec bash "%s" "$@"\n' "$FC/fake.sh" > "$C"
        chmod +x "$C" ;;
esac
export ORDER="$FC/order"
FAKE=a FAKE_SECONDS=6 "$F" "$C" build --target-dir "$FC/target" &
sleep 2
FAKE=b FAKE_SECONDS=0 "$F" "$C" test --target-dir "$FC/target" &
sleep 2
"$F" status | grep -q "waits for another build into" || { "$F" status; echo "the waiting dev build isn't shown"; exit 1; }
FAKE=c FAKE_SECONDS=0 "$F" "$C" build --release --target-dir "$FC/target"
wait
[ "$(tr '\n' ' ' < "$ORDER")" = "a-start c-start c-end a-end b-start b-end " ] ||
    { echo "builds into one target out of order: $(tr '\n' ' ' < "$ORDER")"; exit 1; }
echo "smoke test passed"
