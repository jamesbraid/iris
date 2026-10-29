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

use parking_lot::Mutex;

use super::{NetBackend, Wake};

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
///
/// It is shared (`Arc<StreamBackend>` is a [`NetBackend`] too): one handle
/// runs on the link's thread while another reports its counters. The rings
/// belong to the running handle alone.
pub struct StreamBackend {
    endpoint: StreamEndpoint,
    io: Mutex<Io>,
    tx_wake: Arc<Wake>,
    rx_wake: Arc<Wake>,
    running: Arc<AtomicBool>,
    retry: Duration,
    stats: Arc<StreamStats>,
    /// The link's count of guest frames, for the GUI.
    frames: Arc<AtomicU64>,
}

/// The rings the running backend owns.
struct Io {
    tx_cons: rtrb::Consumer<Vec<u8>>,
    // Held by the receive thread during a session, which hands it back.
    rx_prod: Option<rtrb::Producer<Vec<u8>>>,
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
        let io = Mutex::new(Io { tx_cons, rx_prod: Some(rx_prod) });
        Self { endpoint, io, tx_wake, rx_wake, running, retry: RECONNECT, stats, frames }
    }

    fn live(&self, io: &Io) -> bool {
        self.running.load(Ordering::SeqCst) && !io.tx_cons.is_abandoned()
    }

    /// Connect, retrying until it works. `None` if the backend stopped first.
    fn connect(&self, io: &mut Io) -> Option<Session> {
        let mut said = false;
        while self.live(io) {
            match self.endpoint.connect() {
                Ok(c) => return Some(c),
                Err(e) => {
                    // Once per outage; the connection is logged when it comes.
                    if !std::mem::replace(&mut said, true) {
                        eprintln!("iris: network stream {}: {e}; retrying", self.endpoint);
                    }
                    // Frames the guest sends meanwhile have nowhere to go.
                    while io.tx_cons.pop().is_ok() {
                        self.frames.fetch_add(1, Ordering::Relaxed);
                        self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    let until = Instant::now() + self.retry;
                    while self.live(io) && Instant::now() < until {
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

impl StreamBackend {
    /// `NetBackend::run` for a shared backend.
    fn run_shared(&self) {
        let mut guard = self.io.lock();
        let io = &mut *guard;
        while let Some((rd, mut wr, close)) = self.connect(io) {
            eprintln!("iris: network stream connected to {}", self.endpoint);
            let prod = io.rx_prod.take().expect("receive producer");
            let (rw, running, stats) = (self.rx_wake.clone(), self.running.clone(), self.stats.clone());
            self.stats.connected.store(true, Ordering::Relaxed);
            let rx = std::thread::Builder::new()
                .name("net-stream-rx".into())
                .spawn(move || Self::receive(rd, prod, rw, running, stats))
                .expect("net-stream rx thread");
            loop {
                self.tx_wake.wait(Duration::from_millis(10));
                let mut failed = false;
                while let Ok(frame) = io.tx_cons.pop() {
                    self.frames.fetch_add(1, Ordering::Relaxed);
                    if let Err(e) = write_frame(&mut wr, &frame) {
                        eprintln!("iris: network stream write: {e}");
                        failed = true;
                        break;
                    }
                    self.stats.to_peer.fetch_add(1, Ordering::Relaxed);
                }
                if failed || rx.is_finished() || !self.live(io) {
                    break;
                }
            }
            close();
            self.stats.connected.store(false, Ordering::Relaxed);
            io.rx_prod = Some(rx.join().expect("net-stream rx thread"));
        }
    }
}

impl NetBackend for StreamBackend {
    fn run(&mut self) {
        self.run_shared();
    }

    fn report(&self, w: &mut dyn Write) -> io::Result<()> {
        let s = &self.stats;
        writeln!(w, "Stream segment {}: {}", self.endpoint,
                 if s.connected.load(Ordering::Relaxed) { "connected" } else { "waiting for the peer" })?;
        writeln!(w, "  {} frames to the peer, {} from it, {} dropped while it was away",
                 s.to_peer.load(Ordering::Relaxed), s.from_peer.load(Ordering::Relaxed),
                 s.dropped.load(Ordering::Relaxed))
    }
}

impl NetBackend for Arc<StreamBackend> {
    fn run(&mut self) {
        self.run_shared();
    }

    fn report(&self, w: &mut dyn Write) -> io::Result<()> {
        (**self).report(w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

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
        // Too long for an Ethernet frame, a cut-off length, a cut-off frame.
        for (bad, kind) in [
            (&[0u8, 0, 0x10, 0][..], io::ErrorKind::InvalidData),
            (&[0, 0][..], io::ErrorKind::UnexpectedEof),
            (&[0, 0, 0, 4, 9][..], io::ErrorKind::UnexpectedEof),
        ] {
            let mut r = bad;
            assert_eq!(read_frame(&mut r).unwrap_err().kind(), kind);
        }
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
        status: Arc<StreamBackend>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    fn rig(endpoint: StreamEndpoint, retry: Duration) -> Rig {
        let (to_net, tx_cons) = rtrb::RingBuffer::<Vec<u8>>::new(8);
        let (rx_prod, from_net) = rtrb::RingBuffer::<Vec<u8>>::new(8);
        let (tx_wake, rx_wake) = (Wake::new(), Wake::new());
        let running = Arc::new(AtomicBool::new(true));
        let frames = Arc::new(AtomicU64::new(0));
        let mut b = StreamBackend::new(endpoint, tx_cons, rx_prod, tx_wake.clone(), rx_wake.clone(), running.clone(), frames.clone());
        b.retry = retry;
        let mut r = Rig { to_net, from_net, tx_wake, rx_wake, running, frames, status: Arc::new(b), thread: None };
        r.start();
        r
    }

    impl Rig {
        fn start(&mut self) {
            self.running.store(true, Ordering::SeqCst);
            let mut runner = self.status.clone();
            self.thread = Some(std::thread::spawn(move || runner.run()));
        }

        fn stop(&mut self) {
            self.running.store(false, Ordering::SeqCst);
            self.thread.take().unwrap().join().unwrap();
        }

        /// One frame from the guest to `peer`, and one back.
        fn exchange(&mut self, peer: &mut impl ReadWrite, out: u8, back: u8) {
            self.to_net.push(vec![out; 60]).unwrap();
            self.tx_wake.notify();
            assert_eq!(read_frame(peer).unwrap(), Some(vec![out; 60]));
            write_frame(peer, &[back; 64]).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            let got = loop {
                if let Ok(f) = self.from_net.pop() {
                    break f;
                }
                assert!(Instant::now() < deadline, "no frame from the peer");
                self.rx_wake.wait(Duration::from_millis(10));
            };
            assert_eq!(got, vec![back; 64]);
        }
    }

    trait ReadWrite: Read + Write {}
    impl<T: Read + Write> ReadWrite for T {}

    /// Frames cross a real socket both ways, TCP and unix, with the server
    /// as the peer, and the status line counts them.
    #[test]
    fn pumps_frames_both_ways() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut r = rig(StreamEndpoint::Tcp(listener.local_addr().unwrap()), RECONNECT);
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        r.exchange(&mut peer, 0xaa, 0x55);
        let mut text = Vec::new();
        r.status.report(&mut text).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert_eq!(r.frames.load(Ordering::Relaxed), 1, "the guest's frame is counted for the GUI");
        assert!(text.contains(": connected") && text.contains("1 frames to the peer, 1 from it"), "{text}");
        r.stop();

        #[cfg(unix)]
        {
            let path = std::env::temp_dir().join(format!("iris-stream-{}.sock", std::process::id()));
            let _ = std::fs::remove_file(&path);
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let mut r = rig(StreamEndpoint::Unix(path.clone()), RECONNECT);
            let (mut peer, _) = listener.accept().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            r.exchange(&mut peer, 7, 8);
            r.stop();
            let _ = std::fs::remove_file(&path);
        }
    }

    /// The server starts after the machine, and comes back after it drops
    /// the connection: the client connects, and connects again.
    #[test]
    fn connects_late_and_reconnects() {
        // Reserve a port, then free it so the first attempts are refused.
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut r = rig(StreamEndpoint::Tcp(format!("127.0.0.1:{port}").parse().unwrap()), Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(200));
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        for (out, back) in [(1, 2), (3, 4)] {
            let (mut peer, _) = listener.accept().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            r.exchange(&mut peer, out, back);
        }
        r.stop();
    }

    /// Clearing the running flag ends the backend whether it is waiting for
    /// a peer or connected, and the same backend runs again afterwards, as
    /// the IOC3 does across a stop.
    #[test]
    fn stops_when_running_clears() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        rig(StreamEndpoint::Tcp(format!("127.0.0.1:{port}").parse().unwrap()), RECONNECT).stop();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut r = rig(StreamEndpoint::Tcp(listener.local_addr().unwrap()), RECONNECT);
        let (_first, _) = listener.accept().unwrap();
        r.stop();
        r.start();
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        r.exchange(&mut peer, 5, 9);
        r.stop();
    }
}
