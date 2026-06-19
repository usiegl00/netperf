# WASI 0.3 native-async data-plane prototype

A proof-of-concept showing the netperf data plane on **WASI Preview 3 native async
sockets** (`wasi:sockets@0.3.x`) — no `std::net`, no tokio, and **no `wasi:io/poll`
on the data path**. It exists to A/B the native-async I/O model against the
poll-based `wasm32-wasip2` + tokio build in the parent crate.

## Layout
- `p3echo/` — the guest. A `cdylib` built for `wasm32-wasip2` (auto-componentized),
  using `wit-bindgen`'s async codegen against WASI 0.3 sockets. Modes (argv[1]):
  - `sink` — listen, accept, drain a stream, report bytes/throughput.
  - `source [block] [secs]` — connect, stream bulk data, report throughput +
    per-write-stall percentiles.
  - `conn` / `send1` — minimal connect / small-send smoke tests.
  - `wit/` is wasmtime's vendored WASI 0.3 WIT (must match the runtime's exact
    `0.3.x-rc` version; the published registry `0.3.0` does **not** match).
- `p3host/` — a minimal embedding (~40 lines) that wires both `wasmtime_wasi::p2`
  and `::p3` into the linker and runs a command component under
  `component_model_async`. The `wasmtime` CLI does not link p3 sockets for generic
  commands, so a custom host is required.

## Build & run
```
# guest (needs the wasm32-wasip2 target)
(cd p3echo && cargo build --release --target wasm32-wasip2)
# host (needs wasmtime + wasmtime-wasi with the `p3` feature)
(cd p3host && cargo build --release)

ECHO=p3echo/target/wasm32-wasip2/release/p3echo.wasm
p3host/target/release/p3host "$ECHO" sink &
p3host/target/release/p3host "$ECHO" source 2097152 5
```

## Measured result (loopback, single stream)
At equal block size the native-async path matches small-block throughput with a far
tighter latency tail, and at large blocks (2 MiB) roughly doubles throughput vs the
poll-based tokio path — because the host pipes a stream to TCP in batched copies
instead of crossing the guest/host boundary with a poll-readiness cycle per write.
