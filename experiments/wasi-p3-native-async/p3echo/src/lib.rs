// p3-native data-plane bench: bulk transfer over WASI 0.3 async sockets.
// No std::net, no tokio, no pollables on the socket path. Timing via std::time::Instant
// (sync, wasi-0.2-backed) so the measurement isn't perturbed by an async clock hop.
wit_bindgen::generate!({
    path: "wit",
    world: "echo",
    async: true,
    generate_all,
});

use exports::wasi::cli::run::Guest;
use std::time::Instant;
use wasi::sockets::types::{IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, TcpSocket};
use wit_bindgen::rt::async_support::StreamResult;

const PORT: u16 = 7600;
const BLOCK: usize = 65536;
const SECONDS: u64 = 5;

fn pct(sorted: &[u64], p: u64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p as usize * sorted.len()).div_ceil(100);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

struct Component;

impl Guest for Component {
    async fn run() -> Result<(), ()> {
        let args: Vec<String> = std::env::args().collect();
        match args.get(1).map(|s| s.as_str()).unwrap_or("sink") {
            "source" => source().await,
            "send1" => {
                let sock = TcpSocket::create(IpAddressFamily::Ipv4).await.map_err(|_| ())?;
                sock.connect(IpSocketAddress::Ipv4(Ipv4SocketAddress { port: PORT, address: (127, 0, 0, 1) }))
                    .await.map_err(|_| ())?;
                eprintln!("[send1] connected");
                let (mut tx, rx) = wit_stream::new::<u8>();
                let send_fut = sock.send(rx).await;
                eprintln!("[send1] writing 1KB...");
                let leftover = tx.write_all(vec![0u8; 1024]).await;
                eprintln!("[send1] wrote, leftover={}", leftover.len());
                drop(tx);
                let _ = send_fut.await;
                eprintln!("[send1] send complete");
                Ok(())
            }
            "conn" => {
                let sock = TcpSocket::create(IpAddressFamily::Ipv4).await.map_err(|_| ())?;
                eprintln!("[conn] created, connecting...");
                sock.connect(IpSocketAddress::Ipv4(Ipv4SocketAddress { port: PORT, address: (127, 0, 0, 1) }))
                    .await.map_err(|_| ())?;
                eprintln!("[conn] CONNECTED — active open works");
                Ok(())
            }
            _ => sink().await,
        }
    }
}

async fn sink() -> Result<(), ()> {
    let sock = TcpSocket::create(IpAddressFamily::Ipv4).await.map_err(|_| ())?;
    sock.bind(IpSocketAddress::Ipv4(Ipv4SocketAddress { port: PORT, address: (0, 0, 0, 0) }))
        .await
        .map_err(|_| ())?;
    let mut conns = sock.listen().await.map_err(|_| ())?;
    eprintln!("[sink] listening");
    let client = conns.next().await.ok_or(())?;
    eprintln!("[sink] accepted");
    let (mut rx, _done) = client.receive().await;
    eprintln!("[sink] receiving");
    let mut total: u64 = 0;
    let mut buf: Vec<u8> = Vec::with_capacity(BLOCK);
    let t0 = Instant::now();
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
    }
    let dt = t0.elapsed().as_secs_f64();
    eprintln!("[p3 sink] {total} bytes in {dt:.3}s -> {:.2} Gbits/sec", total as f64 * 8.0 / dt / 1e9);
    Ok(())
}

async fn source() -> Result<(), ()> {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;
    let args: Vec<String> = std::env::args().collect();
    let block: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(65536);
    let seconds: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5);
    let sock = TcpSocket::create(IpAddressFamily::Ipv4).await.map_err(|_| ())?;
    sock.connect(IpSocketAddress::Ipv4(Ipv4SocketAddress { port: PORT, address: (127, 0, 0, 1) }))
        .await.map_err(|_| ())?;
    let (mut tx, rx) = wit_stream::new::<u8>();
    let send_fut = sock.send(rx).await;
    let stats: Rc<RefCell<(u64, Vec<u64>, f64)>> = Rc::new(RefCell::new((0, Vec::new(), 0.0)));
    let s2 = stats.clone();
    wit_bindgen::rt::async_support::spawn_local(async move {
        let mut buf = vec![0u8; block];
        let mut total = 0u64;
        let mut samples: Vec<u64> = Vec::with_capacity(4_000_000);
        let start = Instant::now();
        let dur = Duration::from_secs(seconds);
        while start.elapsed() < dur {
            let t0 = Instant::now();
            let leftover = tx.write_all(buf).await;
            samples.push(t0.elapsed().as_nanos() as u64);
            let wrote = block - leftover.len();
            total += wrote as u64;
            buf = if leftover.is_empty() { vec![0u8; block] } else { leftover };
            if wrote == 0 { break; }
        }
        let elapsed = start.elapsed().as_secs_f64();
        drop(tx);
        *s2.borrow_mut() = (total, samples, elapsed);
    });
    let _ = send_fut.await;
    let (total, mut samples, elapsed) = std::mem::take(&mut *stats.borrow_mut());
    samples.sort_unstable();
    let sum: u128 = samples.iter().map(|&x| x as u128).sum();
    let mean = if samples.is_empty() { 0 } else { (sum / samples.len() as u128) as u64 };
    eprintln!("[p3 source] block={block} {total} bytes in {elapsed:.3}s -> {:.2} Gbits/sec", total as f64 * 8.0 / elapsed / 1e9);
    eprintln!(
        "[p3 source] write-stall ns: n={} min={} p50={} p90={} p99={} p100={} mean={}",
        samples.len(), samples.first().copied().unwrap_or(0),
        pct(&samples, 50), pct(&samples, 90), pct(&samples, 99),
        samples.last().copied().unwrap_or(0), mean,
    );
    Ok(())
}

export!(Component);
