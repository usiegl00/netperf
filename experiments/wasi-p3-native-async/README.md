# WASI 0.3 native-async data-plane prototype

A proof-of-concept showing the netperf data plane on **WASI Preview 3 native async
sockets** (`wasi:sockets@0.3.x`) — no `std::net`, no tokio, and **no `wasi:io/poll`
on the data path**. It exists to A/B the native-async I/O model against the
poll-based `wasm32-wasip2` + tokio build in the parent crate.

## Layout
- `p3echo/` — the guest. A `cdylib` built for `wasm32-wasip2` (auto-componentized),
  using `wit-bindgen`'s async codegen against WASI 0.3 sockets, with a netperf-style
  clap CLI:
  - `-s, --server` — listen for one connection.
  - `-c, --client <HOST>` — connect to HOST.
  - `-p, --port`, `-t, --time <secs>`, `-l, --length <bytes>` (block size).
  - `-R, --reverse` — server sends, client receives.
  - `--bidir` — both ends send and receive.
  - `wit/` is wasmtime's vendored WASI 0.3 WIT (must match the runtime's exact
    `0.3.x-rc` version; the published registry `0.3.0` does **not** match).
- `p3host/` — a minimal embedding (~40 lines) that wires both `wasmtime_wasi::p2`
  and `::p3` into the linker and runs a command component under
  `component_model_async`. The `wasmtime` CLI does not link p3 sockets for generic
  commands, so a custom host is required.

## Build & run
```
(cd p3echo && cargo build --release --target wasm32-wasip2)   # guest
(cd p3host && cargo build --release)                          # host (wasmtime-wasi `p3` feature)

ECHO=p3echo/target/wasm32-wasip2/release/p3echo.wasm
HOST=p3host/target/release/p3host

# forward (client -> server)
"$HOST" "$ECHO" -s &
"$HOST" "$ECHO" -c 127.0.0.1 -t 5 -l 2097152
# reverse (server -> client): pass -R to both; -t goes on the sending side (server)
"$HOST" "$ECHO" -s -R -t 5 &
"$HOST" "$ECHO" -c 127.0.0.1 -R
```

## Measured result (loopback, single stream)
At equal block size the native-async path matches small-block throughput with a far
tighter latency tail, and at large blocks (2 MiB) roughly doubles throughput vs the
poll-based tokio path — because the host pipes a stream to TCP in batched copies
instead of crossing the guest/host boundary with a poll-readiness cycle per write.

## Status / limitations (prototype)
- **No control protocol yet.** The two ends are launched manually and must be given
  matching direction flags; test duration (`-t`) applies to the *sending* side (the
  receiver drains until the peer closes). A real port needs the handshake +
  parameter negotiation from the parent crate's `control.rs`/`net_utils.rs`.
- **Single connection.** No `-P` parallel streams yet.
- **`--bidir` is fair.** Earlier it suffered self-reinforcing starvation in the
  single-threaded cooperative executor (one direction collapsed to a fraction of line
  rate). Each direction now yields to the executor once per block, bounding both to
  one block per scheduling pass — measured ~22.9 / 22.6 Gbps both ways at matched
  duration (was 80:1). The yield is bidir-only; single-direction transfers keep the
  zero-yield hot path.
