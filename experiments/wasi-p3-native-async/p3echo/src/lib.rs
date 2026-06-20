// WASI 0.3 native-async throughput/latency bench — no std::net, no tokio, and no
// wasi:io/poll on the data path. netperf-style CLI, direction modes, a client-driven
// control protocol, and results exchange — so the output mirrors the p2/tokio build;
// the only real difference is the I/O substrate.
wit_bindgen::generate!({
    path: "wit",
    world: "echo",
    async: true,
    generate_all,
});

use clap::Parser;
use exports::wasi::cli::run::Guest;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::net::Ipv4Addr;
use std::str::FromStr;
use std::time::{Duration, Instant};
use wasi::sockets::types::{IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, TcpSocket};
use wit_bindgen::rt::async_support::StreamResult;

#[derive(Parser)]
#[command(name = "p3perf", about = "WASI 0.3 native-async throughput/latency bench")]
struct Opts {
    /// Run as server (listen; the test is negotiated by the client)
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

// ---- protocol types (mirror the p2 crate's control.rs/data.rs) -------------
const DIR_FORWARD: u8 = 0;
const DIR_REVERSE: u8 = 1;
const DIR_BIDIR: u8 = 2;

#[derive(Serialize, Deserialize)]
struct Params {
    dir: u8,
    secs: u64,
    block: u64,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct Dist {
    count: u64,
    min: u64,
    p50: u64,
    p90: u64,
    p99: u64,
    p100: u64,
    mean: u64,
}

impl Dist {
    fn from_samples(mut v: Vec<u64>) -> Self {
        if v.is_empty() {
            return Dist::default();
        }
        v.sort_unstable();
        let n = v.len();
        let pc = |p: u64| v[((p as usize * n).div_ceil(100)).saturating_sub(1).min(n - 1)];
        let sum: u128 = v.iter().map(|&x| x as u128).sum();
        Dist {
            count: n as u64,
            min: v[0],
            p50: pc(50),
            p90: pc(90),
            p99: pc(99),
            p100: v[n - 1],
            mean: (sum / n as u128) as u64,
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct StreamStats {
    sender: bool,
    duration_millis: u64,
    bytes_transferred: u64,
    /// write-stall percentiles (ns); only set for sender streams.
    stall_ns: Option<Dist>,
}

#[derive(Serialize, Deserialize, Default)]
struct TestResults {
    streams: Vec<StreamStats>,
}

// ---- control framing: u32-LE length prefix + serde_json (one msg per dir) --
async fn send_msg<T: Serialize>(sock: &TcpSocket, msg: &T) {
    let json = serde_json::to_vec(msg).unwrap_or_default();
    let mut framed = (json.len() as u32).to_le_bytes().to_vec();
    framed.extend_from_slice(&json);
    let (mut tx, rx) = wit_stream::new::<u8>();
    let send_fut = sock.send(rx).await;
    futures::join!(
        async {
            let _ = tx.write_all(framed).await;
            drop(tx);
        },
        async {
            let _ = send_fut.await;
        }
    );
}

async fn recv_msg<T: DeserializeOwned>(sock: &TcpSocket) -> Option<T> {
    let (mut rx, _done) = sock.receive().await;
    let mut acc: Vec<u8> = Vec::new();
    loop {
        if acc.len() >= 4 {
            let len = u32::from_le_bytes(acc[0..4].try_into().ok()?) as usize;
            if acc.len() >= 4 + len {
                return serde_json::from_slice(&acc[4..4 + len]).ok();
            }
        }
        let (status, b) = rx.read(Vec::with_capacity(8192)).await;
        match status {
            StreamResult::Complete(_) => acc.extend_from_slice(&b),
            _ => return None,
        }
    }
}

/// Map (direction, am-I-server) -> (sending, receiving).
fn roles(dir: u8, server: bool) -> (bool, bool) {
    match dir {
        DIR_BIDIR => (true, true),
        DIR_REVERSE => (server, !server),
        _ => (!server, server),
    }
}

// ---- data plane ------------------------------------------------------------
/// Yield once so co-resident futures get a turn (bidir fairness).
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

async fn send_all(sock: &TcpSocket, block: usize, secs: u64, fair: bool) -> StreamStats {
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
        let elapsed = start.elapsed();
        drop(tx);
        (total, samples, elapsed)
    };
    let ((total, samples, elapsed), ()) = futures::join!(writer, async {
        let _ = send_fut.await;
    });
    StreamStats {
        sender: true,
        duration_millis: elapsed.as_millis() as u64,
        bytes_transferred: total,
        stall_ns: Some(Dist::from_samples(samples)),
    }
}

async fn recv_all(sock: &TcpSocket, block: usize, fair: bool) -> StreamStats {
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
    StreamStats {
        sender: false,
        duration_millis: t0.elapsed().as_millis() as u64,
        bytes_transferred: total,
        stall_ns: None,
    }
}

/// Run the transfer for a negotiated role; return this end's per-stream stats.
async fn run_data(sock: &TcpSocket, sending: bool, receiving: bool, block: usize, secs: u64) -> Vec<StreamStats> {
    match (sending, receiving) {
        (true, true) => {
            let (tx, rx) = futures::join!(send_all(sock, block, secs, true), recv_all(sock, block, true));
            vec![tx, rx]
        }
        (true, false) => vec![send_all(sock, block, secs, false).await],
        (false, true) => vec![recv_all(sock, block, false).await],
        (false, false) => vec![],
    }
}

// ---- reporting (unified summary of both ends, like the p2 crate's ui) ------
fn humanize_bytes(b: u64) -> String {
    let f = b as f64;
    if f < 1024.0 * 1024.0 {
        format!("{:.2} KiB", f / 1024.0)
    } else if f < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} MiB", f / 1024.0 / 1024.0)
    } else {
        format!("{:.2} GiB", f / 1024.0 / 1024.0 / 1024.0)
    }
}

fn print_stream(who: &str, s: &StreamStats) {
    let secs = s.duration_millis as f64 / 1000.0;
    let gbps = if secs > 0.0 { s.bytes_transferred as f64 * 8.0 / secs / 1e9 } else { 0.0 };
    let dir = if s.sender { "TX" } else { "RX" };
    println!(
        "[{who} {dir}]  {}  in {secs:.3}s  ->  {gbps:.2} Gbits/sec",
        humanize_bytes(s.bytes_transferred)
    );
    if let Some(d) = &s.stall_ns {
        println!(
            "          write-stall ns: n={} min={} p50={} p90={} p99={} p100={} mean={}",
            d.count, d.min, d.p50, d.p90, d.p99, d.p100, d.mean
        );
    }
}

fn print_summary(local: &[StreamStats], remote: &[StreamStats]) {
    println!("- - - - - - - - - results (p3 native-async) - - - - - - - - -");
    for s in local {
        print_stream("client", s);
    }
    for s in remote {
        print_stream("server", s);
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
            let lsock = listen(o.port).await?;
            let mut conns = lsock.listen().await.map_err(|_| ())?;
            eprintln!("[p3perf] server listening on :{}", o.port);

            // Control connection: read negotiated params; keep it open for results.
            let ctrl = conns.next().await.ok_or(())?;
            let p: Params = recv_msg(&ctrl).await.ok_or(())?;
            let (sending, receiving) = roles(p.dir, true);
            eprintln!("[p3perf] negotiated dir={} secs={} block={} (tx={sending} rx={receiving})", p.dir, p.secs, p.block);

            // Data connection: run the transfer, then send our results back.
            let data = conns.next().await.ok_or(())?;
            let local = run_data(&data, sending, receiving, p.block as usize, p.secs).await;
            send_msg(&ctrl, &TestResults { streams: local }).await;
        } else {
            let host = o.client.clone().unwrap_or_else(|| "127.0.0.1".into());
            let ip = Ipv4Addr::from_str(&host).map_err(|_| ())?.octets();
            let addr = (ip[0], ip[1], ip[2], ip[3]);
            let dir = if o.bidir { DIR_BIDIR } else if o.reverse { DIR_REVERSE } else { DIR_FORWARD };

            // Control connection: negotiate, keep open for the server's results.
            let ctrl = connect(addr, o.port).await?;
            send_msg(&ctrl, &Params { dir, secs: o.time, block: o.length as u64 }).await;

            // Data connection: run the transfer.
            let data = connect(addr, o.port).await?;
            let (sending, receiving) = roles(dir, false);
            eprintln!("[p3perf] client {host}:{} dir={dir} (tx={sending} rx={receiving})", o.port);
            let local = run_data(&data, sending, receiving, o.length, o.time).await;

            // Collect the server's results and print a unified summary.
            let remote: TestResults = recv_msg(&ctrl).await.unwrap_or_default();
            print_summary(&local, &remote.streams);
        }
        Ok(())
    }
}

export!(Component);
