#!/usr/bin/env bash
# Convenience launcher for the p3 backend, mirroring tools/run.sh: starts a
# netperf-p3 server under the custom host (netperf-p3-host), runs a client
# against 127.0.0.1 with whatever flags you pass, then cleans up the server.
#
#   bash tools/run-p3.sh -t 10 -P 100 -l 128 --bidir   # Redis-like small-message load
#   bash tools/run-p3.sh -t 10 -l 1048576              # large blocks (p3 ~2x p2)
#   bash tools/run-p3.sh -t 10 -R                      # reverse (server sends)
set -uo pipefail
cd "$(dirname "$0")/.."

HOST=crates/netperf-p3-host/target/release/netperf-p3-host
GUEST=target/wasm32-wasip2/release/netperf_p3.wasm

[ -x "$HOST" ]  || { echo "missing $HOST — run: (cd crates/netperf-p3-host && cargo build --release)"; exit 1; }
[ -f "$GUEST" ] || { echo "missing $GUEST — run: cargo build -p netperf-p3 --release --target wasm32-wasip2"; exit 1; }

# Drop any server we previously started, then launch a fresh one in the background.
pkill -f "$GUEST -s" 2>/dev/null
"$HOST" "$GUEST" -s >/tmp/netperf-p3-server.log 2>&1 &
server_pid=$!
trap 'kill "$server_pid" 2>/dev/null' EXIT
sleep 1

# Run the client with the flags you passed (defaults to 127.0.0.1).
"$HOST" "$GUEST" -c 127.0.0.1 "$@"
