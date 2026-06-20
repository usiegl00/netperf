// WASI 0.3 native-async throughput/latency bench — no std::net, no tokio, and no
// wasi:io/poll on the data path. Now with a netperf-style CLI and direction modes.
wit_bindgen::generate!({
    path: "wit",
    world: "echo",
    async: true,
    generate_all,
});

use clap::Parser;
use exports::wasi::cli::run::Guest;
use std::net::Ipv4Addr;
use std::str::FromStr;
use std::time::{Duration, Instant};
use wasi::sockets::types::{IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, TcpSocket};
use wit_bindgen::rt::async_support::StreamResult;

#[derive(Parser)]
#[command(name = "p3perf", about = "WASI 0.3 native-async throughput/latency bench")]
struct Opts {
    /// Run as server (listen for one connection)
    #[arg(short, long)]
    server: bool,
    /// Run as client, connecting to <HOST>
    #[arg(short, long, value_name = "HOST")]
    client: Option<String>,
    /// Port to listen on / connect to
    #[arg(short, long, default_value_t = 7600)]
    port: u16,
    /// Seconds to transmit for
    #[arg(short, long, default_value_t = 5)]
    time: u64,
    /// Block size in bytes
    #[arg(short, long, default_value_t = 65536)]
    length: usize,
    /// Reverse: server sends, client receives
    #[arg(short = 'R', long)]
    reverse: bool,
    /// Bidirectional: both ends send and receive
    #[arg(long)]
    bidir: bool,
}

/// Yield once to the executor so co-resident futures get a turn. Used in bidir to
/// bound each direction to one block per scheduling pass (≈1:1 fairness); without it
/// the receive loop runs many synchronously-ready reads per pass and starves the sender.
async fn yield_now() {
    let mut yielded = false;
    std::future::poll_fn(move |cx| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await
}

fn pct(sorted: &[u64], p: u64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p as usize * sorted.len()).div_ceil(100);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Produce data into a stream for `secs`, measuring per-write stall. Returns
/// (bytes, stall-samples-ns, elapsed-secs).
async fn send_all(sock: &TcpSocket, block: usize, secs: u64, fair: bool) -> (u64, Vec<u64>, f64) {
    let (mut tx, rx) = wit_stream::new::<u8>();
    let send_fut = sock.send(rx).await;
    // The writer fills `tx` while `send_fut` drains it to TCP — run both concurrently.
    let writer = async {
        let mut buf = vec![0u8; block];
        let mut total = 0u64;
        let mut samples: Vec<u64> = Vec::with_capacity(4_000_000);
        let start = Instant::now();
        let dur = Duration::from_secs(secs);
        while start.elapsed() < dur {
            let t0 = Instant::now();
            let leftover = tx.write_all(buf).await;
            samples.push(t0.elapsed().as_nanos() as u64);
            let wrote = block - leftover.len();
            total += wrote as u64;
            buf = if leftover.is_empty() { vec![0u8; block] } else { leftover };
            if wrote == 0 {
                break;
            }
            if fair {
                yield_now().await;
            }
        }
        let elapsed = start.elapsed().as_secs_f64();
        drop(tx);
        (total, samples, elapsed)
    };
    let (stats, ()) = futures::join!(writer, async {
        let _ = send_fut.await;
    });
    stats
}

/// Drain an inbound stream until the peer closes. Returns (bytes, elapsed-secs).
async fn recv_all(sock: &TcpSocket, block: usize, fair: bool) -> (u64, f64) {
    let (mut rx, _done) = sock.receive().await;
    let t0 = Instant::now();
    let mut total = 0u64;
    let mut buf: Vec<u8> = Vec::with_capacity(block);
    loop {
        let (status, mut b) = rx.read(buf).await;
        match status {
            StreamResult::Complete(n) => {
                total += n as u64;
                b.clear();
                buf = b;
            }
            _ => break,
        }
        if fair {
            yield_now().await;
        }
    }
    (total, t0.elapsed().as_secs_f64())
}

fn report_tx(role: &str, total: u64, samples: &mut Vec<u64>, el: f64) {
    samples.sort_unstable();
    let sum: u128 = samples.iter().map(|&x| x as u128).sum();
    let mean = if samples.is_empty() { 0 } else { (sum / samples.len() as u128) as u64 };
    eprintln!("[{role} TX] {total} bytes in {el:.3}s -> {:.2} Gbits/sec", total as f64 * 8.0 / el / 1e9);
    eprintln!(
        "[{role} TX] write-stall ns: n={} min={} p50={} p90={} p99={} p100={} mean={}",
        samples.len(), samples.first().copied().unwrap_or(0),
        pct(samples, 50), pct(samples, 90), pct(samples, 99),
        samples.last().copied().unwrap_or(0), mean,
    );
}

fn report_rx(role: &str, total: u64, el: f64) {
    eprintln!("[{role} RX] {total} bytes in {el:.3}s -> {:.2} Gbits/sec", total as f64 * 8.0 / el / 1e9);
}

struct Component;

impl Guest for Component {
    async fn run() -> Result<(), ()> {
        let o = Opts::parse();
        let server = o.server;
        // Map (role, direction) -> which directions this side drives.
        let (sending, receiving) = if o.bidir {
            (true, true)
        } else if o.reverse {
            (server, !server) // reverse: server sends, client receives
        } else {
            (!server, server) // forward: client sends, server receives
        };
        let role = if server { "server" } else { "client" };

        let peer = if server {
            let s = TcpSocket::create(IpAddressFamily::Ipv4).await.map_err(|_| ())?;
            s.bind(IpSocketAddress::Ipv4(Ipv4SocketAddress { port: o.port, address: (0, 0, 0, 0) }))
                .await.map_err(|_| ())?;
            let mut conns = s.listen().await.map_err(|_| ())?;
            eprintln!("[p3perf] server listening on :{} (tx={sending} rx={receiving})", o.port);
            conns.next().await.ok_or(())?
        } else {
            let host = o.client.clone().unwrap_or_else(|| "127.0.0.1".into());
            let ip = Ipv4Addr::from_str(&host).map_err(|_| ())?.octets();
            let s = TcpSocket::create(IpAddressFamily::Ipv4).await.map_err(|_| ())?;
            s.connect(IpSocketAddress::Ipv4(Ipv4SocketAddress {
                port: o.port,
                address: (ip[0], ip[1], ip[2], ip[3]),
            }))
            .await.map_err(|_| ())?;
            eprintln!("[p3perf] client connected {host}:{} (tx={sending} rx={receiving})", o.port);
            s
        };

        match (sending, receiving) {
            (true, true) => {
                // Bidir: yield each iteration so neither direction starves the other.
                let ((tx_b, mut tx_s, tx_e), (rx_b, rx_e)) = futures::join!(
                    send_all(&peer, o.length, o.time, true),
                    recv_all(&peer, o.length, true)
                );
                report_tx(role, tx_b, &mut tx_s, tx_e);
                report_rx(role, rx_b, rx_e);
            }
            (true, false) => {
                let (b, mut s, e) = send_all(&peer, o.length, o.time, false).await;
                report_tx(role, b, &mut s, e);
            }
            (false, true) => {
                let (b, e) = recv_all(&peer, o.length, false).await;
                report_rx(role, b, e);
            }
            (false, false) => {}
        }
        Ok(())
    }
}

export!(Component);
