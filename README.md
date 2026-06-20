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
bash tools/run.sh -t 10 -L            # latency-under-load
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

## Profiling: full-stack flamegraphs

`tools/host-flamegraph.sh` captures a **single combined flamegraph spanning the kernel,
the wasmtime host, and the wasm guest** for a p2 throughput run. It starts a server,
samples the client with `dtrace` at 997 Hz, symbolicates wasm frames against wasmtime's
`--profile=perfmap` output, and renders an interactive SVG with `inferno`.

Whatever client flags you pass are forwarded to the profiled client, so you can
flamegraph different throughput scenarios. Each scenario writes its **own** SVG (the
filename is derived from the flags), so runs don't clobber each other and you can diff
them side by side:

```
# needs root for the dtrace sample — run with the `!` prefix so you can type the password
! bash tools/host-flamegraph.sh                  # default: -t 10 -P 1  -> wasmtime-host-t_10_-P_1.svg
! bash tools/host-flamegraph.sh -t 10 -P 4       # 4 parallel streams  -> wasmtime-host-t_10_-P_4.svg
! bash tools/host-flamegraph.sh -t 10 -R         # reverse (server sends)
! bash tools/host-flamegraph.sh -t 10 -l 2097152 # 2 MiB blocks
```

Prerequisites: build the p2 wasm first (`cargo build -p netperf-p2 --release --target
wasm32-wasip2`) and `cargo install inferno`. Open the resulting `.svg` in a browser — it's
a normal zoomable flamegraph.

**Kernel symbols.** macOS ships a sparse kernel symbol table, so kernel frames show as
large `+offsets` by default. For accurate kernel names, install the matching Kernel
Debug Kit (KDK) for your build (`sw_vers -buildVersion`), then re-symbolicate the *last*
capture without re-running the workload:

```
! bash tools/resymbolicate-kernel.sh             # reuses /tmp/wasmtime-host.stacks -> wasmtime-host-kernel.svg
```

It refuses a KDK whose build doesn't match the running kernel (the addresses would be
wrong) and falls back to approximate names rather than producing silently-incorrect ones.

(The generated `*.svg`, `*.folded`, `perf-*.map`, and `wasmtime*.json` artifacts embed
local paths and are gitignored.)

## Benchmark results: p2 vs p3

Single-stream loopback (`127.0.0.1`), client-sender, throughput is the steady-state
sender rate. Large-block rows are the median of 3 × 3-second trials (loopback throughput
at these rates is noisy); small-block rows are op-rate bound and stable.

| Block size | p2 (poll + tokio) | p3 (native async) | Winner |
|---|---:|---:|---:|
| 128 B (`-L` off) | ~756 Mbit/s (~738K ops/s) | ~756 Mbit/s (~738K ops/s) | **tie** |
| 128 B (`-L` on)  | ~747 Mbit/s | ~625 Mbit/s | **p2** |
| 64 KiB           | ~59 Gbit/s  | ~70 Gbit/s  | **p3** (+18%) |
| 1 MiB            | ~58 Gbit/s  | ~105 Gbit/s | **p3** (~2×) |

What this says:

- **There is a crossover, set by block size.** At small blocks the two backends are
  **tied** — both are operation-rate bound (~738K socket ops/sec on one core), and the
  per-op cost of p3's async-stream/component-model machinery roughly equals p2's poll
  loop. As blocks grow, p3 pulls away: the host pipes the `stream<u8>` to TCP in batched
  copies instead of crossing the guest/host boundary per write, so its win scales with
  block size (≈2× at 1 MiB).
- **p3's latency instrumentation is expensive; p2's is nearly free.** Turning on `-L`
  (a clock read per block) costs p2 ~1% but p3 ~17% at 128 B. On wasip2 a clock read is a
  host-boundary call, and p3's per-block path is more sensitive to it. So *with latency
  measurement on*, p2 wins the small-block regime — but that's an artifact of the
  measurement, not the data plane. (`-L` is off by default on both.)

**Machine / build (for reference):** Apple M1 Max, performance cores at 3.23 GHz
(single-threaded — wasip2 has no threads, so one P-core), macOS; `wasmtime 45.0.2`,
`wasm32-wasip2`. Loopback only — no NIC in the path. Absolute numbers are machine- and
runtime-specific; the **p2-vs-p3 ratios** are the portable takeaway.

## CLI (both backends)

`-s` server · `-c <HOST>` client · `-p <PORT>` · `-t <SECS>` duration ·
`-l <BYTES>` block size · `-P <N>` parallel streams · `-R` reverse · `--bidir`
bidirectional · `-L` latency-under-load. The client negotiates everything over a
control connection, so the server is given only `-s`.

There is **no `-N`/`--no-delay`**: see below.

## WASI port notes

The original tool assumed a multi-threaded native runtime; the port runs single-threaded
(wasip2 has no threads), fills the send buffer via `getrandom` (`wasi:random`) instead of
`/dev/urandom`, drops the raw-fd socket-buffer hack, and binds IPv4 (wasip2 dual-stack
bind is unreliable). `--socket-buffers` parses but degrades to a warning where the
`wasi:sockets` interface has no equivalent.

**No Nagle / `TCP_NODELAY` control.** `wasi:sockets` has no nodelay verb in Preview 2 or
0.3 — the `tcp` resource exposes only keepalive, hop-limit, and send/recv buffer sizes.
It isn't a permanent exclusion: it's an open design item upstream
([wasi-sockets#75](https://github.com/WebAssembly/wasi-sockets/issues/75)), stuck on how
nodelay should interact with the byte-stream (`stream<u8>`) I/O model — likely a
cork/`MSG_MORE`-style flush rather than a POSIX sticky boolean. There is no guest-side
escape hatch (a userspace flush forces bytes to the kernel but cannot change the kernel's
Nagle hold), so we removed the `-N` flag rather than ship one that silently no-ops. The
only lever today is host policy (patching the runtime to set the option on the OS socket),
which we deliberately do not do.

### Simulating a Redis-like workload

netperf is a bulk-streaming tool, not a request/response benchmark — it never blocks
waiting for a reply, so it cannot reproduce Redis's per-connection RTT-bound ping-pong
(`redis-benchmark`/`memtier_benchmark` are the right tools for that). What it *can*
approximate is the **wire shape** of a busy Redis server: many connections, small
payloads, traffic both directions.

```
# non-pipelined (packet-rate / small-message bound, the classic redis-benchmark hammer)
bash tools/run.sh -t 10 -P 100 -l 128 --bidir

# pipelined (throughput bound: deep pipelines / MGET / large values)
bash tools/run.sh -t 10 -P 16 -l 16384 --bidir
```

Map: `-l` small ≈ small commands/replies; `--bidir` ≈ requests up + replies down;
`-P N` high ≈ many concurrent client connections; `-L` ≈ the tail-latency you actually
care about.

**Read ops/sec, not Gbit/s.** At 128-byte blocks this moves "only" a few hundred Mbit/s
— that's expected, not a defect. The workload is **operation-rate bound, not bandwidth
bound**: a single stream sustains roughly **0.7–0.8 million socket writes/sec** (each a
`wasi:sockets` host-boundary call at ~1.2 µs), and 0.8M × 128 B simply isn't many bytes.
Real Redis is the same — an instance doing ~1M GET/SETs/sec also pushes only a few
hundred Mbit/s. The Redis-relevant number is **ops/sec** (`MiB/s ÷ block size`), which is
genuinely server-class on one core. (Throughput climbs to tens of Gbit/s as `-l` grows
and per-op cost amortizes; ~64 KiB is the sweet spot before copy/buffer effects bite.)

**Both backends are tied here — don't expect p3 to win.** On small messages p2 (poll) and
p3 (native async) sustain the same ~756 Mbit/s / ~738K ops/sec: both are operation-rate
bound and p3's async-stream machinery costs about as much per-op as p2's poll loop. p3
only pulls ahead once blocks are large enough to amortize that (≈2× at 1 MiB). See the
[Benchmark results](#benchmark-results-p2-vs-p3) section for the full table and caveats.

Caveats: with no `TCP_NODELAY`, Nagle stays on, so small back-to-back writes may coalesce
more than real Redis (which sets nodelay) — though on **loopback** Nagle rarely engages
(ACKs return in microseconds), so it barely affects these numbers; it only distorts
results over a real-RTT link. And `--bidir` here is symmetric full-duplex, whereas a
single Redis connection is serialized ping-pong — the *aggregate* NIC view across many
connections is the part that lines up.

### License
Licensed under either of Apache License, Version 2.0 or MIT license at your option.
Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
