//! Transport-agnostic core shared by the p2 (tokio/wasip2) and p3 (native-async)
//! netperf builds: protocol/result types, percentiles, and reporting. The only
//! difference between the two builds is the socket I/O backend — never this.
pub mod stats;
pub mod ui;

pub use stats::{Dist, Direction, LatencyStats, StreamStats, TestResults};
