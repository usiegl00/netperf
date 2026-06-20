//! Transport-agnostic protocol/result types, shared by the p2 (tokio) and p3
//! (native-async) builds. These are cold-path only — constructed once per stream
//! at the end of a test and serialized for the results exchange.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
pub enum Direction {
    /// Traffic flows from Client => Server (the default)
    ClientToServer,
    /// Traffic flows from Server => Client.
    ServerToClient,
    /// Both ways.
    Bidirectional,
}

/// Exact percentiles over a set of samples (nearest-rank, no interpolation).
#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq, Default)]
pub struct Dist {
    pub count: u64,
    pub min: u64,
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub p100: u64,
    pub mean: u64,
}

impl Dist {
    /// Exact percentiles by sorting the raw samples (p100 is the true max).
    pub fn from_samples(mut v: Vec<u64>) -> Self {
        if v.is_empty() {
            return Dist::default();
        }
        v.sort_unstable();
        let n = v.len();
        // Nearest-rank: the smallest value whose rank covers p% of the samples.
        let pct = |p: u64| -> u64 {
            let rank = (p as usize * n).div_ceil(100); // ceil(p/100 * n)
            v[rank.saturating_sub(1).min(n - 1)]
        };
        let sum: u128 = v.iter().map(|&x| x as u128).sum();
        Dist {
            count: n as u64,
            min: v[0],
            p50: pct(50),
            p90: pct(90),
            p99: pct(99),
            p100: v[n - 1],
            mean: (sum / n as u128) as u64,
        }
    }
}

/// Latency-under-load correlates, all derived from one clock read per I/O completion.
#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
pub struct LatencyStats {
    /// Nanoseconds: per-write stall (sender side) or inter-arrival gap (receiver side).
    pub interval_ns: Dist,
    /// Per-window goodput in bits/sec (throughput-stability distribution). `count == 0`
    /// means a transport didn't measure it (then the goodput line is suppressed).
    pub throughput_bps: Dist,
    /// Measured cost of one monotonic clock read (ns), subtracted from `interval_ns`.
    pub clock_baseline_ns: u64,
    /// Samples discarded during warm-up.
    pub warmup_discarded: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
pub struct StreamStats {
    pub sender: bool,
    pub duration_millis: u64,
    pub bytes_transferred: u64,
    pub syscalls: u64,
    /// Present only when the test was run with latency measurement.
    pub latency: Option<LatencyStats>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq, Default)]
pub struct TestResults {
    pub streams: HashMap<usize, StreamStats>,
}
