# WASI 0.3 native-async data-plane prototype

A proof-of-concept showing the netperf data plane on **WASI Preview 3 native async
sockets** (`wasi:sockets@0.3.x`) — no `std::net`, no tokio, and **no `wasi:io/poll`
on the data path**. It exists to A/B the native-async I/O model against the
poll-based `wasm32-wasip2` + tokio build in the parent crate.

It depends on the parent's **`netperf-core`** crate for the protocol/result types,
percentile (`Dist`) computation, and the summary/latency reporting (`ui`). So those
are literally shared with the p2 build, and the output is identical — the only real
difference between the two is the socket I/O backend (tokio `TcpStream` here vs
`wasi:sockets@0.3` streams).

## Layout
- `crates/netperf-p3/` (this crate) — the guest. A `cdylib` built for `wasm32-wasip2`
  (auto-componentized), using `wit-bindgen`'s async codegen against WASI 0.3 sockets,
  with a netperf-style clap CLI:
  - `-s, --server` — listen for one connection.
  - `-c, --client <HOST>` — connect to HOST.
  - `-p, --port`, `-t, --time <secs>`, `-l, --length <bytes>` (block size).
  - `-R, --reverse` — server sends, client receives.
  - `--bidir` — both ends send and receive.
  - `-P, --parallel <N>` — N parallel data streams.
  - `-L, --latency` — collect write-stall percentiles (off by default; adds a
    per-block clock read, so it costs throughput — see the note below).

  Only the **client** is configured. It opens a **control connection** first and
  sends the negotiated parameters (direction, duration, block size, cookie) as a
  length-prefixed serde_json message; the server reads them and adapts its role. The
  **data connection(s)** then carry the transfer (this split mirrors netperf).

  - `wit/` is wasmtime's vendored WASI 0.3 WIT (must match the runtime's exact
    `0.3.x-rc` version; the published registry `0.3.0` does **not** match).
- `crates/netperf-p3-host/` — a minimal embedding that wires both `wasmtime_wasi::p2`
  and `::p3` into the linker and runs a command component under
  `component_model_async`. The `wasmtime` CLI does not link p3 sockets for generic
  commands, so a custom host is required. It is a **native** crate (excluded from the
  wasm workspace), built with a plain `cargo build`.

## Build & run
From the repo root:
```
cargo build -p netperf-p3 --release --target wasm32-wasip2     # guest
(cd crates/netperf-p3-host && cargo build --release)           # native host (wasmtime-wasi `p3`)

GUEST=target/wasm32-wasip2/release/netperf_p3.wasm
HOST=crates/netperf-p3-host/target/release/netperf-p3-host

# server takes no test flags — the client negotiates everything over the control connection
"$HOST" "$GUEST" -s &
"$HOST" "$GUEST" -c 127.0.0.1 -t 5 -l 2097152         # forward (client -> server)
"$HOST" "$GUEST" -c 127.0.0.1 -t 5 -R                 # reverse (server -> client)
"$HOST" "$GUEST" -c 127.0.0.1 -t 5 --bidir            # bidirectional
```

## Measured result (loopback, single stream)
There is a **crossover** between the two backends, set by block size (Apple M1 Max P-core
@ 3.23 GHz, single-threaded, `wasmtime 45.0.2`; large-block rows are the median of 3
trials):

| Block size | p2 (poll + tokio) | p3 (native async) |
|---|---:|---:|
| 128 B (`-L` off) | ~860 Mbit/s | ~810 Mbit/s (p2 +6%) |
| 64 KiB  | ~61 Gbit/s | ~76 Gbit/s (+25%) |
| 1 MiB   | ~57 Gbit/s | ~114 Gbit/s (~2×) |

- **Small blocks → p2 marginally ahead.** Both backends are operation-rate bound (~0.8M
  socket ops/sec on one core); the per-op cost of p3's async `stream<u8>` state machine and
  the component-model async ABI is slightly higher than p2's tight poll loop.
- **Large blocks → p3 wins, and the win scales with block size.** The host pipes the
  stream to TCP in batched copies instead of crossing the guest/host boundary with a
  poll-readiness cycle per write, so the per-op cost is amortized over a big payload.

So the native-async win is about *amortized batching at scale*, not lower per-op cost — it
does not help the small-message (e.g. Redis-like) regime, where p2 edges ahead.

These p3 numbers are after the host was switched to a single-threaded tokio runtime
(`#[tokio::main(flavor = "current_thread")]`): a flamegraph showed the default
multi-thread runtime left idle workers parked in `__psynch_cvwait` and added cross-thread
I/O wakeups for this single-threaded workload. That change is worth ~7–8% at every block
size. The remaining per-op cost is wasmtime's component-model stream machinery (a host-task
create/delete per write, copies, allocator churn), which is inherent to the async ABI and
only amortizes as blocks grow. See the root README's "Optimizing the p3 host".

> Note: write-stall latency is now behind `-L` (off by default), matching p2. It is
> comparatively expensive on p3 — enabling it costs ~17% at 128 B (vs ~1% on p2), because
> the per-block clock read is a host-boundary call on wasip2 — so leave it off for
> throughput runs.

## Status / limitations (prototype)
- **Control protocol: client-driven, with results exchange.** Only the client is
  configured. It opens a control connection, sends the negotiated parameters
  (direction/duration/block) and — after the transfer — receives the server's
  `TestResults` back, then prints a **unified summary of both ends** (the same shape
  as the p2 crate's `ui::print_summary`). Both messages are length-prefixed serde_json,
  matching the p2 crate's control framing. Verified with the server given just `-s`:
  forward/reverse/bidir all negotiate and report correctly (e.g. in reverse the client
  prints the *server's* write-stall percentiles).
- **`-P` multi-stream.** The client opens N data connections after the control
  handshake; both ends run them concurrently (`join_all`) and the summary shows
  per-stream lines plus a SUM per side/direction. The fairness yield scales to N
  streams (e.g. `-P 4` forward → 4 × ~15.5 Gbits/sec, SUM ~62; loopback-bound, split
  evenly). Streams all share one direction/role (no per-stream send/recv flip).
- **`--bidir` is fair.** Earlier it suffered self-reinforcing starvation in the
  single-threaded cooperative executor (one direction collapsed to a fraction of line
  rate). Each direction now yields to the executor once per block, bounding both to
  one block per scheduling pass — measured ~22.9 / 22.6 Gbps both ways at matched
  duration (was 80:1). The yield is bidir-only; single-direction transfers keep the
  zero-yield hot path.
- **Cookie auth on every data connection.** The client generates a per-test cookie
  (in the negotiated params); each data connection presents it as the head of the
  client→server stream, and the server validates before counting any data (mismatch →
  the connection is dropped). For forward/bidir the cookie rides the existing data
  stream. In reverse the data flows the other way, so the cookie needs its own
  client→server stream — which is kept **open for the whole transfer** (closing it
  early half-closes the connection and throttles the server's send). All directions
  run at full throughput with auth on: forward ~70, reverse ~68, bidir ~47/dir,
  `-P 4` SUM ~61 Gbits/sec.
