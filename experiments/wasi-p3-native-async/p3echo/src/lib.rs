// WASI 0.3 native-async throughput/latency bench — no std::net, no tokio, and no
// wasi:io/poll on the data path. netperf-style CLI, direction modes, and a control
// protocol: the client negotiates the test over a control connection, so only the
// client is configured; the server adapts.
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
    /// Run as server (listen; test parameters are negotiated by the client)
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

// ---- control protocol ------------------------------------------------------
// Direction codes carried in the negotiated parameters.
const DIR_FORWARD: u8 = 0; // client -> server
const DIR_REVERSE: u8 = 1; // server -> client
const DIR_BIDIR: u8 = 2;
const PARAMS_LEN: usize = 17; // dir(1) + secs(8) + block(8)

fn encode_params(dir: u8, secs: u64, block: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(PARAMS_LEN);
    v.push(dir);
    v.extend_from_slice(&secs.to_le_bytes());
    v.extend_from_slice(&block.to_le_bytes());
    v
}

fn decode_params(b: &[u8]) -> Option<(u8, u64, usize)> {
    if b.len() < PARAMS_LEN {
        return None;
    }
    let secs = u64::from_le_bytes(b[1..9].try_into().ok()?);
    let block = u64::from_le_bytes(b[9..17].try_into().ok()?);
    Some((b[0], secs, block as usize))
}

/// Map (direction, am-I-server) -> (sending, receiving).
fn roles(dir: u8, server: bool) -> (bool, bool) {
    match dir {
        DIR_BIDIR => (true, true),
        DIR_REVERSE => (server, !server), // server sends
        _ => (!server, server),           // forward: client sends
    }
}

/// Send a short control message and close the stream.
async fn send_control(sock: &TcpSocket, bytes: Vec<u8>) {
    let (mut tx, rx) = wit_stream::new::<u8>();
    let send_fut = sock.send(rx).await;
    futures::join!(
        async {
            let _ = tx.write_all(bytes).await;
            drop(tx);
        },
        async {
            let _ = send_fut.await;
        }
    );
}

/// Read at least `n` bytes of a control message.
async fn recv_control(sock: &TcpSocket, n: usize) -> Vec<u8> {
    let (mut rx, _done) = sock.receive().await;
    let mut acc: Vec<u8> = Vec::with_capacity(n);
    while acc.len() < n {
        let (status, b) = rx.read(Vec::with_capacity(n)).await;
        match status {
            StreamResult::Complete(_) => acc.extend_from_slice(&b),
            _ => break,
        }
    }
    acc
}

// ---- data plane ------------------------------------------------------------
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

/// Run the data transfer for a negotiated (sending, receiving) role.
async fn run_data(sock: &TcpSocket, sending: bool, receiving: bool, block: usize, secs: u64, role: &str) {
    match (sending, receiving) {
        (true, true) => {
            // Bidir: yield each iteration so neither direction starves the other.
            let ((tx_b, mut tx_s, tx_e), (rx_b, rx_e)) =
                futures::join!(send_all(sock, block, secs, true), recv_all(sock, block, true));
            report_tx(role, tx_b, &mut tx_s, tx_e);
            report_rx(role, rx_b, rx_e);
        }
        (true, false) => {
            let (b, mut s, e) = send_all(sock, block, secs, false).await;
            report_tx(role, b, &mut s, e);
        }
        (false, true) => {
            let (b, e) = recv_all(sock, block, false).await;
            report_rx(role, b, e);
        }
        (false, false) => {}
    }
}

async fn listen(port: u16) -> Result<TcpSocket, ()> {
    let s = TcpSocket::create(IpAddressFamily::Ipv4).await.map_err(|_| ())?;
    s.bind(IpSocketAddress::Ipv4(Ipv4SocketAddress { port, address: (0, 0, 0, 0) }))
        .await.map_err(|_| ())?;
    Ok(s)
}

async fn connect(addr: (u8, u8, u8, u8), port: u16) -> Result<TcpSocket, ()> {
    let s = TcpSocket::create(IpAddressFamily::Ipv4).await.map_err(|_| ())?;
    s.connect(IpSocketAddress::Ipv4(Ipv4SocketAddress { port, address: addr }))
        .await.map_err(|_| ())?;
    Ok(s)
}

struct Component;

impl Guest for Component {
    async fn run() -> Result<(), ()> {
        let o = Opts::parse();

        if o.server {
            // One listener serves the control connection then the data connection.
            let lsock = listen(o.port).await?;
            let mut conns = lsock.listen().await.map_err(|_| ())?;
            eprintln!("[p3perf] server listening on :{}", o.port);

            let ctrl = conns.next().await.ok_or(())?;
            let (dir, secs, block) = decode_params(&recv_control(&ctrl, PARAMS_LEN).await).ok_or(())?;
            drop(ctrl);
            let (sending, receiving) = roles(dir, true);
            eprintln!("[p3perf] negotiated: dir={dir} secs={secs} block={block} (tx={sending} rx={receiving})");

            let data = conns.next().await.ok_or(())?;
            run_data(&data, sending, receiving, block, secs, "server").await;
        } else {
            let host = o.client.clone().unwrap_or_else(|| "127.0.0.1".into());
            let ip = Ipv4Addr::from_str(&host).map_err(|_| ())?.octets();
            let addr = (ip[0], ip[1], ip[2], ip[3]);
            let dir = if o.bidir { DIR_BIDIR } else if o.reverse { DIR_REVERSE } else { DIR_FORWARD };

            // Control connection: negotiate, then close.
            let ctrl = connect(addr, o.port).await?;
            send_control(&ctrl, encode_params(dir, o.time, o.length as u64)).await;
            drop(ctrl);

            // Data connection.
            let data = connect(addr, o.port).await?;
            let (sending, receiving) = roles(dir, false);
            eprintln!("[p3perf] client {host}:{} dir={dir} (tx={sending} rx={receiving})", o.port);
            run_data(&data, sending, receiving, o.length, o.time, "client").await;
        }
        Ok(())
    }
}

export!(Component);
