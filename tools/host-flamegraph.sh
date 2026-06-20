#!/usr/bin/env bash
# Combined kernel + host(wasmtime) + wasm(guest) flamegraph for the netperf
# wasip2 sender, captured with dtrace and symbolicated against wasmtime's perfmap.
#
# Only the dtrace line needs root; everything else runs as you. Run via the
# `!` prefix so you can type your sudo password in-session:
#   ! bash tools/host-flamegraph.sh
set -uo pipefail
cd "$(dirname "$0")/.."

WASM=target/wasm32-wasip2/release/netperf-p2.wasm
DUR=10
INFERNO="$HOME/.cargo/bin/inferno-flamegraph"
STACKS=/tmp/wasmtime-host.stacks
FOLDED=/tmp/wasmtime-host.folded
OUT=wasmtime-host.svg

command -v wasmtime >/dev/null || { echo "wasmtime not found"; exit 1; }
[ -x "$INFERNO" ] || { echo "inferno-flamegraph not found at $INFERNO (cargo install inferno)"; exit 1; }
[ -f "$WASM" ] || { echo "missing $WASM (cargo build -p netperf-p2 --release --target wasm32-wasip2)"; exit 1; }

pkill -f netperf-p2.wasm 2>/dev/null
sleep 1
rm -f /tmp/perf-*.map 2>/dev/null   # best-effort; root-owned maps are skipped, newest is chosen below

# Server as you, in the background.
wasmtime run -S inherit-network -S allow-ip-name-lookup "$WASM" -s >/tmp/np-srv.log 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null' EXIT
sleep 1

echo ">>> sudo dtrace will prompt for your password (it samples the client wasmtime process) <<<"
sudo dtrace \
  -x ustackframes=128 -x stackframes=128 -x bufsize=64m -x aggsize=128m \
  -n 'profile-997 /pid == $target/ { @[stack(), ustack()] = count(); }' \
  -c "wasmtime run --profile=perfmap -S inherit-network -S allow-ip-name-lookup $WASM -c 127.0.0.1 -t $DUR -P 1" \
  -o "$STACKS"

kill $SRV 2>/dev/null

MAP=$(ls -t /tmp/perf-*.map 2>/dev/null | head -1)
if [ -z "$MAP" ] || [ ! -r "$MAP" ]; then
  echo "ERROR: no readable /tmp/perf-*.map found (wasm frames cannot be symbolicated)"; exit 1
fi
echo "perfmap : $MAP ($(wc -l < "$MAP") entries)"
echo "stacks  : $STACKS ($(grep -c '^ *[0-9][0-9]*$' "$STACKS" 2>/dev/null) samples)"

python3 tools/symbolicate_stacks.py "$STACKS" "$MAP" > "$FOLDED"
echo "folded  : $FOLDED ($(wc -l < "$FOLDED") unique stacks)"

"$INFERNO" \
  --title "netperf wasip2: kernel + wasmtime host + wasm guest" \
  --subtitle "client sender, 127.0.0.1, ${DUR}s @ 997Hz (wasmtime $(wasmtime --version | awk '{print $2}'))" \
  --colors aqua \
  "$FOLDED" > "$OUT"

echo "WROTE $(pwd)/$OUT  ($(wc -c < "$OUT") bytes)"
echo "Open it in a browser; it is a normal interactive flamegraph SVG."
