// WASI 0.3 native-async throughput/latency bench. Same behavior as the p2/tokio
// build — protocol/result types and reporting come from the shared `netperf-core`
// crate; the only difference here is the I/O substrate (wasi:sockets@0.3 native
// async — no std::net, no tokio, no wasi:io/poll on the data path).
wit_bindgen::generate!({
    path: "wit",
    world: "echo",
    async: true,
    generate_all,
});

use clap::Parser;
use exports::wasi::cli::run::Guest;
use netperf_core::stats::{Direction, Dist, LatencyStats, StreamStats, TestResults};
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
    /// Number of parallel data streams
    #[arg(short = 'P', long, default_value_t = 1)]
    parallel: u32,
    /// Reverse: server sends, client receives
    #[arg(short = 'R', long)]
    reverse: bool,
    /// Bidirectional: both ends send and receive
    #[arg(long)]
    bidir: bool,
}

// ---- control protocol ------------------------------------------------------
const DIR_FORWARD: u8 = 0;
const DIR_REVERSE: u8 = 1;
const DIR_BIDIR: u8 = 2;

#[derive(Serialize, Deserialize)]
struct Params {
    dir: u8,
    secs: u64,
    block: u64,
    parallel: u32,
}

fn direction_of(dir: u8) -> Direction {
    match dir {
        DIR_REVERSE => Direction::ServerToClient,
        DIR_BIDIR => Direction::Bidirectional,
        _ => Direction::ClientToServer,
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

// control framing: u32-LE length prefix + serde_json (one msg per direction).
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

// ---- data plane ------------------------------------------------------------
/// Yield once so co-resident futures get a turn (bidir / multi-stream fairness).
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
        syscalls: 0,
        latency: Some(LatencyStats {
            interval_ns: Dist::from_samples(samples),
            throughput_bps: Dist::default(), // p3 doesn't track goodput windows
            clock_baseline_ns: 0,
            warmup_discarded: 0,
        }),
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
        syscalls: 0,
        latency: None,
    }
}

async fn run_data(sock: &TcpSocket, sending: bool, receiving: bool, block: usize, secs: u64, fair: bool) -> Vec<StreamStats> {
    match (sending, receiving) {
        (true, true) => {
            let (tx, rx) = futures::join!(send_all(sock, block, secs, fair), recv_all(sock, block, fair));
            vec![tx, rx]
        }
        (true, false) => vec![send_all(sock, block, secs, fair).await],
        (false, true) => vec![recv_all(sock, block, fair).await],
        (false, false) => vec![],
    }
}

async fn run_streams(socks: &[TcpSocket], sending: bool, receiving: bool, block: usize, secs: u64, fair: bool) -> Vec<StreamStats> {
    let futs = socks.iter().map(|s| run_data(s, sending, receiving, block, secs, fair));
    futures::future::join_all(futs).await.into_iter().flatten().collect()
}

fn to_results(streams: Vec<StreamStats>) -> TestResults {
    TestResults {
        streams: streams.into_iter().enumerate().collect(),
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

            let ctrl = conns.next().await.ok_or(())?;
            let p: Params = recv_msg(&ctrl).await.ok_or(())?;
            let (sending, receiving) = roles(p.dir, true);
            let fair = p.dir == DIR_BIDIR || p.parallel > 1;
            eprintln!("[p3perf] negotiated dir={} secs={} block={} P={} (tx={sending} rx={receiving})", p.dir, p.secs, p.block, p.parallel);

            let mut datas = Vec::with_capacity(p.parallel as usize);
            for _ in 0..p.parallel {
                datas.push(conns.next().await.ok_or(())?);
            }
            let local = run_streams(&datas, sending, receiving, p.block as usize, p.secs, fair).await;
            send_msg(&ctrl, &to_results(local)).await;
        } else {
            let host = o.client.clone().unwrap_or_else(|| "127.0.0.1".into());
            let ip = Ipv4Addr::from_str(&host).map_err(|_| ())?.octets();
            let addr = (ip[0], ip[1], ip[2], ip[3]);
            let dir = if o.bidir { DIR_BIDIR } else if o.reverse { DIR_REVERSE } else { DIR_FORWARD };

            let ctrl = connect(addr, o.port).await?;
            send_msg(&ctrl, &Params { dir, secs: o.time, block: o.length as u64, parallel: o.parallel }).await;

            let mut datas = Vec::with_capacity(o.parallel as usize);
            for _ in 0..o.parallel {
                datas.push(connect(addr, o.port).await?);
            }
            let (sending, receiving) = roles(dir, false);
            let fair = dir == DIR_BIDIR || o.parallel > 1;
            eprintln!("[p3perf] client {host}:{} dir={dir} P={} (tx={sending} rx={receiving})", o.port, o.parallel);
            let local = to_results(run_streams(&datas, sending, receiving, o.length, o.time, fair).await);

            let remote: TestResults = recv_msg(&ctrl).await.unwrap_or_default();
            netperf_core::ui::print_summary(&local, &remote, &direction_of(dir));
            netperf_core::ui::print_latency_summary(&local, &remote);
        }
        Ok(())
    }
}

export!(Component);
