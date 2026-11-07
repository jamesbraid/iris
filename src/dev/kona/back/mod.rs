//! The InfiniteReality pipe's drawing and display back end.
//!
//! Designed for Iris; the QEMU port follows from this design.
//!
//! This is the skeleton: a `Backend` that consumes every job, counts it by
//! kind and completes retrace requests at once, as `NullBackend` does, but
//! carries the pipe's configuration (raster managers, texture memory per
//! manager, board set, managed area) and answers the `kona` monitor command.
//! The token layer, raster stage, pixel store and display come in behind it.
//!
//! - `store`: the pixel store and the plane layout inside a pixel's words.
//! - `bound`: the most words a pixel's record may take, from the host's own
//!   budget arithmetic.
//! - `census`, `replay` (tests only): the jobs a command stream sends the back
//!   end, and the replay of a recorded stream into a board.

mod bound;
pub mod store;
#[cfg(test)]
mod census;
#[cfg(test)]
mod replay;

pub use bound::BoardSet;

use super::ident::KonaConfig;
use super::handle::DisplayHandle;
use super::job::{BackPort, Backend, Grammar, Job, Reply};
use std::io::{self, Write};

/// What the back end is told about the pipe it draws for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackendConfig {
    /// Raster managers: one to four.
    pub rm_count: u32,
    /// Texture memory on each raster manager, in bytes. One copy is kept
    /// whatever the count; this is the capacity a texture allocation fails at.
    pub texture_bytes_per_rm: u64,
    pub board_set: BoardSet,
    /// The managed area, in pixels, until a video format combination sets up
    /// another.
    pub managed: (u32, u32),
}

impl BackendConfig {
    /// The back end's view of the pipe `cfg` describes: an InfiniteReality2.
    pub fn for_pipe(cfg: &KonaConfig) -> Self {
        BackendConfig {
            rm_count: cfg.rm_count,
            texture_bytes_per_rm: cfg.texture_bytes_per_rm(),
            board_set: BoardSet::InfiniteReality,
            managed: (1280, 1024),
        }
    }

    /// The most words a pixel's record may take at the managed size.
    pub fn record_words(&self) -> u32 {
        bound::record_words(self.board_set, self.rm_count, self.managed.0, self.managed.1)
    }
}

/// The kinds of job the counters tell apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Textport,
    Kernel,
    Gl,
    Data,
    Bitmap,
    Video,
    Retrace,
    Context,
    Indirect,
    DmaIn,
    Dropped,
    FirmwareStart,
}

impl Kind {
    const ALL: [Kind; 12] = [
        Kind::Textport,
        Kind::Kernel,
        Kind::Gl,
        Kind::Data,
        Kind::Bitmap,
        Kind::Video,
        Kind::Retrace,
        Kind::Context,
        Kind::Indirect,
        Kind::DmaIn,
        Kind::Dropped,
        Kind::FirmwareStart,
    ];

    fn name(self) -> &'static str {
        match self {
            Kind::Textport => "textport commands",
            Kind::Kernel => "kernel commands",
            Kind::Gl => "GL commands",
            Kind::Data => "data runs",
            Kind::Bitmap => "bitmaps",
            Kind::Video => "video commands",
            Kind::Retrace => "retrace requests",
            Kind::Context => "context changes",
            Kind::Indirect => "indirect stores",
            Kind::DmaIn => "DMA packets",
            Kind::Dropped => "dropped words",
            Kind::FirmwareStart => "firmware starts",
        }
    }

    fn of(job: &Job<'_>) -> Kind {
        match job {
            Job::Command { grammar: Grammar::Textport, .. } => Kind::Textport,
            Job::Command { grammar: Grammar::Kernel, .. } => Kind::Kernel,
            Job::Command { grammar: Grammar::Gl, .. } => Kind::Gl,
            Job::Data { .. } => Kind::Data,
            Job::Bitmap { .. } => Kind::Bitmap,
            Job::Video { .. } => Kind::Video,
            Job::Retrace { .. } => Kind::Retrace,
            Job::Context(_) => Kind::Context,
            Job::Indirect { .. } => Kind::Indirect,
            Job::DmaIn { .. } => Kind::DmaIn,
            Job::Dropped { .. } => Kind::Dropped,
            Job::FirmwareStart { .. } => Kind::FirmwareStart,
        }
    }
}

/// The InfiniteReality back end.
pub struct KonaBackend {
    cfg: BackendConfig,
    counts: [u64; Kind::ALL.len()],
    resets: u64,
    /// Held between `attach` and `stop`, for the display thread once there is
    /// one.
    handle: Option<DisplayHandle>,
}

impl KonaBackend {
    pub fn new(cfg: BackendConfig) -> Self {
        KonaBackend { cfg, counts: [0; Kind::ALL.len()], resets: 0, handle: None }
    }

    pub fn config(&self) -> &BackendConfig {
        &self.cfg
    }

    fn status(&self, w: &mut dyn Write) -> io::Result<()> {
        let c = &self.cfg;
        writeln!(w, "raster managers   {}", c.rm_count)?;
        writeln!(w, "texture per RM    {} MiB", c.texture_bytes_per_rm >> 20)?;
        writeln!(w, "board set         {:?}", c.board_set)?;
        writeln!(w, "managed area      {} x {}", c.managed.0, c.managed.1)?;
        writeln!(w, "record bound      {} words per pixel", c.record_words())?;
        writeln!(w, "display handle    {}", if self.handle.is_some() { "attached" } else { "detached" })?;
        writeln!(w, "resets            {}", self.resets)
    }

    fn counters(&self, w: &mut dyn Write) -> io::Result<()> {
        for k in Kind::ALL {
            writeln!(w, "{:18}{}", k.name(), self.counts[k as usize])?;
        }
        Ok(())
    }
}

impl Backend for KonaBackend {
    fn job(&mut self, port: &mut BackPort<'_>, job: Job<'_>) -> Reply {
        self.counts[Kind::of(&job) as usize] += 1;
        match job {
            Job::Retrace { tag } => port.retrace_done(tag),
            Job::DmaIn { .. } => return Reply::Unclaimed,
            Job::FirmwareStart { .. } => port.set_managed_size(self.cfg.managed.0, self.cfg.managed.1),
            _ => {}
        }
        Reply::Done
    }

    fn attach(&mut self, handle: DisplayHandle) {
        self.handle = Some(handle);
    }

    fn reset(&mut self) {
        self.resets += 1;
    }

    fn stop(&mut self) {
        self.handle = None;
    }

    fn monitor(&mut self, args: &[&str], w: &mut dyn Write) -> io::Result<bool> {
        match args {
            ["status"] => self.status(w)?,
            ["counters"] => self.counters(w)?,
            ["help"] | [] => writeln!(w, "kona: status|counters")?,
            _ => return Ok(false),
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{attach, TestHost};
    use super::super::{Kona, KonaBuilder};
    use super::*;
    use std::sync::Arc;

    /// A monitor writer a test can read afterwards.
    #[derive(Clone, Default)]
    struct Capture(Arc<parking_lot::Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Capture {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().clone()).unwrap()
        }
    }

    fn board() -> (Arc<TestHost>, Arc<Kona>) {
        let cfg = KonaConfig::default();
        let host = TestHost::new();
        let kona = KonaBuilder::new(cfg, attach(&host)).backend(Box::new(KonaBackend::new(BackendConfig::for_pipe(&cfg)))).build().unwrap();
        (host, kona)
    }

    fn monitor(kona: &Kona, args: &[&str]) -> (bool, String) {
        let mut out = Vec::new();
        let known = kona.with_backend(|b| b.monitor(args, &mut out)).unwrap();
        (known, String::from_utf8(out).unwrap())
    }

    #[test]
    fn the_config_follows_the_pipe() {
        let c = BackendConfig::for_pipe(&KonaConfig::ir2(1).unwrap());
        assert_eq!(c, BackendConfig { rm_count: 1, texture_bytes_per_rm: 64 << 20, board_set: BoardSet::InfiniteReality, managed: (1280, 1024) });
        assert_eq!(c.record_words(), 16);
        assert_eq!(BackendConfig::for_pipe(&KonaConfig::ir2(2).unwrap()).record_words(), 32);
    }

    #[test]
    fn status_and_counters_answer_and_other_words_do_not() {
        let (_host, kona) = board();
        let (known, text) = monitor(&kona, &["status"]);
        assert!(known);
        assert!(text.contains("raster managers   2") && text.contains("texture per RM    64 MiB"), "{text}");
        assert!(text.contains("record bound      32 words per pixel"), "{text}");
        let (known, text) = monitor(&kona, &["counters"]);
        assert!(known && text.contains("GL commands       0"), "{text}");
        assert!(!monitor(&kona, &["pix", "1", "2"]).0);
    }

    /// The monitor reaches the back end through the board's own command.
    #[test]
    fn the_board_routes_the_kona_command() {
        use crate::traits::Device;
        let (_host, kona) = board();
        assert_eq!(kona.register_commands()[0].0, "kona");
        let out = Capture::default();
        kona.execute_command("kona", &["status"], Box::new(out.clone())).unwrap();
        assert!(out.text().contains("managed area      1280 x 1024"));
        assert!(kona.execute_command("kona", &["frob"], Box::new(out)).is_err());
    }

    /// Jobs are counted by kind, a retrace completes at once and the managed
    /// area is reported when the firmware starts.
    #[test]
    fn jobs_are_counted_and_retraces_complete() {
        let (_host, kona) = board();
        let w = |off: u64, v: u32| assert_eq!(kona.bus_write(off, 4, v as u64), crate::traits::BUS_OK);
        w(0xc10000, 1);
        w(0xc10000, 2);
        w(0xc10000, 7);
        w(0xc10000, 0x1f1);
        w(0xc10000, 0x1000_0000);
        w(0xc10000, 0);
        w(0xc10000, 0x4000);
        w(0xc10000, 0x1e7);
        w(0xc10000, 0x42);
        let (_, text) = monitor(&kona, &["counters"]);
        assert!(text.contains("textport commands 1"), "{text}");
        assert!(text.contains("kernel commands   2"), "{text}");
        assert!(text.contains("retrace requests  1"), "{text}");
        assert_eq!(kona.bus_read(0x800248, 4), (crate::traits::BUS_OK, 0x42));
    }

    /// A word that opens no command is counted, not answered.
    #[test]
    fn dropped_words_are_counted() {
        let (_host, kona) = board();
        for w in [1, 2, 7, 0x500, 0x500, 0x7f7f] {
            assert_eq!(kona.bus_write(0xc10000, 4, w), crate::traits::BUS_OK);
        }
        let (_, text) = monitor(&kona, &["counters"]);
        assert!(text.contains("dropped words     3"), "{text}");
    }

    #[test]
    fn attach_and_stop_hold_and_release_the_handle() {
        use crate::traits::Device;
        let (_host, kona) = board();
        assert!(monitor(&kona, &["status"]).1.contains("detached"));
        kona.start();
        assert!(monitor(&kona, &["status"]).1.contains("display handle    attached"));
        kona.stop();
        assert!(monitor(&kona, &["status"]).1.contains("detached"));
    }
}
