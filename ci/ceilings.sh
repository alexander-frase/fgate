#!/bin/bash
# Linux with a systemd user session: commands run in a systemd scope, and a --now command that grows past its
# 2 GB ceiling is stopped, with fgate saying why.
set -eu
cargo build --locked -q
F=target/debug/fgate
export FGATE_DIR="$(mktemp -d)"
printf 'min_free_gb = 0\nnight_min_free_gb = 0\nnow_mem_gb = 2\n' > "$FGATE_DIR/config.txt"

"$F" status | tee "$FGATE_DIR/status.txt"
if grep -q "no memory or CPU ceilings" "$FGATE_DIR/status.txt"; then
  echo "the systemd user session didn't come up"
  exit 1
fi

"$F" sh -c 'cat /proc/self/cgroup' | tee "$FGATE_DIR/cgroup.txt"
grep -q '\.scope$' "$FGATE_DIR/cgroup.txt" || { echo "the command didn't run in a systemd scope"; exit 1; }

set +e
"$F" --now python3 -c 'b = [bytearray(50 * 2**20) for _ in range(80)]; print("allocated")' \
  > "$FGATE_DIR/big.out" 2> "$FGATE_DIR/big.err"
code=$?
set -e
cat "$FGATE_DIR/big.out" "$FGATE_DIR/big.err"
[ "$code" != 0 ] || { echo "the 4 GB --now command wasn't stopped"; exit 1; }
grep -q allocated "$FGATE_DIR/big.out" && { echo "it allocated past the ceiling"; exit 1; }
grep -q "too big to skip the line" "$FGATE_DIR/big.err" || { echo "fgate didn't say why"; exit 1; }
echo "ceilings test passed"
