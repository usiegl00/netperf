use crate::common::control::{Dist, LatencyStats, StreamStats};
use crate::common::data::TestParameters;
use crate::common::ui;
use anyhow::{bail, Result};
use futures::FutureExt;
use log::{debug, warn};
use std::convert::TryInto;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{Receiver, Sender};
// use tokio_stream::StreamExt;
// use tokio::sync::mpsc::{error::TryRecvError, Receiver, Sender};
use tokio::task::JoinHandle;
use tokio::time::{interval_at, sleep_until, Instant};

#[derive(Debug, Clone)]
pub enum WorkerMessage {
    StartLoad,
    Terminate,
}

/// Represents a connected stream connection.
pub struct StreamWorkerRef {
    pub channel: Sender<WorkerMessage>,
    pub join_handle: JoinHandle<Result<StreamStats>>,
}

pub struct StreamWorker {
    pub id: usize,
    pub stream: TcpStream,
    pub params: TestParameters,
    pub is_sending: bool,
    receiver: Receiver<WorkerMessage>,
}

impl StreamWorker {
    pub fn new(
        id: usize,
        stream: TcpStream,
        params: TestParameters,
        is_sending: bool,
        receiver: Receiver<WorkerMessage>,
    ) -> Self {
        StreamWorker {
            id,
            stream,
            params,
            is_sending,
            receiver,
        }
    }

    pub async fn run_worker(mut self) -> Result<StreamStats> {
        // Let's pre-allocate a buffer for 1 block;
        let block_size = self.params.block_size;
        let mut buffer: Vec<u8> = vec![0; block_size];
        // Let's fill the buffer with a random block if we are a sender. `getrandom` is
        // backed by `wasi:random` on wasm32-wasip2 (and the OS RNG natively).
        if self.is_sending {
            getrandom::fill(&mut buffer)
                .map_err(|e| anyhow::anyhow!("failed to generate random data: {e}"))?;
        }

        self.configure_stream_socket()?;
        // First thing is that we need to wait for the `StartLoad` signal to start sending or
        // receiving data. The `StartLoad` signal comes in after the server receives all the
        // expected data stream connections as exchanged through the TestParameters.
        debug!(
            "Data stream {} created ({}), waiting for the StartLoad signal!",
            self.id,
            if self.is_sending {
                "sending"
            } else {
                "receiving"
            }
        );
        let signal = self.receiver.recv().await;
        if !matches!(signal, Some(WorkerMessage::StartLoad)) {
            bail!("Internal communication channel for stream was terminated unexpectedly!");
        }
        // TODO: Connect to the cmdline args.
        let interval = Duration::from_secs(1);
        let start_time = Instant::now();
        let timeout_duration = Duration::from_secs(self.params.time_seconds);
        let mut bytes_transferred: u64 = 0;
        let mut syscalls: u64 = 0;
        let mut last_tick = start_time;
        let mut current_interval_bytes_transferred: u64 = 0;
        let mut current_interval_syscalls: u64 = 0;
        // --- latency-under-load instrumentation (opt-in via --latency) -------------------
        // One Instant::now() per completed I/O yields all three correlated observables:
        //   1. sender:   write-stall duration   2. receiver: inter-arrival gap   -> `samples`
        //   3. either:   per-window goodput, for throughput-stability percentiles -> `tput`
        // Gated on `measure` so the throughput path keeps its zero-clock-read hot loop.
        let measure = self.params.measure_latency;
        let warmup = if self.params.time_seconds > 2 {
            Duration::from_secs(1)
        } else {
            Duration::ZERO
        };
        let win = Duration::from_millis(10);
        let secs = self.params.time_seconds as usize;
        let mut samples: Vec<u64> =
            Vec::with_capacity(if measure { secs.saturating_mul(50_000) } else { 0 });
        let mut tput: Vec<u64> =
            Vec::with_capacity(if measure { secs.saturating_mul(120) } else { 0 });
        let clock_baseline_ns = if measure { calibrate_clock() } else { 0 };
        let mut warmup_discarded: u64 = 0;
        let mut t_prev = start_time;
        let mut win_start = start_time;
        let mut win_bytes: u64 = 0;
        // Copy these out so the hot loop can borrow `stream`/`receiver` disjointly inside
        // `select!` without re-borrowing all of `self`.
        let id = self.id;
        let is_sending = self.is_sending;
        {
            let stream = &mut self.stream;
            let receiver = &mut self.receiver;
            // The hot loop touches neither a timer nor the clock per iteration; both are O(1)
            // for the whole test. Originally every read/write was wrapped in `timeout(100ms, ..)`,
            // allocating and dropping a timer-wheel entry each iteration (~95% of CPU). Removing
            // that exposed the per-iteration `Instant::now()` in the stats check as the next
            // bottleneck (each read is a host-boundary clock call on wasip2). So:
            //   * `deadline` is a single timer, created once and re-polled, that ends the test.
            //   * `ticker` fires once per second to drive stats and supplies the timestamp, so
            //     the I/O branch never reads the clock.
            let deadline = sleep_until(start_time + timeout_duration);
            tokio::pin!(deadline);
            let mut ticker = interval_at(start_time + interval, interval);
            loop {
                let io = if is_sending {
                    stream.write(&buffer).left_future()
                } else {
                    // Read up-to the remaining bytes from the socket.
                    stream.read(&mut buffer).right_future()
                };
                tokio::select! {
                    // Poll cheap termination conditions before the hot I/O branch so the
                    // worker stops promptly when time is up or a Terminate arrives.
                    biased;
                    () = &mut deadline => {
                        debug!("Test time is up!");
                        break;
                    }
                    msg = receiver.recv() => {
                        match msg {
                            Some(WorkerMessage::StartLoad) => warn!(
                                "Unexpected StartLoad from controller, we are already running with load!"
                            ),
                            // Terminate, or the controller dropped the channel.
                            Some(WorkerMessage::Terminate) | None => break,
                        }
                    }
                    tick = ticker.tick() => {
                        // Once per second: emit interval stats using the tick instant rather
                        // than a fresh clock read.
                        let current_interval = tick - last_tick;
                        ui::print_stats(
                            Some(id),
                            (last_tick - start_time).as_millis().try_into().unwrap(),
                            current_interval.as_millis().try_into().unwrap(),
                            current_interval_bytes_transferred,
                            is_sending,
                            current_interval_syscalls,
                            block_size,
                        );
                        current_interval_bytes_transferred = 0;
                        current_interval_syscalls = 0;
                        last_tick = tick;
                    }
                    res = io => {
                        let bytes_count = res? as u64;
                        current_interval_bytes_transferred += bytes_count;
                        bytes_transferred += bytes_count;
                        if bytes_count > 0 {
                            syscalls += 1;
                            current_interval_syscalls += 1;
                            if measure {
                                // Single clock read drives observables 1/2/3.
                                let now = Instant::now();
                                win_bytes += bytes_count;
                                let win_elapsed = now - win_start;
                                if win_elapsed >= win {
                                    let s = win_elapsed.as_secs_f64();
                                    tput.push((win_bytes as f64 * 8.0 / s) as u64);
                                    win_start = now;
                                    win_bytes = 0;
                                }
                                if now - start_time >= warmup {
                                    let dt = (now - t_prev).as_nanos() as u64;
                                    samples.push(dt.saturating_sub(clock_baseline_ns));
                                } else {
                                    warmup_discarded += 1;
                                }
                                t_prev = now;
                            }
                        } else {
                            // zero means that the connection is terminated. Let's wrap this up.
                            warn!("Stream {}'s connection has been closed.", id);
                            break;
                        }
                    }
                }
            }
        }
        let duration = Instant::now() - start_time;
        let latency = if measure {
            Some(LatencyStats {
                interval_ns: Dist::from_samples(samples),
                throughput_bps: Dist::from_samples(tput),
                clock_baseline_ns,
                warmup_discarded,
            })
        } else {
            None
        };
        let stats = StreamStats {
            sender: self.is_sending,
            duration_millis: duration.as_millis().try_into().unwrap(),
            bytes_transferred,
            syscalls,
            latency,
        };

        // Drain the sockets if we are the receiving end, we need to do that to avoid failing the
        // sender stream that might still be sending data.
        if !self.is_sending {
            while self.stream.read(&mut buffer).await? != 0 {}
        }
        Ok(stats)
    }

    fn configure_stream_socket(&mut self) -> Result<()> {
        // TCP_NODELAY (Nagle control) is intentionally absent: it is not part of the
        // `wasi:sockets` interface (tracked upstream in wasi-sockets#75), so there is
        // no `-N` flag — see the README's "WASI port notes" for the Redis-simulation
        // implications.
        // Socket send/receive buffer sizing relied on raw-fd access, which is not
        // available on wasm32-wasip2 (socket duplication is unsupported).
        if self.params.socket_buffers.is_some() {
            warn!("--socket-buffers is not supported on wasm32-wasip2; ignoring.");
        }
        Ok(())
    }
}

/// Median cost of a back-to-back monotonic clock read, in nanoseconds. Subtracted from each
/// stall/gap sample so the measurement reflects the wait, not the clock-read overhead — which
/// is a host-boundary call on wasm32-wasip2 and therefore non-trivial relative to a fast RTT.
fn calibrate_clock() -> u64 {
    let mut deltas = [0u64; 256];
    let mut prev = Instant::now();
    for d in deltas.iter_mut() {
        let now = Instant::now();
        *d = (now - prev).as_nanos() as u64;
        prev = now;
    }
    deltas.sort_unstable();
    deltas[deltas.len() / 2]
}
