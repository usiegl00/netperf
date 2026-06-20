#!/usr/bin/env bash
# Convenience launcher: starts a netperf server under wasmtime, runs a client
# against 127.0.0.1 with whatever flags you pass, then cleans up the server.
#
#   bash tools/run.sh -t 10 -P 1 -N -L      # latency-under-load run
#   bash tools/run.sh -t 5 -P 4             # 4-stream throughput run
#   bash tools/run.sh -t 10 -R              # reverse (server sends)
set -uo pipefail
cd "$(dirname "$0")/.."

WASM=target/wasm32-wasip2/release/netperf-p2.wasm
WT=(wasmtime run -S inherit-network -S allow-ip-name-lookup "$WASM")

[ -f "$WASM" ] || { echo "missing $WASM — run: cargo build -p netperf-p2 --release --target wasm32-wasip2"; exit 1; }

# Drop any server we previously started, then launch a fresh one in the background.
pkill -f "$WASM -s" 2>/dev/null
"${WT[@]}" -s >/tmp/netperf-server.log 2>&1 &
server_pid=$!
trap 'kill "$server_pid" 2>/dev/null' EXIT
sleep 1

# Run the client with the flags you passed (defaults to 127.0.0.1).
"${WT[@]}" -c 127.0.0.1 "$@"
