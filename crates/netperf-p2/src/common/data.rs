use crate::common::opts::ClientOpts;
use serde::{Deserialize, Serialize};

// Shared with the p3 build via the transport-agnostic core.
pub use netperf_core::stats::Direction;

#[derive(Debug, Eq, PartialEq)]
pub enum Role {
    Server,
    Client,
}

#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
pub struct TestParameters {
    pub direction: Direction,
    /// omit the first n seconds.
    pub omit_seconds: u32,
    pub time_seconds: u64,
    // The number of data streams
    pub parallel: u16,
    pub block_size: usize,
    pub client_version: String,
    pub socket_buffers: Option<usize>,
    /// Collect latency-under-load correlates (write-stall / arrival-gap / goodput windows).
    pub measure_latency: bool,
}

impl TestParameters {
    pub fn from_opts(opts: &ClientOpts, default_block_size: usize) -> Self {
        let direction = if opts.bidir {
            Direction::Bidirectional
        } else if opts.reverse {
            Direction::ServerToClient
        } else {
            Direction::ClientToServer
        };
        TestParameters {
            direction,
            omit_seconds: 0,
            time_seconds: opts.time,
            parallel: opts.parallel,
            block_size: opts.length.unwrap_or(default_block_size),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            socket_buffers: opts.socket_buffers,
            measure_latency: opts.latency,
        }
    }
}
