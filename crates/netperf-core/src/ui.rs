//! Shared reporting — the interval/summary/latency tables. Cold-path only (runs
//! once at end of test), so it is identical for both transports.
use crate::stats::{Direction, TestResults};
use colored::Colorize;

// Bytes (u64 so the TiB/Tbit tiers don't overflow a 32-bit usize on wasm targets)
const KBYTES: u64 = 1024;
const MBYTES: u64 = 1024 * KBYTES;
const GBYTES: u64 = 1024 * MBYTES;
const TBYTES: u64 = 1024 * GBYTES;

// Bitrates
const KBITS: u64 = 1000;
const MBITS: u64 = 1000 * KBITS;
const GBITS: u64 = 1000 * MBITS;
const TBITS: u64 = 1000 * GBITS;

pub fn print_header() {
    println!("[ ID]   Interval          Transfer      Bitrate");
}
pub fn print_server_banner(port: u16) {
    println!("--------------------------------------");
    println!("{} {}", "Listening on port".cyan(), port);
    println!("--------------------------------------");
}

pub fn humanize_bytes(bytes: u64) -> String {
    if bytes < KBYTES {
        format!("{} B", bytes)
    } else if bytes < MBYTES {
        format!("{:.2} KiB", bytes as f64 / KBYTES as f64)
    } else if bytes < GBYTES {
        format!("{:.2} MiB", bytes as f64 / MBYTES as f64)
    } else if bytes < TBYTES {
        format!("{:.2} GiB", bytes as f64 / GBYTES as f64)
    } else {
        format!("{:.2} TiB", bytes as f64 / TBYTES as f64)
    }
}

pub fn humanize_bitrate(bytes: u64, duration_millis: u64) -> String {
    let bits = bytes * 8;
    let rate = (bits as f64 / duration_millis as f64) * 1000f64;
    if rate < KBITS as f64 {
        format!("{} Bits/sec", rate)
    } else if bytes < MBITS {
        format!("{:.2} Kbits/sec", rate / KBITS as f64)
    } else if bytes < GBITS {
        format!("{:.2} Mbits/sec", rate / MBITS as f64)
    } else if bytes < TBITS {
        format!("{:.2} Gbits/sec", rate / GBITS as f64)
    } else {
        format!("{:.2} Tbits/sec", rate / TBITS as f64)
    }
}

pub fn print_stats(
    id: Option<usize>,
    offset_from_start_millis: u64,
    duration_millis: u64,
    bytes_transferred: u64,
    sender: bool,
    _syscalls: u64,
    _block_size: usize,
) {
    let end_point = offset_from_start_millis + duration_millis;
    println!(
        "[{:>3}]   {:.2}..{:.2} sec  {}   {}        {}",
        id.map(|x| x.to_string()).unwrap_or_else(|| "SUM".to_owned()),
        offset_from_start_millis as f64 / 1000f64,
        end_point as f64 / 1000f64,
        humanize_bytes(bytes_transferred),
        humanize_bitrate(bytes_transferred, duration_millis),
        if sender { "sender".yellow() } else { "receiver".magenta() },
    );
}

pub fn print_summary(local_results: &TestResults, remote_results: &TestResults, direction: &Direction) {
    println!("- - - - - - - - - - - - - - - - - - - - - - - - - - - - -");
    print_summary_header();
    let mut sender_duration_millis = 0;
    let mut receiver_duration_millis = 0;
    let mut sender_bytes_transferred = 0;
    let mut receiver_bytes_transferred = 0;
    for (id, local_stats) in &local_results.streams {
        print_stats(
            Some(*id), 0, local_stats.duration_millis, local_stats.bytes_transferred,
            local_stats.sender, local_stats.syscalls, 0,
        );
        if local_stats.sender {
            sender_bytes_transferred += local_stats.bytes_transferred;
            sender_duration_millis = std::cmp::max(sender_duration_millis, local_stats.duration_millis);
        } else {
            receiver_bytes_transferred += local_stats.bytes_transferred;
            receiver_duration_millis = std::cmp::max(receiver_duration_millis, local_stats.duration_millis);
        }
        if *direction != Direction::Bidirectional
            && let Some(remote_stats) = remote_results.streams.get(id)
        {
            print_stats(
                Some(*id), 0, remote_stats.duration_millis, remote_stats.bytes_transferred,
                remote_stats.sender, remote_stats.syscalls, 0,
            );
            if remote_stats.sender {
                sender_bytes_transferred += remote_stats.bytes_transferred;
                sender_duration_millis = std::cmp::max(sender_duration_millis, remote_stats.duration_millis);
            } else {
                receiver_bytes_transferred += remote_stats.bytes_transferred;
                receiver_duration_millis = std::cmp::max(receiver_duration_millis, remote_stats.duration_millis);
            }
        }
    }
    if local_results.streams.len() > 1 {
        println!();
        print_stats(None, 0, sender_duration_millis, sender_bytes_transferred, true, 0, 0);
        print_stats(None, 0, receiver_duration_millis, receiver_bytes_transferred, false, 0, 0);
    }
}

fn humanize_ns(ns: u64) -> String {
    if ns < 1_000 {
        format!("{} ns", ns)
    } else if ns < 1_000_000 {
        format!("{:.2} µs", ns as f64 / 1_000.0)
    } else if ns < 1_000_000_000 {
        format!("{:.2} ms", ns as f64 / 1_000_000.0)
    } else {
        format!("{:.2} s", ns as f64 / 1_000_000_000.0)
    }
}

fn humanize_bps(bps: u64) -> String {
    let b = bps as f64;
    if b < 1e3 {
        format!("{:.0} bits/sec", b)
    } else if b < 1e6 {
        format!("{:.2} Kbits/sec", b / 1e3)
    } else if b < 1e9 {
        format!("{:.2} Mbits/sec", b / 1e6)
    } else {
        format!("{:.2} Gbits/sec", b / 1e9)
    }
}

/// Latency-under-load report: per-stream percentiles for the write-stall / arrival-gap,
/// plus the goodput-window stability (suppressed if a transport didn't measure it).
pub fn print_latency_summary(local: &TestResults, remote: &TestResults) {
    use std::collections::BTreeSet;
    let any = |r: &TestResults| r.streams.values().any(|s| s.latency.is_some());
    if !any(local) && !any(remote) {
        return;
    }
    println!("- - - - - - - - - latency under load - - - - - - - - -");
    let ids: BTreeSet<usize> = local.streams.keys().chain(remote.streams.keys()).copied().collect();
    for id in ids {
        for results in [local, remote] {
            let Some(stats) = results.streams.get(&id) else { continue };
            let Some(lat) = &stats.latency else { continue };
            let what = if stats.sender { "write-stall  (sender)  " } else { "arrival-gap  (receiver)" };
            let d = &lat.interval_ns;
            println!(
                "[{:>3}] {}  min {}  p50 {}  p90 {}  p99 {}  p100 {}  mean {}",
                id, what,
                humanize_ns(d.min), humanize_ns(d.p50), humanize_ns(d.p90),
                humanize_ns(d.p99), humanize_ns(d.p100), humanize_ns(d.mean),
            );
            let t = &lat.throughput_bps;
            if t.count > 0 {
                println!(
                    "      goodput/10ms          min {}  p50 {}  p99 {}  max {}   [n={}, clock~{}, warmup-drop={}]",
                    humanize_bps(t.min), humanize_bps(t.p50), humanize_bps(t.p99), humanize_bps(t.p100),
                    d.count, humanize_ns(lat.clock_baseline_ns), lat.warmup_discarded,
                );
            }
        }
    }
}

fn print_summary_header() {
    println!(
        "{}   {}          {}      {}",
        "ID".bold(), "Interval".bold(), "Transfer".bold(), "Bitrate".bold()
    );
}
