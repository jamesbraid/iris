//! In-app IRIX serial-console viewer.
//!
//! The emulated SGI Indy exposes its serial console (ttyd1) as a loopback TCP
//! server on `127.0.0.1:8881`, or the machine's `serial_port_b` (see `iris::dev::z85c30`). This viewer connects to it
//! as a client and shows the live console stream, and lets the user type back
//! into it — so the serial console works inside the app without an external
//! terminal. It is also the visible demonstration of the app's network
//! entitlements: the emulator *listens* (network.server) and this viewer
//! *connects* (network.client), both on loopback.
//!
//! A background thread owns the socket, strips inbound telnet negotiation via
//! `iris::net::telnet::TelnetFilter`, and parks decoded text in a shared buffer.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::Mutex;

use iris::config::SerialMode;
use iris::net::telnet::{self, TelnetFilter};

/// Cap on retained scrollback so a long boot doesn't grow the buffer forever.
const MAX_TEXT: usize = 128 * 1024;

#[derive(Default)]
struct Shared {
    /// Decoded console text (telnet stripped, bare CR dropped).
    text: String,
    /// True once the TCP connection is established.
    connected: bool,
    /// Set if the connection could not be made / was lost.
    error: Option<String>,
    /// Bumped on every change so the GUI can decide whether to re-scroll.
    seq: u64,
}

pub struct SerialConsole {
    shared: Arc<Mutex<Shared>>,
    running: Arc<AtomicBool>,
    /// Write half (a clone of the socket) for sending typed input.
    write: Arc<Mutex<Option<TcpStream>>>,
    worker: Option<JoinHandle<()>>,
    /// The loopback address of the emulator's ttyd1 (IRIX serial console).
    addr: SocketAddr,
    /// The machine's `serial_mode`: what the port speaks.
    mode: SerialMode,
}

impl SerialConsole {
    /// Connect to the loopback serial console on `port` (the machine's
    /// channel B port, `MachineConfig::serial_ports()[1]`) and start streaming.
    ///
    /// `mode` is the machine's `serial_mode`. `raw` carries plain bytes both
    /// ways. `telnet` and `auto` are a telnet client, and `auto` opens with a
    /// request to suppress go-ahead so the port switches this client to
    /// telnet at once, as a viewer that filters and escapes IAC expects.
    pub fn connect(port: u16, mode: SerialMode) -> Self {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let shared = Arc::new(Mutex::new(Shared::default()));
        let running = Arc::new(AtomicBool::new(true));
        let write = Arc::new(Mutex::new(None));
        let (s2, r2, w2) = (shared.clone(), running.clone(), write.clone());
        let worker = std::thread::Builder::new()
            .name("iris-gui-serial".into())
            .spawn(move || run(addr, mode, s2, r2, w2))
            .expect("spawn serial-console worker");
        Self { shared, running, write, worker: Some(worker), addr, mode }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// (text, connected, error, seq) snapshot for rendering.
    pub fn snapshot(&self) -> (String, bool, Option<String>, u64) {
        let g = self.shared.lock();
        (g.text.clone(), g.connected, g.error.clone(), g.seq)
    }

    pub fn clear(&self) {
        let mut g = self.shared.lock();
        g.text.clear();
        g.seq = g.seq.wrapping_add(1);
    }

    /// Send bytes to the guest console (telnet-escaping 0xFF, unless raw).
    pub fn send(&self, bytes: &[u8]) {
        let esc = encode(self.mode, bytes);
        if let Some(s) = self.write.lock().as_mut() {
            let _ = s.write_all(&esc);
            let _ = s.flush();
        }
    }
}

impl Drop for SerialConsole {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        // Shutting the socket down unblocks the reader's blocking read so the
        // worker exits promptly.
        if let Some(s) = self.write.lock().take() {
            let _ = s.shutdown(Shutdown::Both);
        }
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

fn run(addr: SocketAddr, mode: SerialMode, shared: Arc<Mutex<Shared>>, running: Arc<AtomicBool>, write: Arc<Mutex<Option<TcpStream>>>) {
    let stream = match TcpStream::connect_timeout(&addr, Duration::from_millis(800)) {
        Ok(s) => s,
        Err(e) => {
            shared.lock().error = Some(t!("serial-connect-failed",
                addr = addr.to_string(), error = e.to_string()));
            return;
        }
    };
    let wclone = match stream.try_clone() {
        Ok(c) => c,
        Err(e) => {
            shared.lock().error = Some(t!("serial-socket-failed", error = e.to_string()));
            return;
        }
    };
    *write.lock() = Some(wclone);
    if !opening(mode).is_empty() {
        if let Some(s) = write.lock().as_mut() {
            let _ = s.write_all(opening(mode));
        }
    }
    // Short read timeout so the loop can observe `running` for shutdown.
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    {
        let mut g = shared.lock();
        g.connected = true;
        g.error = None;
        g.seq = g.seq.wrapping_add(1);
    }

    // Client-side telnet handling: decline negotiation, strip IAC. The guest's
    // tty echoes typed characters, so no telnet-layer echo is needed.
    let mut filter = TelnetFilter::new_passive();
    let mut buf = [0u8; 2048];
    let mut rstream = stream;

    while running.load(Ordering::Relaxed) {
        match rstream.read(&mut buf) {
            Ok(0) => break, // EOF — server closed
            Ok(n) => {
                let mut replies = Vec::new();
                let data = decode(mode, &mut filter, &buf[..n], &mut replies);
                if !replies.is_empty() {
                    if let Some(s) = write.lock().as_mut() {
                        let _ = s.write_all(&replies);
                        let _ = s.flush();
                    }
                }
                if !data.is_empty() {
                    append_text(&shared, &data);
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        }
    }

    *write.lock() = None;
    let mut g = shared.lock();
    g.connected = false;
    g.seq = g.seq.wrapping_add(1);
}

/// What a viewer says on connecting. An IAC tells an `auto` port the client
/// speaks telnet, so that the port switches to the framing the viewer
/// expects (it filters IAC and escapes 0xFF); `telnet` ports already do, and
/// `raw` ports would take the bytes as data.
fn opening(mode: SerialMode) -> &'static [u8] {
    match mode {
        SerialMode::Auto => &[telnet::IAC, telnet::DO, telnet::OPT_SGA],
        SerialMode::Raw | SerialMode::Telnet => &[],
    }
}

/// Typed bytes as they go on the wire: plain on a `raw` port, 0xFF doubled
/// on the others.
fn encode(mode: SerialMode, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for &b in bytes {
        if mode == SerialMode::Raw {
            out.push(b);
        } else {
            telnet::escape_byte(b, &mut out);
        }
    }
    out
}

/// Received bytes as the guest's text: unchanged on a `raw` port, otherwise
/// through the telnet filter, which strips commands and appends what has to
/// be answered to `replies`.
fn decode(mode: SerialMode, filter: &mut TelnetFilter, bytes: &[u8], replies: &mut Vec<u8>) -> Vec<u8> {
    if mode == SerialMode::Raw {
        return bytes.to_vec();
    }
    bytes.iter().filter_map(|&b| filter.feed(b, replies)).collect()
}

fn append_text(shared: &Arc<Mutex<Shared>>, data: &[u8]) {
    let chunk = String::from_utf8_lossy(data);
    let mut g = shared.lock();
    for ch in chunk.chars() {
        // Drop bare CR; egui handles \n line breaks.
        if ch != '\r' {
            g.text.push(ch);
        }
    }
    if g.text.len() > MAX_TEXT {
        let cut = g.text.len() - MAX_TEXT;
        let mut idx = cut;
        while !g.text.is_char_boundary(idx) {
            idx += 1;
        }
        g.text.drain(..idx);
    }
    g.seq = g.seq.wrapping_add(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    const IAC: u8 = telnet::IAC;

    /// `auto` opens with IAC DO SGA so the port treats the viewer as telnet;
    /// the other two say nothing.
    #[test]
    fn only_an_auto_port_is_told_the_viewer_speaks_telnet() {
        assert_eq!(opening(SerialMode::Auto), [IAC, telnet::DO, telnet::OPT_SGA]);
        assert!(opening(SerialMode::Telnet).is_empty());
        assert!(opening(SerialMode::Raw).is_empty());
    }

    /// 0xFF is doubled for a telnet or auto port and left alone on a raw one.
    #[test]
    fn typed_bytes_are_escaped_except_on_a_raw_port() {
        assert_eq!(encode(SerialMode::Auto, b"a\xffb\n"), b"a\xff\xffb\n");
        assert_eq!(encode(SerialMode::Telnet, b"a\xffb\n"), b"a\xff\xffb\n");
        assert_eq!(encode(SerialMode::Raw, b"a\xffb\n"), b"a\xffb\n");
    }

    /// A telnet or auto port's negotiation is stripped from the text and
    /// answered (the passive filter declines the echo it is offered); a raw
    /// port's bytes are the text, whatever they are.
    #[test]
    fn received_bytes_are_filtered_except_on_a_raw_port() {
        let offer = [IAC, telnet::WILL, telnet::OPT_ECHO, b'h', b'i', IAC, IAC, b'\n'];
        for mode in [SerialMode::Auto, SerialMode::Telnet] {
            let mut filter = TelnetFilter::new_passive();
            let mut replies = Vec::new();
            assert_eq!(decode(mode, &mut filter, &offer, &mut replies), [b'h', b'i', IAC, b'\n'], "{mode:?}");
            assert_eq!(replies, [IAC, telnet::DONT, telnet::OPT_ECHO], "{mode:?}");
        }
        let mut filter = TelnetFilter::new_passive();
        let mut replies = Vec::new();
        assert_eq!(decode(SerialMode::Raw, &mut filter, &offer, &mut replies), offer);
        assert!(replies.is_empty());
    }
}
