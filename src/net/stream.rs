//! A private Ethernet segment carried over a byte stream, in the framing of
//! QEMU's `-netdev stream`: each frame is a 32-bit big-endian length followed
//! by that many bytes of Ethernet frame, with no FCS.
//!
//! iris connects as the client. The listening peer is whatever serves the
//! segment, for example another emulator or an install server with its own
//! user-space TCP/IP stack, so the guest sees a plain LAN with no NAT gateway
//! on it: no DHCP, no DNS, nothing iris answers itself. Nothing here needs
//! host privileges.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{NetBackend, NetStatus, Wake};

/// The largest frame the peer may send: an Ethernet frame without FCS, plus
/// room for a VLAN tag. A longer length means the stream lost framing.
pub const MAX_FRAME: usize = 1518;

/// A connected stream: the read half, the write half and a function that
/// shuts the connection down, ending a blocked read.
type Session = (Box<dyn Read + Send>, Box<dyn Write + Send>, Box<dyn Fn() + Send>);

/// Where the segment's server listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEndpoint {
    Tcp(SocketAddr),
    #[cfg(unix)]
    Unix(PathBuf),
}

impl StreamEndpoint {
    /// `tcp:HOST:PORT` (HOST an IPv4 or bracketed IPv6 literal) or
    /// `unix:PATH`.
    pub fn parse(s: &str) -> Result<Self, String> {
        if let Some(addr) = s.strip_prefix("tcp:") {
            return addr
                .parse()
                .map(Self::Tcp)
                .map_err(|e| format!("\"{addr}\" is not an IP:PORT address: {e}"));
        }
        #[cfg(unix)]
        if let Some(path) = s.strip_prefix("unix:") {
            if path.is_empty() {
                return Err("unix: needs a socket path".into());
            }
            return Ok(Self::Unix(PathBuf::from(path)));
        }
        Err(format!("\"{s}\" is neither tcp:IP:PORT nor unix:PATH"))
    }

    fn connect(&self) -> io::Result<Session> {
        match self {
            Self::Tcp(addr) => {
                let s = TcpStream::connect(addr)?;
                s.set_nodelay(true)?;
                let r = s.try_clone()?;
                let c = s.try_clone()?;
                Ok((Box::new(r), Box::new(s), Box::new(move || { let _ = c.shutdown(Shutdown::Both); })))
            }
            #[cfg(unix)]
            Self::Unix(path) => {
                let s = UnixStream::connect(path)?;
                let r = s.try_clone()?;
                let c = s.try_clone()?;
                Ok((Box::new(r), Box::new(s), Box::new(move || { let _ = c.shutdown(Shutdown::Both); })))
            }
        }
    }
}

impl std::fmt::Display for StreamEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp(a) => write!(f, "tcp:{a}"),
            #[cfg(unix)]
            Self::Unix(p) => write!(f, "unix:{}", p.display()),
        }
    }
}

/// Write one frame with its length prefix.
pub fn write_frame(w: &mut impl Write, frame: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(4 + frame.len());
    buf.extend_from_slice(&(frame.len() as u32).to_be_bytes());
    buf.extend_from_slice(frame);
    w.write_all(&buf)
}

/// Read one frame. `Ok(None)` is a clean end of stream between frames.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut len[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("frame length {len}")));
    }
    let mut frame = vec![0u8; len];
    r.read_exact(&mut frame)?;
    Ok(Some(frame))
}

impl super::GatewayConfig {
    /// The `[network] stream` peer, for a machine whose mode is
    /// [`NetMode::Stream`](crate::config::NetMode::Stream). `validate` has
    /// already parsed it; a missing or bad value here is a bug in the caller.
    pub fn stream_endpoint(&self) -> Result<StreamEndpoint, String> {
        let s = self.stream.as_deref().ok_or("network.mode = \"stream\" needs [network] stream")?;
        StreamEndpoint::parse(s)
    }
}

/// The backend for `[network] mode = "stream"`: connects to `endpoint` and
/// moves frames between it and the NIC's rings. Frames the guest sends
/// arrive on `tx_cons` (with `tx_wake` notified), and frames from the peer go
/// on `rx_prod` with `rx_wake` notified.
///
/// [`run`](NetBackend::run) retries the connection once a second until it
/// succeeds, as QEMU's `reconnect-ms` does, so the server may start after the
/// machine, and connects again if the peer later goes away. Frames the guest
/// sends while there is no peer are dropped, as on an unplugged cable. It
/// returns when `running` clears or the NIC drops its end of the rings, and
/// may be run again.
pub struct StreamBackend {
    endpoint: StreamEndpoint,
    tx_cons: rtrb::Consumer<Vec<u8>>,
    // Held by the receive thread during a session, which hands it back.
    rx_prod: Option<rtrb::Producer<Vec<u8>>>,
    tx_wake: Arc<Wake>,
    rx_wake: Arc<Wake>,
    running: Arc<AtomicBool>,
    retry: Duration,
    stats: Arc<StreamStats>,
    /// The link's count of guest frames, for the GUI.
    frames: Arc<AtomicU64>,
}

const RECONNECT: Duration = Duration::from_secs(1);

/// What the segment has carried, for the adapter's status command.
#[derive(Default)]
pub struct StreamStats {
    connected: AtomicBool,
    to_peer: AtomicU64,
    from_peer: AtomicU64,
    dropped: AtomicU64,
}

struct Report {
    endpoint: StreamEndpoint,
    stats: Arc<StreamStats>,
}

impl NetStatus for Report {
    fn report(&self, w: &mut dyn Write) -> io::Result<()> {
        let s = &self.stats;
        writeln!(w, "Stream segment {}: {}", self.endpoint,
                 if s.connected.load(Ordering::Relaxed) { "connected" } else { "waiting for the peer" })?;
        writeln!(w, "  {} frames to the peer, {} from it, {} dropped while it was away",
                 s.to_peer.load(Ordering::Relaxed), s.from_peer.load(Ordering::Relaxed),
                 s.dropped.load(Ordering::Relaxed))
    }
}

impl StreamBackend {
    pub fn new(
        endpoint: StreamEndpoint,
        tx_cons: rtrb::Consumer<Vec<u8>>,
        rx_prod: rtrb::Producer<Vec<u8>>,
        tx_wake: Arc<Wake>,
        rx_wake: Arc<Wake>,
        running: Arc<AtomicBool>,
        frames: Arc<AtomicU64>,
    ) -> Self {
        let stats = Arc::new(StreamStats::default());
        Self { endpoint, tx_cons, rx_prod: Some(rx_prod), tx_wake, rx_wake, running, retry: RECONNECT, stats, frames }
    }

    /// The segment's account of itself for `net status`.
    pub fn status(&self) -> Arc<dyn NetStatus> {
        Arc::new(Report { endpoint: self.endpoint.clone(), stats: self.stats.clone() })
    }

    fn live(&self) -> bool {
        self.running.load(Ordering::SeqCst) && !self.tx_cons.is_abandoned()
    }

    /// Connect, retrying until it works. `None` if the backend stopped first.
    fn connect(&mut self) -> Option<Session> {
        let mut said = false;
        while self.live() {
            match self.endpoint.connect() {
                Ok(c) => return Some(c),
                Err(e) => {
                    // Once per outage; the connection is logged when it comes.
                    if !std::mem::replace(&mut said, true) {
                        eprintln!("iris: network stream {}: {e}; retrying", self.endpoint);
                    }
                    // Frames the guest sends meanwhile have nowhere to go.
                    while self.tx_cons.pop().is_ok() {
                        self.frames.fetch_add(1, Ordering::Relaxed);
                        self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    let until = Instant::now() + self.retry;
                    while self.live() && Instant::now() < until {
                        std::thread::sleep(Duration::from_millis(20).min(self.retry));
                    }
                }
            }
        }
        None
    }

    /// The receive thread for one session: peer frames onto the ring. It
    /// hands the producer back when the session ends, so the next session
    /// keeps feeding the same ring.
    fn receive(
        mut rd: Box<dyn Read + Send>,
        mut rx_prod: rtrb::Producer<Vec<u8>>,
        rx_wake: Arc<Wake>,
        running: Arc<AtomicBool>,
        stats: Arc<StreamStats>,
    ) -> rtrb::Producer<Vec<u8>> {
        loop {
            match read_frame(&mut rd) {
                Ok(Some(mut frame)) => {
                    // Wait for the NIC to make room rather than drop: the
                    // peer's TCP retransmits cost far more.
                    loop {
                        match rx_prod.push(frame) {
                            Ok(()) => break,
                            Err(rtrb::PushError::Full(f)) => {
                                if rx_prod.is_abandoned() || !running.load(Ordering::SeqCst) {
                                    return rx_prod;
                                }
                                frame = f;
                                rx_wake.notify();
                                std::thread::sleep(Duration::from_micros(200));
                            }
                        }
                    }
                    stats.from_peer.fetch_add(1, Ordering::Relaxed);
                    rx_wake.notify();
                }
                Ok(None) => {
                    eprintln!("iris: network stream closed by peer");
                    return rx_prod;
                }
                Err(e) => {
                    eprintln!("iris: network stream read: {e}");
                    return rx_prod;
                }
            }
        }
    }
}

impl NetBackend for StreamBackend {
    fn run(&mut self) {
        while let Some((rd, mut wr, close)) = self.connect() {
            eprintln!("iris: network stream connected to {}", self.endpoint);
            let prod = self.rx_prod.take().expect("receive producer");
            let (rw, running, stats) = (self.rx_wake.clone(), self.running.clone(), self.stats.clone());
            self.stats.connected.store(true, Ordering::Relaxed);
            let rx = std::thread::Builder::new()
                .name("net-stream-rx".into())
                .spawn(move || Self::receive(rd, prod, rw, running, stats))
                .expect("net-stream rx thread");
            loop {
                self.tx_wake.wait(Duration::from_millis(10));
                let mut failed = false;
                while let Ok(frame) = self.tx_cons.pop() {
                    self.frames.fetch_add(1, Ordering::Relaxed);
                    if let Err(e) = write_frame(&mut wr, &frame) {
                        eprintln!("iris: network stream write: {e}");
                        failed = true;
                        break;
                    }
                    self.stats.to_peer.fetch_add(1, Ordering::Relaxed);
                }
                if failed || rx.is_finished() || !self.live() {
                    break;
                }
            }
            close();
            self.stats.connected.store(false, Ordering::Relaxed);
            self.rx_prod = Some(rx.join().expect("net-stream rx thread"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn endpoints_are_tcp_or_unix_and_nothing_else() {
        assert_eq!(
            StreamEndpoint::parse("tcp:127.0.0.1:7000").unwrap(),
            StreamEndpoint::Tcp("127.0.0.1:7000".parse().unwrap())
        );
        #[cfg(unix)]
        assert_eq!(StreamEndpoint::parse("unix:/run/x.sock").unwrap(), StreamEndpoint::Unix("/run/x.sock".into()));
        assert!(StreamEndpoint::parse("tcp:localhost").is_err());
        assert!(StreamEndpoint::parse("udp:1.2.3.4:5").is_err());
    }

    #[test]
    fn a_frame_is_a_big_endian_length_then_its_bytes() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &[1, 2, 3]).unwrap();
        write_frame(&mut buf, &[]).unwrap();
        assert_eq!(&buf[..7], &[0, 0, 0, 3, 1, 2, 3]);
        let mut r = &buf[..];
        assert_eq!(read_frame(&mut r).unwrap(), Some(vec![1, 2, 3]));
        assert_eq!(read_frame(&mut r).unwrap(), Some(vec![]));
        assert_eq!(read_frame(&mut r).unwrap(), None);
    }

    #[test]
    fn oversized_and_truncated_frames_are_errors() {
        let mut r: &[u8] = &[0, 0, 0x10, 0];
        assert_eq!(read_frame(&mut r).unwrap_err().kind(), io::ErrorKind::InvalidData);
        let mut r: &[u8] = &[0, 0];
        assert_eq!(read_frame(&mut r).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        let mut r: &[u8] = &[0, 0, 0, 4, 9];
        assert_eq!(read_frame(&mut r).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    /// A backend on a running flag, run on a thread, and the NIC's ends of
    /// its rings.
    struct Rig {
        to_net: rtrb::Producer<Vec<u8>>,
        from_net: rtrb::Consumer<Vec<u8>>,
        tx_wake: Arc<Wake>,
        rx_wake: Arc<Wake>,
        running: Arc<AtomicBool>,
        frames: Arc<AtomicU64>,
        status: Arc<dyn NetStatus>,
        thread: std::thread::JoinHandle<()>,
    }

    fn rig(endpoint: StreamEndpoint, retry: Duration) -> Rig {
        let (to_net, tx_cons) = rtrb::RingBuffer::<Vec<u8>>::new(8);
        let (rx_prod, from_net) = rtrb::RingBuffer::<Vec<u8>>::new(8);
        let (tx_wake, rx_wake) = (Wake::new(), Wake::new());
        let running = Arc::new(AtomicBool::new(true));
        let frames = Arc::new(AtomicU64::new(0));
        let mut b = StreamBackend::new(endpoint, tx_cons, rx_prod, tx_wake.clone(), rx_wake.clone(), running.clone(), frames.clone());
        let status = b.status();
        b.retry = retry;
        let thread = std::thread::spawn(move || b.run());
        Rig { to_net, from_net, tx_wake, rx_wake, running, frames, status, thread }
    }

    impl Rig {
        fn stop(self) {
            self.running.store(false, Ordering::SeqCst);
            self.thread.join().unwrap();
        }
    }

    fn wait_frame(from_net: &mut rtrb::Consumer<Vec<u8>>, rx_wake: &Wake) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(f) = from_net.pop() {
                return f;
            }
            assert!(Instant::now() < deadline, "no frame from the peer");
            rx_wake.wait(Duration::from_millis(10));
        }
    }

    /// Frames cross a real socket both ways, with the server as the peer.
    #[test]
    fn pumps_frames_both_ways() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut r = rig(StreamEndpoint::Tcp(listener.local_addr().unwrap()), RECONNECT);
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(10))).unwrap();

        r.to_net.push(vec![0xaa; 60]).unwrap();
        r.tx_wake.notify();
        assert_eq!(read_frame(&mut peer).unwrap(), Some(vec![0xaa; 60]));

        write_frame(&mut peer, &[0x55; 64]).unwrap();
        assert_eq!(wait_frame(&mut r.from_net, &r.rx_wake), vec![0x55; 64]);
        let mut text = Vec::new();
        r.status.report(&mut text).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert_eq!(r.frames.load(Ordering::Relaxed), 1, "the guest's frame is counted for the GUI");
        assert!(text.contains(": connected") && text.contains("1 frames to the peer, 1 from it"), "{text}");
        r.stop();
    }

    /// The server starts after the machine, and comes back after it drops
    /// the connection: the client connects, and connects again.
    #[test]
    fn connects_late_and_reconnects() {
        // Reserve a port, then free it so the first attempts are refused.
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let ep = StreamEndpoint::Tcp(format!("127.0.0.1:{port}").parse().unwrap());
        let mut r = rig(ep, Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(200));
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        write_frame(&mut peer, &[1; 60]).unwrap();
        assert_eq!(wait_frame(&mut r.from_net, &r.rx_wake), vec![1; 60]);
        drop(peer);

        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        r.to_net.push(vec![2; 64]).unwrap();
        r.tx_wake.notify();
        assert_eq!(read_frame(&mut peer).unwrap(), Some(vec![2; 64]));
        write_frame(&mut peer, &[3; 70]).unwrap();
        assert_eq!(wait_frame(&mut r.from_net, &r.rx_wake), vec![3; 70]);
        r.stop();
    }

    /// The same exchange over a unix socket.
    #[cfg(unix)]
    #[test]
    fn pumps_frames_over_a_unix_socket() {
        let path = std::env::temp_dir().join(format!("iris-stream-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let mut r = rig(StreamEndpoint::Unix(path.clone()), RECONNECT);
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        r.to_net.push(vec![7; 60]).unwrap();
        r.tx_wake.notify();
        assert_eq!(read_frame(&mut peer).unwrap(), Some(vec![7; 60]));
        write_frame(&mut peer, &[8; 64]).unwrap();
        assert_eq!(wait_frame(&mut r.from_net, &r.rx_wake), vec![8; 64]);
        r.stop();
        let _ = std::fs::remove_file(&path);
    }

    /// Clearing the running flag ends the backend whether it is waiting for
    /// a peer or connected, and it can be run again afterwards.
    #[test]
    fn stops_when_running_clears() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let ep = StreamEndpoint::Tcp(format!("127.0.0.1:{port}").parse().unwrap());
        rig(ep, RECONNECT).stop();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let (to_net, tx_cons) = rtrb::RingBuffer::<Vec<u8>>::new(8);
        let (rx_prod, mut from_net) = rtrb::RingBuffer::<Vec<u8>>::new(8);
        let (tx_wake, rx_wake) = (Wake::new(), Wake::new());
        let running = Arc::new(AtomicBool::new(true));
        let mut b = StreamBackend::new(
            StreamEndpoint::Tcp(listener.local_addr().unwrap()),
            tx_cons, rx_prod, tx_wake, rx_wake.clone(), running.clone(), Arc::default(),
        );
        let b = std::thread::spawn(move || { b.run(); b });
        let (mut peer, _) = listener.accept().unwrap();
        running.store(false, Ordering::SeqCst);
        let mut b = b.join().unwrap();
        // Second run: same rings.
        running.store(true, Ordering::SeqCst);
        let b = std::thread::spawn(move || { b.run(); b });
        let (mut peer2, _) = listener.accept().unwrap();
        peer2.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        write_frame(&mut peer2, &[9; 64]).unwrap();
        assert_eq!(wait_frame(&mut from_net, &rx_wake), vec![9; 64]);
        running.store(false, Ordering::SeqCst);
        b.join().unwrap();
        let _ = (&mut peer, to_net);
    }
}
