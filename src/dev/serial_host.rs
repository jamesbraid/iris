//! The host end of a serial line: where a guest UART's bytes go and where
//! its input comes from.
//!
//! A [`SerialBackend`] is a TCP console, a Unix-socket console, the in-process CI backend, a file
//! tee, or nothing. Every machine's UARTs (the Indy's Z85C30, the O2's MACE
//! 16550s, the Octane's IOC3) take one, and [`console_backends`] picks them
//! for the machines whose consoles are configured the same way.

use std::fs::OpenOptions;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use crate::config::{MachineConfig, SerialMode};
use crate::net::telnet::TelnetFilter;
use crate::dev::serial_log::SerialLog;

pub trait SerialBackend: Send + Sync {
    /// Send a byte, dropping it where the host end cannot take it now.
    fn send_byte(&self, byte: u8);

    /// Send a byte if the host end can take it now. A backend that can be
    /// full (a socket nobody reads) says so here and the 16550 holds the byte
    /// and retries; the default takes every byte, as the others always did.
    /// The Unix-socket and TCP consoles refuse while their client's socket
    /// is full, and a log tee or a fan-out passes that on. A refused byte
    /// has not been sent anywhere, so retrying it never sends it twice.
    fn try_send_byte(&self, byte: u8) -> bool {
        self.send_byte(byte);
        true
    }
    fn recv_byte(&self) -> io::Result<u8>;
}

/// Drops TX bytes and never yields RX. Used as a placeholder when a channel
/// isn't wired to a host I/O source (e.g. CI mode unused channel).
pub(crate) struct NullBackend;

impl SerialBackend for NullBackend {
    fn send_byte(&self, _byte: u8) {}
    fn recv_byte(&self) -> io::Result<u8> {
        Err(io::Error::new(io::ErrorKind::WouldBlock, "null"))
    }
}

/// Tees guest-emitted bytes to a host file in addition to delegating to an
/// inner backend.  Used by `--serial-log <file>` to capture the serial
/// console transcript while the real TCP listener on port 8881 keeps
/// working unchanged.
pub struct TeeBackend {
    inner: Arc<dyn SerialBackend>,
    log: Arc<SerialLog>,
}

impl TeeBackend {
    pub fn new(inner: Arc<dyn SerialBackend>, log: Arc<SerialLog>) -> Self {
        Self { inner, log }
    }
}

impl SerialBackend for TeeBackend {
    fn send_byte(&self, byte: u8) {
        self.log.write(&[byte]);
        self.inner.send_byte(byte);
    }
    /// Logged once the inner end takes it. One the 16550 gives up on comes
    /// back through `send_byte`, so the log still has every byte.
    fn try_send_byte(&self, byte: u8) -> bool {
        let taken = self.inner.try_send_byte(byte);
        if taken {
            self.log.write(&[byte]);
        }
        taken
    }
    fn recv_byte(&self) -> io::Result<u8> {
        self.inner.recv_byte()
    }
}

/// A serial line on a Unix socket at `path`: raw bytes, no telnet, one client
/// at a time (the next waits in the listen queue until the first hangs up).
/// What a script wants from a console: it can connect without a free TCP
/// port, and the socket's directory permissions say who may.
#[cfg(unix)]
pub(crate) struct UnixSocketBackend {
    path: std::path::PathBuf,
    listener: UnixListener,
    stream: Mutex<Option<UnixStream>>,
}

/// Bind a Unix socket at `path`. A socket file left by an earlier iris is
/// replaced; one that something still answers on, or any other kind of
/// file, is an error and left alone.
#[cfg(unix)]
pub(crate) fn bind_unix_socket(path: &Path) -> io::Result<UnixListener> {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            if UnixStream::connect(path).is_ok() {
                return Err(io::Error::new(io::ErrorKind::AddrInUse, "another process is listening on it"));
            }
            std::fs::remove_file(path)?;
        }
        Ok(_) => return Err(io::Error::new(io::ErrorKind::AlreadyExists, "exists and is not a socket")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    UnixListener::bind(path)
}

#[cfg(unix)]
impl UnixSocketBackend {
    /// Bind `path`, as `bind_unix_socket` does.
    pub(crate) fn new<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let listener = bind_unix_socket(&path)?;
        listener.set_nonblocking(true)?;
        crate::workstation::remove_at_exit(&path);
        Ok(Self { path, listener, stream: Mutex::new(None) })
    }
}

#[cfg(unix)]
impl Drop for UnixSocketBackend {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        crate::workstation::keep_at_exit(&self.path);
    }
}

#[cfg(unix)]
impl SerialBackend for UnixSocketBackend {
    fn send_byte(&self, byte: u8) {
        let _ = self.try_send_byte(byte);
    }

    /// Refused while the client's socket buffer is full. With no client, or
    /// one that has gone, the byte is taken and dropped: nobody waits for it.
    fn try_send_byte(&self, byte: u8) -> bool {
        let mut stream_guard = self.stream.lock();
        let Some(stream) = stream_guard.as_mut() else {
            return true;
        };
        match stream.write(&[byte]) {
            Ok(1) => true,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => false,
            // Written nothing without blocking, or failed: the client is gone.
            _ => {
                *stream_guard = None;
                true
            }
        }
    }

    fn recv_byte(&self) -> io::Result<u8> {
        let mut guard = self.stream.lock();

        if guard.is_none() {
            match self.listener.accept() {
                Ok((socket, _)) => {
                    socket.set_nonblocking(true)?;
                    *guard = Some(socket);
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "No connection"));
                }
                Err(e) => return Err(e),
            }
        }

        if let Some(ref mut stream) = *guard {
            let mut buf = [0u8; 1];
            match stream.read(&mut buf) {
                Ok(1) => Ok(buf[0]),
                Ok(_) => { // EOF
                    *guard = None;
                    Err(io::Error::new(io::ErrorKind::NotConnected, "EOF"))
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Err(io::Error::new(io::ErrorKind::WouldBlock, "WouldBlock")),
                Err(e) => {
                    *guard = None;
                    Err(e)
                }
            }
        } else {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "No connection"))
        }
    }
}

/// Several host ends of one serial line (a TCP port and a Unix socket):
/// the guest's output goes to all of them and its input comes from
/// whichever has some.
///
/// A byte one end refuses while another takes it is owed to the refusing
/// end, one byte each: the next byte is refused until every end has taken
/// what it is owed, so the slowest end paces the line and none sees a byte
/// twice.
struct FanoutBackend {
    ends: Vec<Arc<dyn SerialBackend>>,
    owed: Mutex<Vec<Option<u8>>>,
}

impl FanoutBackend {
    fn new(ends: Vec<Arc<dyn SerialBackend>>) -> Self {
        let owed = Mutex::new(vec![None; ends.len()]);
        Self { ends, owed }
    }

    /// Offer each end what it is owed. True when none is owed anything.
    fn settle(&self, owed: &mut [Option<u8>]) -> bool {
        let mut clear = true;
        for (end, o) in self.ends.iter().zip(owed.iter_mut()) {
            if let Some(b) = *o {
                if end.try_send_byte(b) {
                    *o = None;
                } else {
                    clear = false;
                }
            }
        }
        clear
    }
}

impl SerialBackend for FanoutBackend {
    /// An end still owed a byte misses this one, as a full socket drops it.
    fn send_byte(&self, byte: u8) {
        let mut owed = self.owed.lock();
        self.settle(&mut owed);
        for (end, o) in self.ends.iter().zip(owed.iter()) {
            if o.is_none() {
                end.send_byte(byte);
            }
        }
    }
    fn try_send_byte(&self, byte: u8) -> bool {
        let mut owed = self.owed.lock();
        if !self.settle(&mut owed) {
            return false;
        }
        for (end, o) in self.ends.iter().zip(owed.iter_mut()) {
            if !end.try_send_byte(byte) {
                *o = Some(byte);
            }
        }
        true
    }
    fn recv_byte(&self) -> io::Result<u8> {
        // As for TCP: a byte owed to a slow end goes once it has room, even
        // if the guest sends nothing more.
        self.settle(&mut self.owed.lock());
        for b in &self.ends {
            if let Ok(byte) = b.recv_byte() {
                return Ok(byte);
            }
        }
        Err(io::Error::new(io::ErrorKind::WouldBlock, "WouldBlock"))
    }
}

/// What a TCP serial line says on the wire, fixed when the backend is built.
/// The backend is generic over it, so each line's byte loop is compiled for
/// one protocol and carries no test for the other.
pub(crate) trait Wire: Send + Sync + 'static {
    /// Per-client receive state.
    type State: Send;
    fn new_state() -> Self::State;
    /// Bytes to send a client the moment it connects.
    fn greeting() -> &'static [u8];
    /// One outbound data byte as it goes on the wire.
    fn escape(byte: u8, out: &mut Vec<u8>);
    /// One inbound byte: the data byte it carries, if any, with any reply it
    /// calls for appended to `reply`.
    fn feed(state: &mut Self::State, byte: u8, reply: &mut Vec<u8>) -> Option<u8>;
}

/// Telnet character mode: the handshake, 0xFF doubled, commands stripped.
pub(crate) struct Telnet;

impl Wire for Telnet {
    type State = crate::net::telnet::TelnetFilter;
    fn new_state() -> Self::State {
        crate::net::telnet::TelnetFilter::new()
    }
    fn greeting() -> &'static [u8] {
        const HS: [u8; 12] = crate::net::telnet::TelnetFilter::initial_handshake();
        &HS
    }
    fn escape(byte: u8, out: &mut Vec<u8>) {
        crate::net::telnet::escape_byte(byte, out);
    }
    fn feed(state: &mut Self::State, byte: u8, reply: &mut Vec<u8>) -> Option<u8> {
        state.feed(byte, reply)
    }
}

/// Plain bytes both ways: no handshake, no escaping, nothing filtered. For
/// scripts, `nc` and tools that treat the port as a serial line.
pub(crate) struct Raw;

impl Wire for Raw {
    type State = ();
    fn new_state() {}
    fn greeting() -> &'static [u8] {
        &[]
    }
    fn escape(byte: u8, out: &mut Vec<u8>) {
        out.push(byte);
    }
    fn feed(_: &mut (), byte: u8, _: &mut Vec<u8>) -> Option<u8> {
        Some(byte)
    }
}

pub(crate) struct TcpConn<S> {
    stream: TcpStream,
    wire: S,
    /// Bytes this client's socket would not take yet: the rest of one
    /// escaped data byte and any telnet replies, in order. While any wait,
    /// the next data byte is refused (or, sent without retry, skipped).
    pending: Vec<u8>,
    /// Bytes read from the client before it was recognised as telnet; see
    /// [`Carry`]. Only `Auto`'s joining list sets it.
    carry: Carry,
    /// The echo of its first line to drop; see [`EchoHold`]. Only `Auto`'s
    /// joining list sets it.
    hold: Option<EchoHold>,
}

impl<S> TcpConn<S> {
    fn new(stream: TcpStream, wire: S) -> Self {
        Self {
            stream,
            wire,
            pending: Vec::new(),
            carry: Carry::default(),
            hold: None,
        }
    }

    /// Write what is pending as far as the socket takes it. `Ok(true)` when
    /// nothing is left; an error means the client has gone.
    fn flush(&mut self) -> io::Result<bool> {
        while !self.pending.is_empty() {
            match self.stream.write(&self.pending) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.pending.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }

    /// Queue `bytes` behind what is pending and write what the socket takes.
    fn push(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.pending.extend_from_slice(bytes);
        self.flush().map(|_| ())
    }
}

/// The clients of one TCP serial line, as a protocol decides to treat them.
/// Each implementation runs its byte loops on connections whose protocol is
/// already fixed; none tests it per byte.
pub(crate) trait Lines: Default + Send + 'static {
    fn is_empty(&self) -> bool;
    /// Take a freshly accepted client. `false` drops it.
    fn accept(&mut self, stream: TcpStream) -> bool;
    /// Write what sockets held back. Gone clients are dropped.
    fn flush(&mut self);
    /// Give every client one outbound data byte, skipping any whose socket
    /// is full.
    fn send(&mut self, byte: u8);
    /// As `send`, but refused (`false`, nothing sent) while any client still
    /// has bytes its socket would not take, so all clients get every byte
    /// once, paced by the slowest.
    fn try_send(&mut self, byte: u8) -> bool;
    /// Poll the clients for one inbound data byte.
    fn recv(&mut self) -> Option<u8>;
}

/// Clients all speaking one protocol, fixed for the line.
pub(crate) struct Fixed<W: Wire>(Vec<TcpConn<W::State>>);

impl<W: Wire> Default for Fixed<W> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<W: Wire> std::ops::Deref for Fixed<W> {
    type Target = Vec<TcpConn<W::State>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<W: Wire> std::ops::DerefMut for Fixed<W> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

fn flush_all<S>(conns: &mut Vec<TcpConn<S>>) -> bool {
    let mut clear = true;
    conns.retain_mut(|c| match c.flush() {
        Ok(done) => {
            clear &= done;
            true
        }
        Err(_) => false,
    });
    clear
}

fn send_all<S>(conns: &mut Vec<TcpConn<S>>, buf: &[u8]) {
    conns.retain_mut(|c| match c.flush() {
        Ok(true) => c.push(buf).is_ok(),
        Ok(false) => true,
        Err(_) => false,
    });
}

/// Pull bytes from each client through its protocol until a data byte falls
/// out or none has more. A client that has gone is dropped.
fn recv_all<W: Wire>(conns: &mut Vec<TcpConn<W::State>>) -> Option<u8> {
    let mut i = 0;
    while i < conns.len() {
        let mut gone = false;
        loop {
            let c = &mut conns[i];
            let mut buf = [0u8; 1];
            match c.stream.read(&mut buf) {
                Ok(1) => {
                    let mut reply = Vec::new();
                    let data = W::feed(&mut c.wire, buf[0], &mut reply);
                    // Behind any pending output, so a reply never lands
                    // inside an escaped data byte.
                    if !reply.is_empty() && c.push(&reply).is_err() {
                        gone = true;
                        break;
                    }
                    if let Some(d) = data {
                        return Some(d);
                    }
                    // A protocol command consumed; keep reading.
                }
                Ok(_) => {
                    gone = true;
                    break;
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    gone = true;
                    break;
                }
            }
        }
        if gone {
            conns.remove(i);
        } else {
            i += 1;
        }
    }
    None
}

impl<W: Wire> Lines for Fixed<W> {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// The client gets the protocol's greeting. One whose greeting cannot be
    /// written is dropped; it can reconnect.
    fn accept(&mut self, mut stream: TcpStream) -> bool {
        if stream.write_all(W::greeting()).is_err() {
            return false;
        }
        self.0.push(TcpConn::new(stream, W::new_state()));
        true
    }
    fn flush(&mut self) {
        flush_all(&mut self.0);
    }
    fn send(&mut self, byte: u8) {
        let mut buf = Vec::with_capacity(2);
        W::escape(byte, &mut buf);
        send_all(&mut self.0, &buf);
    }
    fn try_send(&mut self, byte: u8) -> bool {
        if !flush_all(&mut self.0) {
            return false;
        }
        let mut buf = Vec::with_capacity(2);
        W::escape(byte, &mut buf);
        self.0.retain_mut(|c| c.push(&buf).is_ok());
        true
    }
    fn recv(&mut self) -> Option<u8> {
        recv_all::<W>(&mut self.0)
    }
}

/// `serial_mode = "auto"`: a client is plain bytes until it shows it speaks
/// telnet, and from then on it is a telnet client. The kinds sit in separate
/// lists, so a client is moved once, when it is recognised, and no list's
/// loops test which protocol a client is on.
///
/// Telnet is recognised in one of two ways, and only before the client has
/// sent one complete line:
/// - an IAC followed by a telnet command (SB, WILL, WONT, DO or DONT), at any
///   point before that. PuTTY and other clients that negotiate on connect do
///   it first. A lone 0xFF a plain client sends as data is not one, nor is
///   the escaped pair 0xFF 0xFF.
/// - the client's first line ending CR LF or CR NUL, when the whole line
///   arrived in one chunk. Telnet clients that stay silent on connect (GNU
///   inetutils, BSD, Apple and busybox telnet, screen) send their first line
///   that way. `nc`, `ncat` and `socat` send a bare LF or CR for Enter.
///
/// A client whose first line ends any other way, or arrives in pieces, is
/// plain for good: from then on nothing it sends changes that. Recognising
/// telnet, the line sends the handshake, which puts such a client in
/// character mode, and from then on escapes 0xFF and filters IAC.
#[derive(Default)]
pub(crate) struct Auto {
    plain: Vec<TcpConn<SniffState>>,
    /// Telnet clients just recognised: what they sent before it was clear
    /// still reaches the guest first, then they move to `telnet`, which so
    /// carries no test for it.
    /// While an echo of their first line is to be dropped their output goes
    /// through [`EchoHold`] too.
    joining: Vec<TcpConn<TelnetFilter>>,
    telnet: Fixed<Telnet>,
}

/// What `Auto` remembers of a plain client between the chunks it reads. It
/// is not a [`Wire`]: a plain client's bytes go through unchanged.
#[derive(Default)]
pub(crate) struct SniffState {
    /// A complete line has been seen with no sign of telnet: plain for good.
    locked: bool,
    /// Data bytes of the first line were read in an earlier chunk.
    consumed: bool,
    after_cr: bool,
    after_iac: bool,
    /// The first line so far (up to [`MAX_HELD_LINE`] bytes): what a
    /// line-mode telnet client has already drawn itself, if this turns out to
    /// be one.
    line: Vec<u8>,
    line_overflow: bool,
    /// Data read from the socket and not yet given to the guest.
    inq: VecDeque<u8>,
    /// An IAC that ended the last chunk is not in `inq` until the next byte
    /// says what it is, or until this long has passed with nothing after it.
    held_iac_since: Option<Instant>,
}

/// What a chunk read from a plain client shows.
enum Scan {
    /// Nothing telnet: all of it is data.
    Plain,
    /// A telnet command starts at this index; the bytes before it are data.
    Iac(usize),
    /// The chunk starts with the command; its IAC ended the last chunk.
    IacHeld,
    /// The client's first line, which began in this chunk, ends CR LF or
    /// CR NUL at this index; the bytes up to and including it are data.
    /// `line` is the line it ends, when there is one worth holding the
    /// guest's echo of.
    NvtEnd { at: usize, line: Option<Vec<u8>> },
}

impl SniffState {
    /// Look through a chunk for the first sign of telnet. Bytes before it are
    /// data; the state carries what a later chunk needs.
    fn scan(&mut self, chunk: &[u8]) -> Scan {
        if self.locked {
            return Scan::Plain;
        }
        let first_line_starts_here = !self.consumed;
        for (i, &byte) in chunk.iter().enumerate() {
            if self.after_iac {
                self.after_iac = false;
                if (crate::net::telnet::SB..=crate::net::telnet::DONT).contains(&byte) {
                    return if i == 0 { Scan::IacHeld } else { Scan::Iac(i - 1) };
                }
                // 0xFF 0xFF is one escaped data byte. After a lone 0xFF this
                // byte is data too, and is looked at as such below.
                if byte == crate::net::telnet::IAC {
                    continue;
                }
            } else if byte == crate::net::telnet::IAC {
                self.consumed = true;
                self.after_iac = true;
                continue;
            }
            let nvt_end = self.after_cr && (byte == b'\n' || byte == 0);
            let bare_cr_ended = self.after_cr && !nvt_end;
            self.after_cr = byte == b'\r';
            if nvt_end {
                if first_line_starts_here {
                    let line = (!self.line_overflow && !self.line.is_empty()).then(|| std::mem::take(&mut self.line));
                    return Scan::NvtEnd { at: i, line };
                }
                // The line was in pieces: not enough to call it telnet.
                self.lock();
                return Scan::Plain;
            }
            if bare_cr_ended || byte == b'\n' {
                self.lock();
                return Scan::Plain;
            }
            self.consumed = true;
            match byte {
                b'\r' => {}
                _ if self.line.len() < MAX_HELD_LINE => self.line.push(byte),
                _ => self.line_overflow = true,
            }
        }
        Scan::Plain
    }

    /// Plain for good, and nothing more of the line is kept.
    fn lock(&mut self) {
        self.locked = true;
        self.line = Vec::new();
    }
}

/// The longest first line `Auto` will hold the guest's echo of.
const MAX_HELD_LINE: usize = 128;

/// How long `Auto` waits for what follows before it acts on a pause: a lone
/// 0xFF at the end of a plain client's read is data after this long with
/// nothing after it.
/// An echo being dropped is given up on after this long without more of it.
const POLL_TIMEOUT: Duration = Duration::from_millis(250);

/// What a client that has just been recognised as telnet has sent that is
/// still to reach the guest: plain data from the chunk that showed it, and
/// the bytes after that, which are telnet.
#[derive(Default)]
pub(crate) struct Carry {
    data: VecDeque<u8>,
    wire: VecDeque<u8>,
}

/// The guest's echo of the line a line-mode telnet client sent just before
/// `Auto` recognised it. That client shows what it types itself, and goes to
/// character mode only when the offer reaches it, so it has already drawn the
/// line; a guest that echoes would draw it again. The line arrived in one
/// chunk and is recognised on reading it, before the guest sees a byte of it,
/// so the guest's echo, byte by byte, finds this in place. While it is held
/// the guest's output is matched against the line byte for byte and the match
/// dropped, then the line break that ends the echo (at most as many CR and LF
/// bytes as the client's line ending had, each of which a tty may echo as a
/// line break). The first byte that does not match releases what was held and
/// everything after it unchanged, as does the timeout, so a guest that does
/// not echo (a password prompt) loses nothing.
struct EchoHold {
    /// The line the client sent, without its ending.
    text: Vec<u8>,
    /// How much of `text` the guest has echoed so far.
    matched: usize,
    /// How many CR and LF bytes of the echo's ending may still be dropped.
    ending: u8,
    since: Instant,
}

enum HoldStep {
    /// The byte was part of the echo: send nothing.
    Dropped,
    /// The hold is over: send these bytes (what was matched if the echo
    /// broke off, then the byte unless it was part of the echo's ending).
    Done(Vec<u8>),
}

impl EchoHold {
    /// The most CR and LF bytes dropped after the echoed text: each of the
    /// two bytes of the client's line ending may be echoed as CR LF.
    const ENDING_BYTES: u8 = 4;

    fn new(text: Vec<u8>) -> Self {
        Self { text, matched: 0, ending: Self::ENDING_BYTES, since: Instant::now() }
    }

    fn step(&mut self, byte: u8) -> HoldStep {
        if self.matched < self.text.len() {
            if byte == self.text[self.matched] {
                self.matched += 1;
                self.since = Instant::now();
                return HoldStep::Dropped;
            }
            let mut out = self.text[..self.matched].to_vec();
            out.push(byte);
            return HoldStep::Done(out);
        }
        // The text is echoed; the line break that ends it goes with it.
        if (byte == b'\r' || byte == b'\n') && self.ending > 0 {
            self.ending -= 1;
            self.since = Instant::now();
            return HoldStep::Dropped;
        }
        HoldStep::Done(vec![byte])
    }
}

impl Auto {
    /// Pass one guest byte through the clients that are joining: one whose
    /// echo hold ends, and that has nothing left to read, moves to the telnet
    /// list.
    fn send_joining(&mut self, byte: u8) {
        let mut i = 0;
        while i < self.joining.len() {
            let c = &mut self.joining[i];
            let mut out = Vec::new();
            match c.hold.as_mut().map(|h| h.step(byte)) {
                Some(HoldStep::Dropped) => {}
                Some(HoldStep::Done(bytes)) => {
                    c.hold = None;
                    for b in bytes {
                        Telnet::escape(b, &mut out);
                    }
                }
                None => Telnet::escape(byte, &mut out),
            }
            if c.flush().is_ok() && (out.is_empty() || c.push(&out).is_ok()) {
                i += 1;
            } else {
                self.joining.remove(i);
            }
        }
        self.settle_joining();
    }

    /// End the echo holds that have waited too long, giving back what they
    /// matched if the echo was cut short.
    fn expire_holds(&mut self) {
        let now = Instant::now();
        let mut i = 0;
        while i < self.joining.len() {
            let c = &mut self.joining[i];
            if c.hold.as_ref().is_some_and(|h| now.duration_since(h.since) >= POLL_TIMEOUT) {
                let mut out = Vec::new();
                if let Some(h) = c.hold.take() {
                    // An echo that was all there stops here and was to be
                    // dropped: only a partial one is given back.
                    if h.matched < h.text.len() {
                        for b in &h.text[..h.matched] {
                            Telnet::escape(*b, &mut out);
                        }
                    }
                }
                if c.push(&out).is_err() {
                    self.joining.remove(i);
                    continue;
                }
            }
            i += 1;
        }
        self.settle_joining();
    }

    /// Move clients that have nothing left to carry over to the telnet list.
    /// An echo still to be dropped keeps a client here too.
    fn settle_joining(&mut self) {
        let mut i = 0;
        while i < self.joining.len() {
            let c = &self.joining[i];
            let done = c.carry.data.is_empty() && c.carry.wire.is_empty() && c.hold.is_none();
            if done {
                let c = self.joining.remove(i);
                self.telnet.0.push(c);
            } else {
                i += 1;
            }
        }
    }

    /// The next data byte from clients just recognised as telnet: what was
    /// read with the chunk that showed it, then what they send.
    fn joining_recv(&mut self) -> Option<u8> {
        let mut i = 0;
        while i < self.joining.len() {
            let c = &mut self.joining[i];
            if let Some(d) = c.carry.data.pop_front() {
                return Some(d);
            }
            while let Some(b) = c.carry.wire.pop_front() {
                let mut reply = Vec::new();
                let data = Telnet::feed(&mut c.wire, b, &mut reply);
                if !reply.is_empty() && c.push(&reply).is_err() {
                    break;
                }
                if let Some(d) = data {
                    return Some(d);
                }
            }
            i += 1;
        }
        if self.joining.is_empty() {
            return None;
        }
        let got = recv_all::<Telnet>(&mut self.joining);
        self.settle_joining();
        got
    }

    /// Read a chunk from the plain client at `i` and look it over.
    fn plain_recv(&mut self, i: usize) -> PlainStep {
        let c = &mut self.plain[i];
        if let Some(d) = c.wire.inq.pop_front() {
            return PlainStep::Data(d);
        }
        let mut buf = [0u8; 256];
        let n = match c.stream.read(&mut buf) {
            Ok(0) => {
                self.plain.remove(i);
                return PlainStep::Gone;
            }
            Ok(n) => n,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                // A lone IAC that nothing followed is data.
                if c.wire.held_iac_since.is_some_and(|t| t.elapsed() >= POLL_TIMEOUT) {
                    c.wire.held_iac_since = None;
                    c.wire.after_iac = false;
                    return PlainStep::Data(crate::net::telnet::IAC);
                }
                return PlainStep::Idle;
            }
            Err(_) => {
                self.plain.remove(i);
                return PlainStep::Gone;
            }
        };
        let chunk = &buf[..n];
        let held = c.wire.held_iac_since.take().is_some();
        let mut iac_was_held = false;
        let mut hold_text = None;
        let (mut cut, switched) = match c.wire.scan(chunk) {
            Scan::Plain => (n, false),
            Scan::Iac(at) => (at, true),
            Scan::IacHeld => {
                iac_was_held = true;
                (0, true)
            }
            Scan::NvtEnd { at, line } => {
                hold_text = line;
                (at + 1, true)
            }
        };
        if held && !iac_was_held {
            // The IAC that ended the last chunk was data.
            c.wire.inq.push_back(crate::net::telnet::IAC);
        }
        if !switched && c.wire.after_iac && !c.wire.locked {
            // An IAC ending this chunk waits for what follows.
            cut = n - 1;
            c.wire.held_iac_since = Some(Instant::now());
        }
        c.wire.inq.extend(&chunk[..cut]);
        if !switched {
            return match c.wire.inq.pop_front() {
                Some(d) => PlainStep::Data(d),
                None => PlainStep::Idle,
            };
        }
        // Telnet. Move the client over with what it has sent: the plain data
        // up to the sign, and the rest as telnet.
        let TcpConn { stream, mut pending, wire: sniff, .. } = self.plain.remove(i);
        pending.extend_from_slice(Telnet::greeting());
        let mut wire: VecDeque<u8> = VecDeque::new();
        if iac_was_held {
            wire.push_back(crate::net::telnet::IAC);
        }
        wire.extend(&chunk[cut..]);
        let mut conn = TcpConn::new(stream, Telnet::new_state());
        conn.pending = pending;
        conn.carry = Carry { data: sniff.inq, wire };
        // A line-mode client has drawn its first line; hold the guest's echo
        // of it.
        if let Some(text) = hold_text {
            conn.hold = Some(EchoHold::new(text));
        }
        if conn.flush().is_err() {
            return PlainStep::Gone;
        }
        self.joining.push(conn);
        PlainStep::Gone
    }
}

enum PlainStep {
    Data(u8),
    /// The client was moved or dropped.
    Gone,
    /// Nothing to read from this client now.
    Idle,
}

impl Lines for Auto {
    fn is_empty(&self) -> bool {
        self.plain.is_empty() && self.joining.is_empty() && self.telnet.is_empty()
    }
    fn accept(&mut self, stream: TcpStream) -> bool {
        self.plain.push(TcpConn::new(stream, SniffState::default()));
        true
    }
    fn flush(&mut self) {
        flush_all(&mut self.plain);
        flush_all(&mut self.joining);
        flush_all(&mut self.telnet.0);
        if !self.joining.is_empty() {
            self.expire_holds();
        }
    }
    fn send(&mut self, byte: u8) {
        send_all(&mut self.plain, &[byte]);
        // Before the joining clients, so one that moves over on this byte is
        // not sent it twice.
        self.telnet.send(byte);
        if !self.joining.is_empty() {
            self.send_joining(byte);
        }
    }
    fn try_send(&mut self, byte: u8) -> bool {
        // Every list must be clear before any takes the byte.
        let clear = flush_all(&mut self.plain) & flush_all(&mut self.joining) & flush_all(&mut self.telnet.0);
        if !clear {
            return false;
        }
        self.plain.retain_mut(|c| c.push(&[byte]).is_ok());
        let taken = self.telnet.try_send(byte);
        if !self.joining.is_empty() {
            self.send_joining(byte);
        }
        taken
    }
    fn recv(&mut self) -> Option<u8> {
        if !self.joining.is_empty() {
            if let Some(d) = self.joining_recv() {
                return Some(d);
            }
        }
        if let Some(d) = self.telnet.recv() {
            return Some(d);
        }
        let mut i = 0;
        while i < self.plain.len() {
            match self.plain_recv(i) {
                PlainStep::Data(d) => return Some(d),
                PlainStep::Idle => i += 1,
                // A client that moved over has data waiting.
                PlainStep::Gone => {
                    if let Some(d) = self.joining_recv() {
                        return Some(d);
                    }
                }
            }
        }
        None
    }
}

pub(crate) struct TcpSocketBackend<L: Lines = Fixed<Telnet>> {
    pub(crate) listener: TcpListener,
    /// Every connected client: each sees all the output and may type, so a
    /// person at the console and a script can share it.
    conns: Mutex<L>,
}

impl TcpSocketBackend<Fixed<Telnet>> {
    pub(crate) fn new<A: std::net::ToSocketAddrs>(addr: A) -> Option<Self> {
        Self::bind(addr)
    }
}

impl<L: Lines> TcpSocketBackend<L> {
    /// Bind a TCP serial backend. Returns `None` (rather than panicking) if the
    /// port can't be bound — most commonly because a stale emulator process from
    /// an earlier crash is still holding it. Callers fall back to a NullBackend
    /// so the machine still boots; that serial channel is simply unavailable
    /// until the port frees.
    pub(crate) fn bind<A: std::net::ToSocketAddrs>(addr: A) -> Option<Self> {
        let listener = match TcpListener::bind(addr) {
            Ok(l) => l,
            Err(e) => {
                log::warn!("serial TCP backend disabled: failed to bind socket: {e}");
                return None;
            }
        };
        if let Err(e) = listener.set_nonblocking(true) {
            log::warn!("serial TCP backend disabled: failed to set nonblocking: {e}");
            return None;
        }
        Some(Self { listener, conns: Mutex::new(L::default()) })
    }

    /// Take any clients waiting to connect.
    fn accept_waiting(&self, conns: &mut L) {
        while let Ok((socket, _)) = self.listener.accept() {
            if socket.set_nonblocking(true).is_err() {
                continue;
            }
            // The console is latency-sensitive: without TCP_NODELAY, Nagle
            // holds a keystroke or a log line back until the previous
            // segment is acked, which shows up as a stutter on every
            // interaction. Set it on accept, before any byte is written.
            let _ = socket.set_nodelay(true);
            conns.accept(socket);
        }
    }
}

impl<L: Lines> SerialBackend for TcpSocketBackend<L> {
    /// A client whose socket is still full misses this byte.
    fn send_byte(&self, byte: u8) {
        let mut conns = self.conns.lock();
        if conns.is_empty() {
            return;
        }
        conns.send(byte);
    }

    /// Refused while any client still has bytes its socket would not take,
    /// so every client gets every byte once, paced by the slowest. A byte
    /// taken goes to each client whole: what a socket takes only in part
    /// waits in that client's pending bytes. A client that has gone is
    /// dropped; with none the byte is taken and dropped.
    fn try_send_byte(&self, byte: u8) -> bool {
        self.conns.lock().try_send(byte)
    }

    fn recv_byte(&self) -> io::Result<u8> {
        let mut conns = self.conns.lock();
        self.accept_waiting(&mut conns);
        // The receive poll also moves output a full socket held back, so the
        // last byte before the guest falls quiet is not left waiting.
        conns.flush();
        if conns.is_empty() {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "No connection"));
        }
        match conns.recv() {
            Some(d) => Ok(d),
            None => Err(io::Error::new(io::ErrorKind::WouldBlock, "WouldBlock")),
        }
    }
}

/// A TCP serial console on 127.0.0.1:`port` (0 takes any free port), for any
/// device with a serial line: every client connected sees the output and may
/// type. `mode` says how it talks to a client (see [`SerialMode`]). Returns
/// the backend and the port it bound, or `None` when the port is taken.
pub fn tcp_serial_backend(port: u16, mode: SerialMode) -> Option<(Arc<dyn SerialBackend>, u16)> {
    // Chosen here, once: each backend is its own type with its own byte loops.
    fn serve<L: Lines>(port: u16) -> Option<(Arc<dyn SerialBackend>, u16)> {
        let b = TcpSocketBackend::<L>::bind(("127.0.0.1", port))?;
        let bound = b.listener.local_addr().ok()?.port();
        Some((Arc::new(b), bound))
    }
    match mode {
        SerialMode::Auto => serve::<Auto>(port),
        SerialMode::Raw => serve::<Fixed<Raw>>(port),
        SerialMode::Telnet => serve::<Fixed<Telnet>>(port),
    }
}

/// A serial line wired to nothing: output is dropped, nothing arrives.
pub fn null_serial_backend() -> Arc<dyn SerialBackend> {
    Arc::new(NullBackend)
}

// ============================================================================
// CiSerialBackend — in-process serial backend used by --ci mode.
// ============================================================================

/// Serial backend that the CI control socket reads from and writes to. The
/// guest sees this as channel A (the IRIX console). Host pushes bytes into
/// `host_to_guest` via `push_host`; the existing RX thread drains them into
/// `channel_a.rx_queue`. Guest output reaches `send_byte`, which pushes into
/// `guest_to_host` and wakes anyone waiting in `wait_for`.
pub struct CiSerialBackend {
    host_to_guest: Mutex<VecDeque<u8>>,
    guest_to_host: Mutex<Vec<u8>>,
    /// Counts the times the buffer was emptied (a drain, a reset or a wait
    /// taking its match), changed under the buffer's lock. A `wait_for` that
    /// sees it change searches from the start again, as what it had searched
    /// is gone.
    drains: AtomicU64,
    cv: Condvar,
    /// Optional file mirror: every byte that flows through ttyd1 in either
    /// direction is appended here. Host-injected bytes are echoed by IRIX
    /// anyway, so writing only guest output already captures the full
    /// transcript without duplication.
    log: Mutex<Option<Arc<SerialLog>>>,
}

impl CiSerialBackend {
    pub fn new() -> Self {
        Self {
            host_to_guest: Mutex::new(VecDeque::new()),
            guest_to_host: Mutex::new(Vec::new()),
            drains: AtomicU64::new(0),
            cv: Condvar::new(),
            log: Mutex::new(None),
        }
    }

    /// Attach the shared serial-log sink. The caller opens the file (and
    /// picks the timestamp mode), so the CI and TCP paths write the same bytes.
    pub fn set_log(&self, log: Arc<SerialLog>) {
        *self.log.lock() = Some(log);
    }

    /// Inject bytes from host to guest (the harness typing on the console).
    pub fn push_host(&self, data: &[u8]) {
        let mut q = self.host_to_guest.lock();
        q.extend(data.iter().copied());
    }

    /// Drain everything the guest has produced since the last call. Empties
    /// the buffer; the returned Vec is the guest output as raw bytes.
    pub fn drain_guest(&self) -> Vec<u8> {
        let mut q = self.guest_to_host.lock();
        self.drains.fetch_add(1, Ordering::Relaxed);
        std::mem::take(&mut *q)
    }

    /// Block until `needle` is seen in guest output, or `timeout` expires.
    /// On success returns the consumed bytes up to and including the match;
    /// bytes that arrived after the match stay in the buffer for the next
    /// `serial-read`. On timeout returns `None` without consuming anything.
    pub fn wait_for(&self, needle: &[u8], timeout: Duration) -> Option<Vec<u8>> {
        if needle.is_empty() {
            return Some(Vec::new());
        }
        let deadline = Instant::now() + timeout;
        let mut q = self.guest_to_host.lock();
        // How much of the buffer earlier passes searched, so a pass after a
        // wakeup looks at the new bytes only. The guest's transmit thread
        // waits on the lock this holds. A match can straddle the old end,
        // hence the needle's length less one is searched again.
        let mut scanned = 0usize;
        let mut seen = self.drains.load(Ordering::Relaxed);
        let mut last = false;
        loop {
            // Another reader emptied the buffer under us, and it may have
            // filled again past `scanned`: start over.
            let drains = self.drains.load(Ordering::Relaxed);
            if drains != seen {
                (scanned, seen) = (0, drains);
            }
            let from = scanned.saturating_sub(needle.len() - 1);
            if let Some(pos) = find_subseq(&q[from..], needle) {
                let end = from + pos + needle.len();
                let consumed: Vec<u8> = q.drain(..end).collect();
                self.drains.fetch_add(1, Ordering::Relaxed);
                return Some(consumed);
            }
            // One more pass after the wait times out, in case bytes arrived
            // between the last pass and the timeout.
            if last || Instant::now() >= deadline {
                return None;
            }
            scanned = q.len();
            last = self.cv.wait_until(&mut q, deadline).timed_out();
        }
    }

    /// Clear both queues. Called on `restore`/`rollback` so stale serial
    /// output from the previous run doesn't leak into the next test.
    pub fn reset(&self) {
        self.host_to_guest.lock().clear();
        let mut q = self.guest_to_host.lock();
        self.drains.fetch_add(1, Ordering::Relaxed);
        q.clear();
    }
}

fn find_subseq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

impl SerialBackend for CiSerialBackend {
    fn send_byte(&self, byte: u8) {
        self.guest_to_host.lock().push(byte);
        if let Some(log) = self.log.lock().as_ref() {
            log.write(&[byte]);
        }
        self.cv.notify_all();
    }

    fn recv_byte(&self) -> io::Result<u8> {
        let mut q = self.host_to_guest.lock();
        match q.pop_front() {
            Some(b) => Ok(b),
            None => Err(io::Error::new(io::ErrorKind::WouldBlock, "empty")),
        }
    }
}

/// Open `cfg.serial_log`, if one is set, as the sink the machine's CI and
/// TCP consoles both write through, so the two logs are the same bytes
/// (timestamps included). A file that cannot be opened is reported and the
/// machine boots without a log. `tag` names the machine in the messages. An
/// existing file is added to, or started over with `serial_log_overwrite`.
pub fn open_log(cfg: &MachineConfig, tag: &str) -> Option<Arc<SerialLog>> {
    open_log_at(cfg, cfg.serial_log.as_deref(), tag)
}

/// As [`open_log`] for the transcript at `path` (a line's own log, when it
/// is not the console's), with `cfg`'s timestamp setting.
pub fn open_log_at(cfg: &MachineConfig, path: Option<&str>, tag: &str) -> Option<Arc<SerialLog>> {
    let path = path?;
    let p = Path::new(path);
    let opened = if path == "-" {
        Ok(SerialLog::stdout(cfg.serial_log_timestamps))
    } else {
        if cfg.serial_log_overwrite {
            SerialLog::create(p, cfg.serial_log_timestamps)
        } else {
            SerialLog::open(p, cfg.serial_log_timestamps)
        }
    };
    match opened {
        Ok(log) => {
            eprintln!("iris: {tag} serial console mirroring to {path}");
            Some(log)
        }
        Err(e) => {
            eprintln!("iris: {tag} serial log {path}: {e}");
            None
        }
    }
}

/// A serial console on a Unix socket at `path`; see [`UnixSocketBackend`].
/// Returns `None` when the socket cannot be made, with the reason on stderr,
/// as a TCP port that is taken does.
#[cfg(unix)]
pub fn unix_serial_backend(path: &str, label: &str, setting: &str) -> Option<Arc<dyn SerialBackend>> {
    match UnixSocketBackend::new(path) {
        Ok(b) => {
            eprintln!("iris: {label} on socket {path}");
            Some(Arc::new(b))
        }
        Err(e) => {
            eprintln!("iris: {label}: socket {path}: {e} ({setting})");
            None
        }
    }
}

#[cfg(not(unix))]
pub fn unix_serial_backend(path: &str, label: &str, setting: &str) -> Option<Arc<dyn SerialBackend>> {
    eprintln!("iris: {label}: socket {path}: Unix sockets need a Unix host ({setting})");
    None
}

/// Where one serial line is served on the host.
pub struct SerialLine<'a> {
    /// Names the line in messages, e.g. "Octane serial port 1".
    pub label: &'a str,
    /// The TCP port, or `None` for a line with no listener.
    pub tcp_port: Option<u16>,
    /// The setting that chose `tcp_port`, for the message when it is taken.
    pub tcp_setting: &'a str,
    /// A Unix socket to serve the line on as well, from the `socket_setting`
    /// key.
    pub socket: Option<&'a str>,
    pub socket_setting: &'a str,
    /// How the TCP port talks to a client (`serial_mode`).
    pub mode: SerialMode,
}

/// Wire one serial line: TCP on `tcp_port` plus the Unix socket when one is
/// set, teed to `log`. The line that carries the CI control socket's console
/// (`ci`) is not served on TCP (so several `--ci` instances coexist); with a
/// socket, its output goes to both the control socket's buffer and the
/// socket, and either may type. Returns the backend, and whether a host end
/// could be made; without one the backend only logs.
pub fn serial_line(
    line: &SerialLine,
    log: Option<Arc<SerialLog>>,
    ci: Option<&Arc<CiSerialBackend>>,
) -> (Arc<dyn SerialBackend>, bool) {
    let socket = line.socket.and_then(|path| unix_serial_backend(path, line.label, line.socket_setting));
    if let Some(ci) = ci {
        if let Some(log) = log {
            ci.set_log(log);
        }
        return match socket {
            Some(socket) => (Arc::new(FanoutBackend::new(vec![ci.clone(), socket])), true),
            None => (ci.clone(), true),
        };
    }
    let mut ends: Vec<Arc<dyn SerialBackend>> = Vec::new();
    if let Some(port) = line.tcp_port {
        match tcp_serial_backend(port, line.mode) {
            Some((b, p)) => {
                eprintln!("iris: {} on 127.0.0.1:{p}", line.label);
                ends.push(b);
            }
            None => eprintln!(
                "iris: {}: port {port} unavailable; is another iris using it? ({})",
                line.label, line.tcp_setting
            ),
        }
    }
    ends.extend(socket);
    let served = !ends.is_empty();
    let backend: Arc<dyn SerialBackend> = match ends.len() {
        0 => null_serial_backend(),
        1 => ends.remove(0),
        _ => Arc::new(FanoutBackend::new(ends)),
    };
    match log {
        Some(log) => (Arc::new(TeeBackend::new(backend, log)), served),
        None => (backend, served),
    }
}

/// The host ends of a machine's two serial ports: `(console, second)`.
///
/// The console is the CI backend in CI mode, otherwise TCP on
/// `serial_port_a` (nothing, if the port is taken), either teed to
/// `serial_log`. The second port serves TCP on `serial_port_b`, teed to
/// `serial_log_b`, or is `None` when nothing could serve it, so the caller
/// leaves it unwired. In CI mode neither port is served on TCP, so several
/// `--ci` instances coexist. Each also takes a Unix socket, `serial_socket_a` and
/// `serial_socket_b`.
pub fn console_backends(
    cfg: &MachineConfig,
    tag: &str,
    ci: Option<&Arc<CiSerialBackend>>,
) -> (Arc<dyn SerialBackend>, Option<Arc<dyn SerialBackend>>) {
    let [port_a, port_b] = cfg.serial_ports();
    let log = open_log(cfg, tag);
    let label = format!("{tag} serial port 1");
    let (console, _) = serial_line(
        &SerialLine {
            label: &label,
            tcp_port: Some(port_a),
            tcp_setting: "serial_port_a",
            socket: cfg.serial_socket_a.as_deref(),
            socket_setting: "serial_socket_a",
            mode: cfg.serial_mode,
        },
        log,
        ci,
    );
    let label = format!("{tag} serial port 2");
    let log = open_log_at(cfg, cfg.serial_log_b.as_deref(), &label);
    let (second, served) = serial_line(
        &SerialLine {
            label: &label,
            tcp_port: ci.is_none().then_some(port_b),
            tcp_setting: "serial_port_b",
            socket: cfg.serial_socket_b.as_deref(),
            socket_setting: "serial_socket_b",
            mode: cfg.serial_mode,
        },
        log,
        None,
    );
    (console, served.then_some(second))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The console is the CI backend in CI mode and TCP otherwise, either
    /// teed to the same log, and the second port comes up on its own outside
    /// CI mode only. The log adds to what is there, or starts over with
    /// `serial_log_overwrite`.
    #[test]
    fn the_console_is_ci_or_tcp_and_both_log_the_same_bytes() {
        let dir = std::env::temp_dir().join(format!("iris-serial-host-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("serial.log");
        std::fs::write(&log, "stale").unwrap();
        let mut cfg = MachineConfig::default();
        cfg.serial_log = Some(log.to_string_lossy().into_owned());
        cfg.serial_port_a = Some(0);
        cfg.serial_port_b = Some(0);

        let ci = Arc::new(CiSerialBackend::new());
        let (console, second) = console_backends(&cfg, "Test", Some(&ci));
        console.send_byte(b'c');
        assert_eq!(ci.drain_guest(), b"c");
        assert_eq!(std::fs::read(&log).unwrap(), b"stalec", "added to what was there");
        drop(console);
        cfg.serial_log_overwrite = true;
        let (console, _) = console_backends(&cfg, "Test", Some(&ci));
        console.send_byte(b'c');
        assert_eq!(std::fs::read(&log).unwrap(), b"c", "started over");
        assert!(second.is_none(), "CI mode serves no TCP port, so the second port stays unwired");

        drop(console);
        let (console, second) = console_backends(&cfg, "Test", None);
        assert!(second.is_some());
        console.send_byte(b't');
        assert_eq!(std::fs::read(&log).unwrap(), b"t");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A TCP line carries the guest's output to a client and into the log;
    /// a line with no listener only logs.
    #[test]
    fn a_tcp_line_is_teed_to_its_log_and_a_line_without_ends_only_logs() {
        let dir = std::env::temp_dir().join(format!("iris-serial-host-line-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("tcp.log");
        let log = SerialLog::create(&log_path, Default::default()).unwrap();
        let (backend, served) = serial_line(
            &SerialLine { label: "Test", tcp_port: Some(0), tcp_setting: "port", socket: None, socket_setting: "socket", mode: SerialMode::Auto },
            Some(log),
            None,
        );
        assert!(served);
        backend.send_byte(b't');
        assert_eq!(std::fs::read(&log_path).unwrap(), b"t");

        let log_path = dir.join("none.log");
        let log = SerialLog::create(&log_path, Default::default()).unwrap();
        let (backend, served) = serial_line(
            &SerialLine { label: "Test", tcp_port: None, tcp_setting: "port", socket: None, socket_setting: "socket", mode: SerialMode::Auto },
            Some(log),
            None,
        );
        assert!(!served);
        backend.send_byte(b'n');
        assert_eq!(std::fs::read(&log_path).unwrap(), b"n");
        assert!(backend.recv_byte().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A needle that arrives a byte at a time, so no wakeup sees all of it,
    /// is still found, and only up to its end is consumed.
    #[test]
    fn wait_for_finds_a_match_split_across_wakeups() {
        let ci = Arc::new(CiSerialBackend::new());
        let waiter = {
            let ci = ci.clone();
            std::thread::spawn(move || ci.wait_for(b"login: ", Duration::from_secs(10)))
        };
        for &b in b"boot\r\nlogin: more" {
            ci.send_byte(b);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(waiter.join().unwrap().as_deref(), Some(&b"boot\r\nlogin: "[..]));
        assert_eq!(ci.drain_guest(), b"more");
    }

    /// A drain and a refill that land together, past the point a parked
    /// waiter last searched, must not hide the needle at the start of the new
    /// output. The refill is made without a wake-up and one follows it, so the
    /// waiter first looks at it after both.
    #[test]
    fn wait_for_sees_output_that_refilled_a_drained_buffer() {
        let ci = Arc::new(CiSerialBackend::new());
        ci.guest_to_host.lock().extend_from_slice(b"0123456789");
        let waiter = {
            let ci = ci.clone();
            std::thread::spawn(move || ci.wait_for(b"NEEDLE", Duration::from_secs(10)))
        };
        // notify_all reports how many threads it woke, so a non-zero count
        // proves the waiter has searched the ten bytes and parked. Nothing
        // changed, so it searches again and parks again with the same offset.
        let deadline = Instant::now() + Duration::from_secs(10);
        while ci.cv.notify_all() == 0 {
            assert!(Instant::now() < deadline, "waiter never parked");
            std::thread::yield_now();
        }
        assert_eq!(ci.drain_guest(), b"0123456789");
        ci.guest_to_host.lock().extend_from_slice(b"NEEDLE and more bytes");
        ci.cv.notify_all();
        assert_eq!(waiter.join().unwrap().as_deref(), Some(&b"NEEDLE"[..]));
    }

    #[cfg(unix)]
    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("iris-serial-host-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Read from a socket until `want` has arrived, a second at most.
    #[cfg(unix)]
    fn read_until(stream: &mut UnixStream, want: &[u8]) -> Vec<u8> {
        stream.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !got.windows(want.len()).any(|w| w == want) && Instant::now() < deadline {
            let mut buf = [0u8; 64];
            if let Ok(n) = stream.read(&mut buf) {
                got.extend_from_slice(&buf[..n]);
            }
        }
        got
    }

    /// A console on a Unix socket sends the guest's bytes to the client and
    /// takes the client's, alongside its TCP port, and both go to the log.
    #[cfg(unix)]
    #[test]
    fn a_console_is_served_on_a_unix_socket_beside_its_tcp_port() {
        let dir = temp_dir("socket");
        let sock = dir.join("console.sock");
        let log_path = dir.join("console.log");
        let log = SerialLog::create(&log_path, Default::default()).unwrap();
        let path = sock.to_string_lossy().into_owned();
        let (backend, served) = serial_line(
            &SerialLine { label: "Test", tcp_port: Some(0), tcp_setting: "port", socket: Some(&path), socket_setting: "socket", mode: SerialMode::Auto },
            Some(log),
            None,
        );
        assert!(served);

        let mut client = UnixStream::connect(&sock).unwrap();
        // The socket accepts on the guest's receive poll.
        let _ = backend.recv_byte();
        client.write_all(b"x").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let typed = loop {
            match backend.recv_byte() {
                Ok(b) => break b,
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => panic!("no input from the socket: {e}"),
            }
        };
        assert_eq!(typed, b'x');

        for b in b"login: " {
            backend.send_byte(*b);
        }
        assert_eq!(read_until(&mut client, b"login: "), b"login: ");
        assert_eq!(std::fs::read(&log_path).unwrap(), b"login: ");

        drop(backend);
        assert!(!sock.exists(), "the socket file goes with the backend");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A socket file an earlier run left is replaced; a live one, and a file
    /// that is not a socket, are refused and left alone.
    #[cfg(unix)]
    #[test]
    fn a_unix_serial_socket_replaces_a_stale_one_and_spares_other_files() {
        let dir = temp_dir("stale");
        let sock = dir.join("s.sock");
        let stale = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        drop(stale);
        assert!(sock.exists(), "a dead listener leaves its file");
        let live = UnixSocketBackend::new(&sock).expect("a stale socket is replaced");
        assert_eq!(UnixSocketBackend::new(&sock).err().map(|e| e.kind()), Some(io::ErrorKind::AddrInUse));
        drop(live);

        let plain = dir.join("plain");
        std::fs::write(&plain, "keep").unwrap();
        assert_eq!(UnixSocketBackend::new(&plain).err().map(|e| e.kind()), Some(io::ErrorKind::AlreadyExists));
        assert_eq!(std::fs::read(&plain).unwrap(), b"keep");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With the control socket and a console socket both in play, the guest's
    /// output reaches the socket, the log and the control socket's buffer,
    /// and typing works from either.
    #[cfg(unix)]
    #[test]
    fn a_ci_console_on_a_socket_still_feeds_the_control_buffer() {
        let dir = temp_dir("ci");
        let sock = dir.join("console.sock");
        let log_path = dir.join("console.log");
        let mut cfg = MachineConfig::default();
        cfg.serial_log = Some(log_path.to_string_lossy().into_owned());
        cfg.serial_port_a = Some(0);
        cfg.serial_port_b = Some(0);
        cfg.serial_socket_a = Some(sock.to_string_lossy().into_owned());

        let ci = Arc::new(CiSerialBackend::new());
        let (console, _) = console_backends(&cfg, "Test", Some(&ci));
        let mut client = UnixStream::connect(&sock).unwrap();
        let _ = console.recv_byte();
        console.send_byte(b'k');
        assert_eq!(read_until(&mut client, b"k"), b"k");
        assert_eq!(std::fs::read(&log_path).unwrap(), b"k");
        assert_eq!(ci.drain_guest(), b"k");

        ci.push_host(b"c");
        assert_eq!(console.recv_byte().unwrap(), b'c');
        client.write_all(b"s").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let typed = loop {
            match console.recv_byte() {
                Ok(b) => break b,
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => panic!("no input from the socket: {e}"),
            }
        };
        assert_eq!(typed, b's');
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The second port has its own log and socket; with neither TCP nor a
    /// socket to serve it, it is left unwired.
    #[cfg(unix)]
    #[test]
    fn the_second_port_has_its_own_socket_and_log() {
        let dir = temp_dir("second");
        let sock = dir.join("b.sock");
        let log_path = dir.join("b.log");
        let busy = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mut cfg = MachineConfig::default();
        cfg.serial_port_a = Some(0);
        cfg.serial_port_b = Some(busy.local_addr().unwrap().port());
        let (_, second) = console_backends(&cfg, "Test", None);
        assert!(second.is_none(), "a taken port with no socket leaves the port unwired");

        cfg.serial_socket_b = Some(sock.to_string_lossy().into_owned());
        cfg.serial_log_b = Some(log_path.to_string_lossy().into_owned());
        let (_, second) = console_backends(&cfg, "Test", None);
        let second = second.expect("the socket serves the port");
        let mut client = UnixStream::connect(&sock).unwrap();
        let _ = second.recv_byte();
        second.send_byte(b'b');
        assert_eq!(read_until(&mut client, b"b"), b"b");
        assert_eq!(std::fs::read(&log_path).unwrap(), b"b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A host end that takes bytes only while open; its plain send drops a
    /// byte while shut, as a full socket's does.
    #[derive(Default)]
    struct Gate {
        open: std::sync::atomic::AtomicBool,
        out: Mutex<Vec<u8>>,
    }

    impl Gate {
        fn set(&self, open: bool) {
            self.open.store(open, Ordering::SeqCst);
        }
    }

    impl SerialBackend for Gate {
        fn send_byte(&self, byte: u8) {
            let _ = self.try_send_byte(byte);
        }
        fn try_send_byte(&self, byte: u8) -> bool {
            let open = self.open.load(Ordering::SeqCst);
            if open {
                self.out.lock().push(byte);
            }
            open
        }
        fn recv_byte(&self) -> io::Result<u8> {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "empty"))
        }
    }

    /// A TCP client that stops reading fills its socket and the backend then
    /// refuses bytes; once it reads, it gets every byte the backend took
    /// exactly once and in order, telnet escapes whole, and the refused byte
    /// goes. The receive poll moves the last held-back byte without another
    /// send.
    #[test]
    fn a_tcp_client_that_stops_reading_holds_bytes_back_and_gets_each_once() {
        use socket2::{Domain, SockRef, Socket, Type};
        let b = TcpSocketBackend::new(("127.0.0.1", 0)).unwrap();
        let addr = b.listener.local_addr().unwrap();
        let client = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        client.set_recv_buffer_size(4096).unwrap();
        client.connect(&addr.into()).unwrap();
        let mut client: TcpStream = client.into();
        let t0 = Instant::now();
        while b.conns.lock().is_empty() {
            assert!(t0.elapsed() < Duration::from_secs(10), "never accepted");
            let _ = b.recv_byte();
            std::thread::sleep(Duration::from_millis(1));
        }
        SockRef::from(&b.conns.lock()[0].stream).set_send_buffer_size(4096).unwrap();

        // Every fourth byte is the telnet escape, which goes out doubled.
        let byte = |i: usize| if i % 4 == 3 { 0xff } else { i as u8 };
        let mut taken = 0;
        while b.try_send_byte(byte(taken)) {
            taken += 1;
            assert!(taken < 16 << 20, "a client that never reads must fill up");
        }
        assert!(!b.try_send_byte(byte(taken)), "still full");

        let mut want = crate::net::telnet::TelnetFilter::initial_handshake().to_vec();
        for i in 0..=taken {
            crate::net::telnet::escape_byte(byte(i), &mut want);
        }
        client.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        let mut got = Vec::new();
        let mut last_taken = false;
        while got.len() < want.len() {
            assert!(t0.elapsed() < Duration::from_secs(20), "only {} of {} bytes arrived", got.len(), want.len());
            let mut buf = [0u8; 4096];
            match client.read(&mut buf) {
                Ok(0) => panic!("the backend hung up"),
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) => assert!(matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut), "{e}"),
            }
            if last_taken {
                let _ = b.recv_byte();
            } else {
                last_taken = b.try_send_byte(byte(taken));
            }
        }
        assert!(last_taken);
        assert!(got == want, "the client saw {} bytes, wanted {}, or not in order", got.len(), want.len());
    }

    /// A scripted client on a serial line: it connects, sends what it is
    /// told, and the line is polled as the guest's SCC would poll it.
    struct Client {
        line: Arc<dyn SerialBackend>,
        stream: TcpStream,
        got: Vec<u8>,
    }

    impl Client {
        fn connect(mode: SerialMode) -> Self {
            let (line, port) = tcp_serial_backend(0, mode).expect("bind");
            let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream.set_nonblocking(true).unwrap();
            let mut c = Client { line, stream, got: Vec::new() };
            c.settle();
            c
        }

        /// Poll the line for the guest's input and read what reached the
        /// client, until both have been quiet for a few polls.
        fn settle(&mut self) -> Vec<u8> {
            let mut input = Vec::new();
            let mut quiet = 0;
            let t0 = Instant::now();
            while quiet < 20 {
                assert!(t0.elapsed() < Duration::from_secs(10));
                let mut moved = false;
                while let Ok(b) = self.line.recv_byte() {
                    input.push(b);
                    moved = true;
                }
                let mut buf = [0u8; 256];
                if let Ok(n) = self.stream.read(&mut buf) {
                    self.got.extend_from_slice(&buf[..n]);
                    moved = moved || n > 0;
                }
                quiet = if moved { 0 } else { quiet + 1 };
                std::thread::sleep(Duration::from_millis(2));
            }
            input
        }

        /// The client sends `bytes`; returns what the guest read.
        fn send(&mut self, bytes: &[u8]) -> Vec<u8> {
            use std::io::Write;
            self.stream.set_nonblocking(false).unwrap();
            self.stream.write_all(bytes).unwrap();
            self.stream.set_nonblocking(true).unwrap();
            self.settle()
        }

        /// As `send`, but the guest echoes each byte as it reads it, the way a
        /// tty does, answering CR with `cr` and LF with `lf`.
        fn send_echoing(&mut self, bytes: &[u8], cr: &[u8], lf: &[u8]) -> Vec<u8> {
            use std::io::Write;
            self.stream.set_nonblocking(false).unwrap();
            self.stream.write_all(bytes).unwrap();
            self.stream.set_nonblocking(true).unwrap();
            let mut input = Vec::new();
            let mut quiet = 0;
            while quiet < 20 {
                let mut moved = false;
                while let Ok(b) = self.line.recv_byte() {
                    input.push(b);
                    moved = true;
                    let echo: &[u8] = match b {
                        b'\r' => cr,
                        b'\n' => lf,
                        _ => std::slice::from_ref(&b),
                    };
                    for e in echo {
                        self.line.send_byte(*e);
                    }
                }
                let mut buf = [0u8; 256];
                if let Ok(n) = self.stream.read(&mut buf) {
                    self.got.extend_from_slice(&buf[..n]);
                    moved = moved || n > 0;
                }
                quiet = if moved { 0 } else { quiet + 1 };
                std::thread::sleep(Duration::from_millis(2));
            }
            input
        }

        /// The guest writes `bytes`; returns what the client has received in
        /// total since the last call.
        fn guest_writes(&mut self, bytes: &[u8]) -> Vec<u8> {
            for b in bytes {
                self.line.send_byte(*b);
            }
            self.settle();
            std::mem::take(&mut self.got)
        }
    }

    const GREETING: [u8; 12] = crate::net::telnet::TelnetFilter::initial_handshake();

    /// `raw` is plain bytes whatever the client sends; `telnet` greets on
    /// connect and filters and escapes.
    #[test]
    fn raw_and_telnet_modes_are_fixed_from_the_first_byte() {
        let mut raw = Client::connect(SerialMode::Raw);
        assert_eq!(std::mem::take(&mut raw.got), b"", "raw sends nothing on connect");
        assert_eq!(raw.send(b"a\r\n\xff\xfb\x01"), b"a\r\n\xff\xfb\x01", "and passes IAC as data");
        assert_eq!(raw.guest_writes(b"x\xffy"), b"x\xffy");

        let mut tel = Client::connect(SerialMode::Telnet);
        assert_eq!(std::mem::take(&mut tel.got), GREETING, "telnet offers on connect");
        assert_eq!(tel.send(b"a\r\n\xff\xff"), b"a\r\n\xff", "filters, and unescapes 0xff");
        assert_eq!(tel.guest_writes(b"x\xffy"), b"x\xff\xffy");
    }

    /// Auto starts as raw bytes with nothing sent, so `nc`, `ncat` and
    /// `socat` (bare LF or CR for Enter) see exactly the guest's bytes,
    /// including 0xff, however long they type.
    #[test]
    fn auto_leaves_a_plain_client_plain() {
        for enter in [&b"\n"[..], &b"\r"[..]] {
            let mut c = Client::connect(SerialMode::Auto);
            assert_eq!(std::mem::take(&mut c.got), b"", "nothing is sent on connect");
            for _ in 0..3 {
                let mut line = b"root".to_vec();
                line.extend_from_slice(enter);
                assert_eq!(c.send(&line), line);
            }
            assert_eq!(c.guest_writes(b"login: \xff"), b"login: \xff", "no negotiation, no escaping");
        }
    }

    /// Only a client's first line can show it is telnet. Once it has sent a
    /// complete line with no sign of it, a later line ending CR LF (a script
    /// whose first line was a bare LF, a user pasting from a Windows editor)
    /// leaves it plain, and so does a telnet command after that.
    #[test]
    fn auto_keeps_a_client_plain_once_it_has_sent_a_line() {
        let mut c = Client::connect(SerialMode::Auto);
        assert_eq!(c.send(b"one\n"), b"one\n");
        assert_eq!(c.send(b"two\r\nthree\r\0"), b"two\r\nthree\r\0");
        assert_eq!(c.send(b"\xff\xfb\x01"), b"\xff\xfb\x01", "an IAC command is data now");
        assert_eq!(std::mem::take(&mut c.got), b"", "no negotiation was ever sent");
        assert_eq!(c.guest_writes(b"\xff"), b"\xff", "and nothing is escaped");

        // A first line ended by a bare CR (socat) locks as well.
        let mut c = Client::connect(SerialMode::Auto);
        assert_eq!(c.send(b"one\r"), b"one\r");
        assert_eq!(c.send(b"two\r\n"), b"two\r\n");
        assert_eq!(std::mem::take(&mut c.got), b"");
    }

    /// Once a client has sent a complete line with no sign of telnet, an
    /// IAC WILL later on is data, and 0xFF is not doubled to it.
    #[test]
    fn auto_does_not_take_a_locked_plain_client_for_telnet_on_a_later_iac_will() {
        let mut c = Client::connect(SerialMode::Auto);
        assert_eq!(c.send(b"login\n"), b"login\n");
        assert_eq!(c.send(&[255, 251, 1]), [255, 251, 1], "IAC WILL ECHO reaches the guest as data");
        assert_eq!(c.send(&[255, 253, 3, b'x']), [255, 253, 3, b'x'], "so does IAC DO SGA");
        assert_eq!(std::mem::take(&mut c.got), b"", "nothing was offered");
        assert_eq!(c.guest_writes(b"\xff"), b"\xff", "and the client is still plain");
    }

    /// A first line that arrives in pieces is not enough to call a client
    /// telnet, even if its last piece ends CR LF.
    #[test]
    fn auto_leaves_a_first_line_that_arrives_in_pieces_plain() {
        let mut c = Client::connect(SerialMode::Auto);
        assert_eq!(c.send(b"ab"), b"ab");
        assert_eq!(c.send(b"\r\n"), b"\r\n");
        assert_eq!(std::mem::take(&mut c.got), b"", "no negotiation");
        // A CR LF split between chunks is the same.
        let mut c = Client::connect(SerialMode::Auto);
        assert_eq!(c.send(b"ab\r"), b"ab\r");
        assert_eq!(c.send(b"\n"), b"\n");
        assert_eq!(std::mem::take(&mut c.got), b"");
        assert_eq!(c.guest_writes(b"\xff"), b"\xff");
    }

    /// A plain client may send the byte 0xFF as data: it is not a telnet
    /// command unless one follows, so it and the byte after it reach the guest
    /// and the client stays plain. The escaped pair 0xFF 0xFF is data too,
    /// whatever follows it.
    #[test]
    fn auto_passes_a_plain_clients_ff_data_byte() {
        let mut c = Client::connect(SerialMode::Auto);
        assert_eq!(c.send(b"x\xffy\xff\xffz\n"), b"x\xffy\xff\xffz\n");
        assert_eq!(std::mem::take(&mut c.got), b"", "no negotiation was sent");
        assert_eq!(c.guest_writes(b"\xff"), b"\xff", "and it is still plain");
        let mut c = Client::connect(SerialMode::Auto);
        assert_eq!(c.send(b"\xff\xff\xfb"), b"\xff\xff\xfb", "0xFF 0xFF 0xFB is escaped data, not WILL");
        assert_eq!(std::mem::take(&mut c.got), b"");
    }

    /// A lone 0xFF that ends a plain client's read (`printf '\377' | nc`)
    /// waits for what follows, and is data after a short idle with nothing
    /// after it.
    #[test]
    fn auto_gives_a_trailing_lone_ff_to_the_guest_after_a_pause() {
        let mut c = Client::connect(SerialMode::Auto);
        assert_eq!(c.send(b"\xff"), b"", "held back for the moment");
        std::thread::sleep(POLL_TIMEOUT + Duration::from_millis(100));
        assert_eq!(c.settle(), b"\xff", "then it is data");
        assert_eq!(std::mem::take(&mut c.got), b"", "and the client was not taken for telnet");
        assert_eq!(c.send(b"x\n"), b"x\n");
        assert_eq!(c.guest_writes(b"\xff"), b"\xff");
    }

    /// A command split from its IAC by a read boundary is still a command.
    #[test]
    fn auto_takes_an_iac_command_split_across_reads_as_telnet() {
        let mut c = Client::connect(SerialMode::Auto);
        assert_eq!(c.send(b"\xff"), b"");
        assert_eq!(c.send(b"\xfb\x18"), b"", "the option is not data");
        assert!(c.got.starts_with(&GREETING), "{:x?}", c.got);
    }

    /// A client that negotiates on connect (PuTTY in telnet mode, whose
    /// first bytes are IAC WILL NAWS ...) is telnet from its first byte: it
    /// gets the handshake, the guest sees none of its options, and 0xff is
    /// escaped from then on.
    #[test]
    fn auto_takes_a_client_that_negotiates_on_connect_as_telnet() {
        let mut c = Client::connect(SerialMode::Auto);
        let first = [255u8, 251, 31, 255, 251, 32, 255, 251, 24, 255, 253, 1, 255, 251, 3, 255, 253, 3];
        assert_eq!(c.send(&first), b"", "options are not data");
        assert!(c.got.starts_with(&GREETING), "{:x?}", c.got);
        c.got.clear();
        assert_eq!(c.send(b"a\r\0"), b"a\r\0");
        assert_eq!(c.guest_writes(b"\xff"), b"\xff\xff");
    }

    /// A line-mode telnet client that stays silent on connect (GNU inetutils,
    /// BSD, netkit and Apple-derived telnet, busybox, screen) sends its first
    /// line ending in CR LF. That line reaches the guest, the handshake
    /// follows so the client goes to character mode, and the Enter it then
    /// sends (CR NUL, or CR LF from busybox) is passed as telnet does.
    #[test]
    fn auto_takes_a_line_mode_telnet_client_at_its_first_line_ending() {
        let mut c = Client::connect(SerialMode::Auto);
        assert_eq!(c.guest_writes(b"PROM> \xff"), b"PROM> \xff", "plain until it shows itself");
        assert_eq!(c.send(b"go\r\n"), b"go\r\n");
        assert_eq!(c.got, GREETING, "then it is offered character mode");
        // Its reply to the offer is consumed, and the next key is data.
        assert_eq!(c.send(&[255, 253, 1, 255, 253, 3, b'x']), b"x");
        c.got.clear();
        assert_eq!(c.guest_writes(b"\xff"), b"\xff\xff");
    }

    /// The same, for a client whose first Enter is CR NUL.
    #[test]
    fn auto_takes_cr_nul_as_telnet_too() {
        let mut c = Client::connect(SerialMode::Auto);
        assert_eq!(c.send(b"\r\0"), b"\r\0");
        assert_eq!(c.got, GREETING);
    }

    /// A line-mode telnet client draws its first line itself, so a guest that
    /// echoes must not draw it again: the echo is dropped, in the CR LF, LF
    /// and doubled-newline shapes a tty gives, and what follows is untouched.
    #[test]
    fn auto_drops_the_guests_echo_of_a_line_mode_clients_first_line() {
        for echo in [&b"ab\r\n"[..], b"ab\n", b"ab\r\n\r\n", b"ab\r\r\n", b"ab"] {
            let mut c = Client::connect(SerialMode::Auto);
            c.send(b"ab\r\n");
            c.got.clear();
            let mut out = echo.to_vec();
            out.extend_from_slice(b"# ");
            assert_eq!(c.guest_writes(&out), b"# ", "echo {echo:?}");
            // Over: the same text later is the guest's own output.
            assert_eq!(c.guest_writes(b"ab\xff"), b"ab\xff\xff", "echo {echo:?}");
        }
    }

    /// The echo comes back byte by byte while the line is still being read,
    /// as a tty does it: the hold is in place before the guest sees the first
    /// byte, so none of the echo reaches the client, whichever way the tty
    /// echoes the line ending.
    #[test]
    fn auto_drops_an_echo_that_comes_back_as_the_line_is_read() {
        for (cr, lf) in [(&b"\r\n"[..], &b""[..]), (b"\r\n", b"\r\n"), (b"\r", b"\n"), (b"\r\n", b"\n")] {
            let mut c = Client::connect(SerialMode::Auto);
            let input = c.send_echoing(b"ab\r\n", cr, lf);
            assert_eq!(input, b"ab\r\n");
            assert_eq!(c.got, GREETING, "nothing of the echo arrived; cr {cr:?} lf {lf:?}");
            c.got.clear();
            assert_eq!(c.guest_writes(b"# "), b"# ", "cr {cr:?} lf {lf:?}");
        }
    }

    /// Only the line break that ends the echo is dropped, not whatever line
    /// breaks the guest prints after it.
    #[test]
    fn auto_drops_no_more_line_breaks_than_an_echo_can_end_with() {
        let mut c = Client::connect(SerialMode::Auto);
        c.send(b"ab\r\n");
        c.got.clear();
        assert_eq!(c.guest_writes(b"ab\r\n\r\n\r\n\r\n# "), b"\r\n\r\n# ", "four of the six are the echo's");
    }

    /// A complete echo is dropped for good: the hold ending by timeout, with
    /// the user thinking for longer than it, gives nothing back.
    #[test]
    fn auto_does_not_give_back_a_complete_echo_when_the_hold_times_out() {
        let mut c = Client::connect(SerialMode::Auto);
        c.send_echoing(b"ab\r\n", b"\r\n", b"");
        c.got.clear();
        std::thread::sleep(POLL_TIMEOUT + Duration::from_millis(100));
        c.settle();
        assert_eq!(std::mem::take(&mut c.got), b"", "nothing was released");
        assert_eq!(c.guest_writes(b"cd"), b"cd");
    }

    /// A guest that does not echo (a password prompt) loses nothing: its first
    /// byte is not the line's, so it goes through.
    #[test]
    fn auto_holds_nothing_back_from_a_guest_that_does_not_echo() {
        let mut c = Client::connect(SerialMode::Auto);
        c.send(b"hunter2\r\n");
        c.got.clear();
        assert_eq!(c.guest_writes(b"\r\nLast login: now\r\n$ "), b"\r\nLast login: now\r\n$ ");
    }

    /// A partial echo followed by something else is released unchanged, in
    /// order, with nothing added or lost.
    #[test]
    fn auto_releases_a_partial_echo_that_then_differs() {
        let mut c = Client::connect(SerialMode::Auto);
        c.send(b"abc\r\n");
        c.got.clear();
        assert_eq!(c.guest_writes(b"abX\r\nrest"), b"abX\r\nrest");
        let mut c = Client::connect(SerialMode::Auto);
        c.send(b"abc\r\n");
        c.got.clear();
        assert_eq!(c.guest_writes(b"abc"), b"", "echo so far is held");
        assert_eq!(c.guest_writes(b"abcd"), b"abcd", "the echo was complete, so this is the guest's own output");
    }

    /// An echo that stops short is released after the timeout, so a guest
    /// whose output merely starts like the line is not held for good.
    #[test]
    fn auto_releases_a_held_echo_after_the_timeout() {
        let mut c = Client::connect(SerialMode::Auto);
        c.send(b"abc\r\n");
        c.got.clear();
        assert_eq!(c.guest_writes(b"ab"), b"", "held for now");
        std::thread::sleep(POLL_TIMEOUT + Duration::from_millis(100));
        c.settle();
        assert_eq!(std::mem::take(&mut c.got), b"ab", "released unchanged");
        assert_eq!(c.guest_writes(b"abc"), b"abc", "and the hold is over");
    }

    /// Only a line-mode client's first line, which arrived whole, is held
    /// for: an IAC client, a bare Enter and a line sent in pieces are not.
    #[test]
    fn auto_holds_for_no_other_client() {
        let mut c = Client::connect(SerialMode::Auto);
        c.send(&[255, 251, 31]);
        c.got.clear();
        assert_eq!(c.guest_writes(b"ab"), b"ab");
        let mut c = Client::connect(SerialMode::Auto);
        c.send(b"\r\n");
        c.got.clear();
        assert_eq!(c.guest_writes(b"\r\n# "), b"\r\n# ");
        let mut c = Client::connect(SerialMode::Auto);
        c.send(b"ab");
        c.send(b"\r\n");
        c.got.clear();
        assert_eq!(c.guest_writes(b"ab\r\n# "), b"ab\r\n# ", "a first line in pieces is plain, so nothing is dropped");
    }

    /// `serial_mode` in the config reaches the console line: the
    /// default is auto, and each value gives the behaviour above.
    #[test]
    fn serial_mode_in_the_config_picks_the_console_lines_protocol() {
        let free = || TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
        assert_eq!(MachineConfig::default().serial_mode, SerialMode::Auto);
        for (mode, greets) in [(SerialMode::Auto, false), (SerialMode::Raw, false), (SerialMode::Telnet, true)] {
            let mut cfg = MachineConfig::default();
            cfg.serial_mode = mode;
            cfg.serial_port_a = Some(free());
            cfg.serial_port_b = Some(0);
            let (console, _) = console_backends(&cfg, "Test", None);
            let mut client = TcpStream::connect(("127.0.0.1", cfg.serial_port_a.unwrap())).unwrap();
            client.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
            let _ = console.recv_byte(); // accepts the client
            assert!(console.try_send_byte(b'A'));
            let mut got = Vec::new();
            let mut buf = [0u8; 32];
            while let Ok(n) = client.read(&mut buf) {
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
                if got.ends_with(b"A") {
                    break;
                }
            }
            let mut want = if greets { GREETING.to_vec() } else { Vec::new() };
            want.push(b'A');
            assert_eq!(got, want, "{mode:?}");
        }
    }

    /// Not a test: serves a line that echoes what it reads, in the mode named
    /// by `IRIS_SERIAL_MODE`, on the port in `IRIS_SERIAL_PORT`, for
    /// `IRIS_SERIAL_SECS` seconds, so real telnet and netcat clients can be
    /// pointed at it. Run with `--ignored`.
    #[test]
    #[ignore]
    fn serve_an_echo_line_for_real_clients() {
        let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        let mode = match env("IRIS_SERIAL_MODE", "auto").as_str() {
            "raw" => SerialMode::Raw,
            "telnet" => SerialMode::Telnet,
            _ => SerialMode::Auto,
        };
        let (line, _) = tcp_serial_backend(env("IRIS_SERIAL_PORT", "0").parse().unwrap(), mode).expect("bind");
        let end = Instant::now() + Duration::from_secs(env("IRIS_SERIAL_SECS", "20").parse().unwrap());
        while Instant::now() < end {
            match line.recv_byte() {
                // A guest that echoes, and answers a line end with CR LF.
                Ok(b'\n') => {}
                Ok(b'\r') => {
                    line.send_byte(b'\r');
                    line.send_byte(b'\n');
                }
                Ok(b) => line.send_byte(b),
                Err(_) => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    }

    /// A Unix-socket client that stops reading makes the backend refuse;
    /// once it reads, the refused byte goes and nothing came twice.
    #[cfg(unix)]
    #[test]
    fn a_unix_socket_client_that_stops_reading_holds_bytes_back() {
        let dir = temp_dir("full");
        let sock = dir.join("full.sock");
        let b = UnixSocketBackend::new(&sock).unwrap();
        assert!(b.try_send_byte(b'x'), "with no client a byte is taken and dropped");
        let mut client = UnixStream::connect(&sock).unwrap();
        let _ = b.recv_byte();
        let mut taken = 0usize;
        while b.try_send_byte(taken as u8) {
            taken += 1;
            assert!(taken < 16 << 20, "a client that never reads must fill up");
        }
        let mut got = vec![0u8; taken];
        client.read_exact(&mut got).unwrap();
        assert!(got.iter().enumerate().all(|(i, &v)| v == i as u8), "in order, none twice");
        assert!(b.try_send_byte(taken as u8), "room again");
        let mut last = [0u8; 1];
        client.read_exact(&mut last).unwrap();
        assert_eq!(last[0], taken as u8);
        drop(b);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fan-out owes a refusing end the byte the others took, refuses the
    /// next until it is paid, and pays it from the receive poll; no end sees
    /// a byte twice.
    #[test]
    fn a_fanout_owes_a_slow_end_its_byte_and_no_end_sees_one_twice() {
        let (fast, slow) = (Arc::new(Gate::default()), Arc::new(Gate::default()));
        fast.set(true);
        let f = FanoutBackend::new(vec![fast.clone(), slow.clone()]);
        assert!(f.try_send_byte(b'a'), "taken: the slow end is owed it");
        assert!(!f.try_send_byte(b'b'), "refused while an end is owed");
        assert_eq!(*fast.out.lock(), b"a");
        slow.set(true);
        let _ = f.recv_byte();
        assert_eq!(*slow.out.lock(), b"a", "the receive poll pays the debt");
        assert!(f.try_send_byte(b'b'));
        assert_eq!((&*fast.out.lock(), &*slow.out.lock()), (&b"ab".to_vec(), &b"ab".to_vec()));
    }

    /// A log tee records a byte once, when the end behind it takes it, and a
    /// byte given up on when the 16550 hands it to the plain send.
    #[test]
    fn a_tee_logs_each_byte_once_whether_taken_or_given_up_on() {
        let dir = std::env::temp_dir().join(format!("iris-serial-host-tee-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("tee.log");
        let gate = Arc::new(Gate::default());
        let tee = TeeBackend::new(gate.clone(), SerialLog::create(&log_path, Default::default()).unwrap());
        assert!(!tee.try_send_byte(b'a'));
        assert!(!tee.try_send_byte(b'a'), "a retry");
        tee.send_byte(b'a');
        gate.set(true);
        assert!(tee.try_send_byte(b'b'));
        assert_eq!(std::fs::read(&log_path).unwrap(), b"ab");
        assert_eq!(*gate.out.lock(), b"b");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
