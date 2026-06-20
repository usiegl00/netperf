#!/usr/bin/env bash
# Combined kernel + host + wasm(guest) flamegraph for the netperf-p3 (WASI 0.3
# native-async) sender. Unlike tools/p2-flamegraph.sh this drives the custom embedding
# (netperf-p3-host), enabling its perfmap emitter (NETPERF_PERFMAP=1) so dtrace
# can symbolicate JIT'd guest frames.
#
# Any client flags you pass are forwarded to the profiled client:
#   ! bash tools/p3-flamegraph.sh                  # default: -t 10 -P 1 -l 128 (small-block, per-op bound)
#   ! bash tools/p3-flamegraph.sh -t 10 -l 1048576 # large-block (batched-copy regime)
#   ! bash tools/p3-flamegraph.sh -t 10 -R         # reverse
# Each scenario writes its own SVG (name derived from the flags).
#
# Only the dtrace line needs root; everything else runs as you. Run via the
# `!` prefix so you can type your sudo password in-session.
set -uo pipefail
cd "$(dirname "$0")/.."

HOST=crates/netperf-p3-host/target/release/netperf-p3-host
GUEST=target/wasm32-wasip2/release/netperf_p3.wasm
INFERNO="$HOME/.cargo/bin/inferno-flamegraph"
STACKS=/tmp/p3-host.stacks
FOLDED=/tmp/p3-host.folded

[ -x "$HOST" ]  || { echo "missing $HOST (cd crates/netperf-p3-host && cargo build --release)"; exit 1; }
[ -f "$GUEST" ] || { echo "missing $GUEST (cargo build -p netperf-p3 --release --target wasm32-wasip2)"; exit 1; }
[ -x "$INFERNO" ] || { echo "inferno-flamegraph not found at $INFERNO (cargo install inferno)"; exit 1; }

# Client flags to profile (default: small-block forward, the per-op-bound case).
CLIENT_ARGS=("$@")
[ ${#CLIENT_ARGS[@]} -eq 0 ] && CLIENT_ARGS=(-t 10 -P 1 -l 128)
LABEL=$(printf '%s' "${CLIENT_ARGS[*]}" | tr ' ' '_' | tr -cd 'A-Za-z0-9_.-')
LABEL=${LABEL#-}
OUT="p3-host${LABEL:+-$LABEL}.svg"

pkill -f netperf-p3-host 2>/dev/null
sleep 1
rm -f /tmp/perf-*.map 2>/dev/null   # newest readable map is chosen after the run

# Server as you, in the background (no perfmap — only the client is profiled).
"$HOST" "$GUEST" -s >/tmp/p3-srv.log 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null' EXIT
sleep 1

echo ">>> sudo dtrace will prompt for your password (it samples the client host process) <<<"
# /usr/bin/env carries NETPERF_PERFMAP through to the child (sudo would otherwise strip it).
sudo dtrace \
  -x ustackframes=128 -x stackframes=128 -x bufsize=64m -x aggsize=128m \
  -n 'profile-997 /pid == $target/ { @[stack(), ustack()] = count(); }' \
  -c "/usr/bin/env NETPERF_PERFMAP=1 $HOST $GUEST -c 127.0.0.1 ${CLIENT_ARGS[*]}" \
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
  --title "netperf-p3 (WASI 0.3 async): kernel + host + wasm guest" \
  --subtitle "client ${CLIENT_ARGS[*]}, 127.0.0.1 @ 997Hz (wasmtime $(wasmtime --version 2>/dev/null | awk '{print $2}'))" \
  --colors aqua \
  "$FOLDED" > "$OUT"

echo "WROTE $(pwd)/$OUT  ($(wc -c < "$OUT") bytes)"
echo "Open it in a browser; it is a normal interactive flamegraph SVG."
