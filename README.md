# netperf (WASI port)

A TCP throughput/latency measurement tool (iperf3-style) ported to run as a
**WebAssembly component**. This is a fork of [AhmedSoliman/netperf](https://github.com/AhmedSoliman/netperf)
that targets WASI, with two parallel I/O backends — one per WASI generation — sharing a
single core of protocol, statistics, and reporting code.

The two backends are intentionally divergent: they are different systems built on
different I/O models, kept feature-equivalent so the *only* meaningful difference between
a run on each is the socket layer.

## Layout

```
crates/
  netperf-core/      shared, target-agnostic: protocol/result types, percentile (Dist)
                     computation, and the summary/latency reporting (ui). No I/O.
  netperf-p2/        WASI Preview 2 (wasm32-wasip2) build on tokio. Poll-based I/O
                     via wasi:io/poll. The full netperf CLI/control protocol.
  netperf-p3/        WASI Preview 3 (Preview 2 ABI, wasi:sockets@0.3 async) build.
                     No std::net, no tokio, no wasi:io/poll on the data path — native
                     async sockets via wit-bindgen. Feature-equivalent to p2.
  netperf-p3-host/   Minimal wasmtime embedding that links wasi p2+p3 and runs the
                     p3 component (the wasmtime CLI does not link p3 sockets). Native.
tools/               run.sh, host-flamegraph.sh, kernel symbolication helpers.
```

The wasm crates (`netperf-core`, `netperf-p2`) form a Cargo workspace at the repo root;
the p3 guest and its native host build standalone (excluded from the workspace because
they pin a specific wasmtime/WASI 0.3-rc toolchain).

## p2 — WASI Preview 2 + tokio

```
cargo build -p netperf-p2 --release --target wasm32-wasip2

WASM=target/wasm32-wasip2/release/netperf-p2.wasm
wasmtime run -S inherit-network -S allow-ip-name-lookup "$WASM" -s &           # server
wasmtime run -S inherit-network -S allow-ip-name-lookup "$WASM" -c 127.0.0.1 -t 10 -P 4
```

Or use the launcher, which starts a server, runs a client, and cleans up:
```
bash tools/run.sh -t 10 -P 4          # 4-stream throughput
bash tools/run.sh -t 10 -R            # reverse (server sends)
bash tools/run.sh -t 10 -N -L         # latency-under-load
```

## p3 — WASI Preview 3 native async

Needs the custom host (the `wasmtime` CLI does not link p3 sockets). See
`crates/netperf-p3/README.md` for details.
```
cargo build -p netperf-p3 --release --target wasm32-wasip2
(cd crates/netperf-p3-host && cargo build --release)

GUEST=crates/netperf-p3/target/wasm32-wasip2/release/netperf_p3.wasm
HOST=crates/netperf-p3-host/target/release/netperf-p3-host
"$HOST" "$GUEST" -s &
"$HOST" "$GUEST" -c 127.0.0.1 -t 10 -P 4
```

## CLI (both backends)

`-s` server · `-c <HOST>` client · `-p <PORT>` · `-t <SECS>` duration ·
`-l <BYTES>` block size · `-P <N>` parallel streams · `-R` reverse · `--bidir`
bidirectional. The client negotiates everything over a control connection, so the
server is given only `-s`.

## WASI port notes

The original tool assumed a multi-threaded native runtime; the port runs single-threaded
(wasip2 has no threads), fills the send buffer via `getrandom` (`wasi:random`) instead of
`/dev/urandom`, drops the raw-fd socket-buffer hack, and binds IPv4 (wasip2 dual-stack
bind is unreliable). `--socket-buffers` and `-N/--no-delay` parse but degrade to warnings
where the `wasi:sockets` interface has no equivalent.

### License
Licensed under either of Apache License, Version 2.0 or MIT license at your option.
Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
