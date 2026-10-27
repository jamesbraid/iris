//! A private Ethernet segment carried over a byte stream, in the framing of
//! QEMU's `-netdev stream`: each frame is a 32-bit big-endian length followed
//! by that many bytes of Ethernet frame, with no FCS.
//!
//! iris connects as the client. The listening peer is whatever serves the
//! segment, for example an IRIX install server with its own user-space TCP/IP
//! stack, so the guest sees a plain LAN with no NAT gateway on it: no DHCP,
//! no DNS, nothing iris answers itself. Nothing here needs host privileges.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::Wake;

/// The largest frame the peer may send: an Ethernet frame without FCS, plus
/// room for a VLAN tag. A longer length means the stream lost framing.
pub const MAX_FRAME: usize = 1518;

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

    fn connect(&self) -> io::Result<(Box<dyn Read + Send>, Box<dyn Write + Send>, Box<dyn Fn() + Send>)> {
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

/// Connect to `endpoint` and move frames between it and the NIC's rings:
/// frames the guest sends arrive on `tx_cons` (with `tx_wake` notified), and
/// frames from the peer go on `rx_prod` with `rx_wake` notified. Retries the
/// connection once a second until it succeeds, as QEMU's `reconnect-ms`
/// does, so the server may start after the machine. The pump threads end
/// when the NIC drops its end of the rings or the peer closes.
pub fn spawn(
    endpoint: StreamEndpoint,
    mut tx_cons: rtrb::Consumer<Vec<u8>>,
    mut rx_prod: rtrb::Producer<Vec<u8>>,
    tx_wake: Arc<Wake>,
    rx_wake: Arc<Wake>,
) {
    std::thread::Builder::new()
        .name("net-stream".into())
        .spawn(move || {
            let (mut rd, mut wr, close) = loop {
                match endpoint.connect() {
                    Ok(c) => break c,
                    Err(e) => {
                        if tx_cons.is_abandoned() {
                            return;
                        }
                        eprintln!("iris: network stream {endpoint}: {e}; retrying");
                        // Frames the guest sends meanwhile have nowhere to go.
                        while tx_cons.pop().is_ok() {}
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            };
            eprintln!("iris: network stream connected to {endpoint}");
            std::thread::Builder::new()
                .name("net-stream-rx".into())
                .spawn(move || {
                    loop {
                        match read_frame(&mut rd) {
                            Ok(Some(frame)) => {
                                let mut frame = frame;
                                // Wait for the NIC to make room rather than drop:
                                // the peer's TCP retransmits cost far more.
                                loop {
                                    match rx_prod.push(frame) {
                                        Ok(()) => break,
                                        Err(rtrb::PushError::Full(f)) => {
                                            if rx_prod.is_abandoned() {
                                                return;
                                            }
                                            frame = f;
                                            rx_wake.notify();
                                            std::thread::sleep(Duration::from_micros(200));
                                        }
                                    }
                                }
                                rx_wake.notify();
                            }
                            Ok(None) => {
                                eprintln!("iris: network stream closed by peer");
                                return;
                            }
                            Err(e) => {
                                eprintln!("iris: network stream read: {e}");
                                return;
                            }
                        }
                    }
                })
                .expect("net-stream rx thread");
            loop {
                tx_wake.wait(Duration::from_millis(10));
                while let Ok(frame) = tx_cons.pop() {
                    if let Err(e) = write_frame(&mut wr, &frame) {
                        eprintln!("iris: network stream write: {e}");
                        close();
                        return;
                    }
                }
                if tx_cons.is_abandoned() {
                    close();
                    return;
                }
            }
        })
        .expect("net-stream thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn parse_endpoints() {
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
    fn framing_round_trip() {
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
    fn oversized_or_truncated_frames_are_errors() {
        let mut r: &[u8] = &[0, 0, 0x10, 0];
        assert_eq!(read_frame(&mut r).unwrap_err().kind(), io::ErrorKind::InvalidData);
        let mut r: &[u8] = &[0, 0];
        assert_eq!(read_frame(&mut r).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        let mut r: &[u8] = &[0, 0, 0, 4, 9];
        assert_eq!(read_frame(&mut r).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    /// Frames cross a real socket both ways, with the server as the peer.
    #[test]
    fn pumps_frames_both_ways() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let ep = StreamEndpoint::Tcp(listener.local_addr().unwrap());
        let (mut to_net, tx_cons) = rtrb::RingBuffer::<Vec<u8>>::new(8);
        let (rx_prod, mut from_net) = rtrb::RingBuffer::<Vec<u8>>::new(8);
        let (tx_wake, rx_wake) = (Wake::new(), Wake::new());
        spawn(ep, tx_cons, rx_prod, tx_wake.clone(), rx_wake.clone());
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

        to_net.push(vec![0xaa; 60]).unwrap();
        tx_wake.notify();
        assert_eq!(read_frame(&mut peer).unwrap(), Some(vec![0xaa; 60]));

        write_frame(&mut peer, &[0x55; 64]).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let got = loop {
            if let Ok(f) = from_net.pop() {
                break f;
            }
            assert!(std::time::Instant::now() < deadline, "no frame from the peer");
            rx_wake.wait(Duration::from_millis(10));
        };
        assert_eq!(got, vec![0x55; 64]);
    }
}
