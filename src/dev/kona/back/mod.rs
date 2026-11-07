//! The InfiniteReality pipe's drawing and display back end.
//!
//! This is the skeleton: a `Backend` that consumes every job, counts it by
//! kind and completes retrace requests at once, as `NullBackend` does, but
//! carries the pipe's configuration (raster managers, texture memory per
//! manager, board set, managed area) and answers the `kona` monitor command.
//! The token layer, raster stage, pixel store and display come in behind it.
//!
//! - `store`: the pixel store and the plane layout inside a pixel's words.
//! - `raster`: drawing into the store, and the fill rule.
//! - `token`: the commands that say what to draw (the textport).
//! - `pack`: the host and bus pixel formats and their packers.
//! - `bound`: the most words a pixel's record may take, from the host's own
//!   budget arithmetic.
//! - `census`, `replay` (tests only): the jobs a command stream sends the back
//!   end, and the replay of a recorded stream into a board.

mod bound;
pub mod pack;
pub mod raster;
pub mod store;
pub mod token;
#[cfg(test)]
mod census;
#[cfg(test)]
mod replay;

pub use bound::BoardSet;

use super::ident::KonaConfig;
use super::handle::DisplayHandle;
use super::job::{BackPort, Backend, Grammar, Job, Reply};
use raster::FillRule;
use store::layout::{BufferConfig, Layout, Refusal};
use token::textport::Textport;
use token::x::XState;
use store::PixelStore;
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
    /// Which edges of a whole-pixel box light.
    pub fill_rule: FillRule,
}

impl BackendConfig {
    /// The back end's view of the pipe `cfg` describes: an InfiniteReality2.
    pub fn for_pipe(cfg: &KonaConfig) -> Self {
        BackendConfig {
            rm_count: cfg.rm_count,
            texture_bytes_per_rm: cfg.texture_bytes_per_rm(),
            board_set: BoardSet::InfiniteReality,
            managed: (1280, 1024),
            fill_rule: FillRule::OmitLeftBottom,
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
    store: PixelStore,
    textport: Textport,
    x: XState,
    /// Buffer-configuration commands that got a layout, and those refused by
    /// reason (indexed by `Refusal`).
    layouts: u64,
    refused: [u64; Refusal::ALL.len()],
    /// Buffer-configuration commands that were not ten words, and whether that
    /// has been logged.
    malformed: u64,
    malformed_logged: bool,
    /// The layout of the last buffer configuration that got one.
    last_layout: Option<Layout>,
}

/// The GL grammar's buffer-configuration selector.
const GL_BUFFER_CONFIG: u32 = 0x1;

impl KonaBackend {
    pub fn new(cfg: BackendConfig) -> Self {
        KonaBackend {
            store: PixelStore::new(cfg.managed.0, cfg.managed.1),
            textport: Textport::default(),
            x: XState::default(),
            cfg,
            counts: [0; Kind::ALL.len()],
            resets: 0,
            handle: None,
            layouts: 0,
            refused: [0; Refusal::ALL.len()],
            malformed: 0,
            malformed_logged: false,
            last_layout: None,
        }
    }

    /// A buffer configuration: lay its planes out in a pixel's words, within
    /// the host's budget, or count why not. The token layer will keep the
    /// layout per context; until then the last one is shown by `kona status`.
    fn buffer_config(&mut self, args: &[u32]) {
        let Ok(words) = <&[u32; BufferConfig::WORDS]>::try_from(args) else {
            self.malformed += 1;
            if !std::mem::replace(&mut self.malformed_logged, true) {
                crate::dlog!(crate::devlog::LogModule::Kona, "kona: buffer configuration of {} words, not {}; refused", args.len(), BufferConfig::WORDS);
            }
            return;
        };
        match Layout::new(&BufferConfig::from_words(words), 0, self.cfg.record_words()) {
            Ok(l) => {
                self.layouts += 1;
                self.last_layout = Some(l);
            }
            Err(why) => self.refused[why as usize] += 1,
        }
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
        writeln!(w, "pixel store       {} word arrays of {} x {}", self.store.allocated().len(), self.store.width(), self.store.height())?;
        writeln!(w, "textport          colour {} raster {:?}, {} drawn, {} unmodelled", self.textport.colour(), self.textport.raster(), self.textport.drawn, self.textport.unmodelled)?;
        let gc = match self.x.gc() {
            Ok(g) => format!("{:?} fg {:#x} bg {:#x} mask {:#x} gx {}", g.target, g.foreground, g.background, g.mask, g.gx),
            Err(why) => format!("not decoded ({why:?})"),
        };
        writeln!(w, "X gc              {gc}")?;
        writeln!(
            w,
            "X state           origin {:?}, {} clip rects ({} refused), scissor {:?}, {} runs dropped",
            self.x.origin(),
            self.x.clip_rects().len(),
            self.x.clips_refused,
            self.x.scissor(),
            self.x.runs_dropped
        )?;
        let f = &self.x.fills;
        writeln!(w, "X fills           {} drawn, refused: {} context, {} target, {} destination", f.drawn, f.bad_context, f.other_target, f.bad_destination)?;
        writeln!(w, "fill rule         {:?}", self.cfg.fill_rule)?;
        writeln!(w, "layouts           {} made", self.layouts)?;
        if self.malformed != 0 {
            writeln!(w, "  refused         {} x not {} words", self.malformed, BufferConfig::WORDS)?;
        }
        for r in Refusal::ALL {
            let n = self.refused[r as usize];
            if n != 0 {
                writeln!(w, "  refused         {n} x {r:?}")?;
            }
        }
        if let Some(l) = &self.last_layout {
            writeln!(w, "  last            colour {:?} depth {:?} stencil {:?} multisample {:?} accumulation {:?} ({} words)", l.colour, l.depth, l.stencil, l.multisample, l.accumulation, l.words())?;
        }
        writeln!(w, "resets            {}", self.resets)
    }

    /// The words of one pixel that have an array, as the planes hold them.
    fn pix(&self, x: u32, y: u32, w: &mut dyn Write) -> io::Result<()> {
        if self.store.word_slot(x, y, 0).is_none() {
            return writeln!(w, "({x}, {y}) is outside the {} x {} managed area", self.store.width(), self.store.height());
        }
        let words = self.store.allocated();
        for i in &words {
            writeln!(w, "word {i:2}  {:#010x}", self.store.read(x, y, *i))?;
        }
        writeln!(w, "{} word arrays allocated", words.len())
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
            Job::Command { grammar: Grammar::Gl, selector: GL_BUFFER_CONFIG, args, .. } => self.buffer_config(args),
            Job::Command { grammar: Grammar::Textport, selector, args, .. } => {
                self.textport.command(&self.store, self.cfg.fill_rule, selector, args);
            }
            Job::Command { grammar: Grammar::Kernel, selector, args, .. } => {
                if !self.x.command(selector, args) && token::xfill::is_draw(selector) {
                    let surface = raster::Surface::new(&self.store, token::textport::COLOUR_WORD);
                    self.x.draw_command(&surface, selector, args);
                }
            }
            Job::Bitmap { header, payload } => self.textport.bitmap(&self.store, header, payload),
            _ => {}
        }
        Reply::Done
    }

    fn attach(&mut self, handle: DisplayHandle) {
        self.handle = Some(handle);
    }

    fn reset(&mut self) {
        self.resets += 1;
        self.store.reset();
        self.textport.reset();
        self.x.reset();
        self.last_layout = None;
    }

    fn stop(&mut self) {
        self.handle = None;
    }

    fn monitor(&mut self, args: &[&str], w: &mut dyn Write) -> io::Result<bool> {
        match args {
            ["status"] => self.status(w)?,
            ["counters"] => self.counters(w)?,
            ["hash"] => writeln!(w, "{:016x}", self.store.checksum())?,
            ["pix", x, y] => {
                let (Ok(x), Ok(y)) = (x.parse::<u32>(), y.parse::<u32>()) else { return Ok(false) };
                self.pix(x, y, w)?
            }
            ["help"] | [] => writeln!(w, "kona: status|counters|hash|pix X Y")?,
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
        assert_eq!(c, BackendConfig { rm_count: 1, texture_bytes_per_rm: 64 << 20, board_set: BoardSet::InfiniteReality, managed: (1280, 1024), fill_rule: FillRule::OmitLeftBottom });
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
        assert!(!monitor(&kona, &["shot", "x.png"]).0);
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

    fn backend() -> KonaBackend {
        KonaBackend::new(BackendConfig::for_pipe(&KonaConfig::default()))
    }

    fn run(b: &mut KonaBackend, args: &[&str]) -> (bool, String) {
        let mut out = Vec::new();
        let known = b.monitor(args, &mut out).unwrap();
        (known, String::from_utf8(out).unwrap())
    }

    /// A GL buffer configuration is laid out within the budget; one the hardware
    /// documents do not cover is refused and counted; so is one of the wrong
    /// length; the monitor shows them.
    #[test]
    fn a_buffer_configuration_gets_a_layout_or_a_counted_refusal() {
        let mut b = backend();
        assert!(run(&mut b, &["status"]).1.contains("layouts           0 made"));
        b.buffer_config(&[0, 10, 0, 1, 0, 4, 24, 8, 25, 0]);
        b.buffer_config(&[0, 8, 0, 0, 0, 2, 0, 0, 0, 0]);
        b.buffer_config(&[0, 8, 0]);
        b.buffer_config(&[0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let text = run(&mut b, &["status"]).1;
        assert!(text.contains("layouts           1 made"), "{text}");
        assert!(text.contains("refused         2 x not 10 words"), "{text}");
        assert!(text.contains("refused         1 x Samples"), "{text}");
        assert!(text.contains("(13 words)"), "{text}");
    }

    /// Run one job through `Backend::job` with a front end behind it.
    fn send(b: &mut KonaBackend, job: Job<'_>) {
        use super::super::front::{Front, Host};
        use super::super::pipe::Core;
        let t = TestHost::new();
        let cfg = KonaConfig::default();
        let host = Host {
            front: parking_lot::Mutex::new(Front::new(&cfg)),
            attach: attach(&t),
            held_words: Default::default(),
            reasons_waiting: Default::default(),
            list_running: Default::default(),
            activity: Default::default(),
        };
        let mut core = Core::new(cfg, super::super::flash::Flash::fresh());
        b.job(&mut BackPort { core: &mut core, host: &host }, job);
    }

    fn command(b: &mut KonaBackend, grammar: Grammar, selector: u32, args: &[u32]) {
        send(b, Job::Command { grammar, selector, args, packed: 0, handled: false });
    }

    /// A framed GL command reaches the layout through `Backend::job`; the
    /// same selector in the textport grammar does not.
    #[test]
    fn a_framed_gl_buffer_configuration_is_laid_out() {
        let mut b = backend();
        let args = [0, 10, 0, 1, 0, 4, 24, 8, 25, 0];
        command(&mut b, Grammar::Textport, 1, &args);
        command(&mut b, Grammar::Gl, 1, &args);
        command(&mut b, Grammar::Gl, 1, &args[..3]);
        let text = run(&mut b, &["status"]).1;
        assert!(text.contains("layouts           1 made") && text.contains("1 x not 10 words"), "{text}");
        assert!(text.contains("(13 words)"), "{text}");
    }

    /// Textport commands draw into the colour word of the store, a bitmap
    /// draws at the raster position, and the hash follows the contents.
    #[test]
    fn textport_commands_draw_into_the_store() {
        let mut b = backend();
        let empty = run(&mut b, &["hash"]).1;
        command(&mut b, Grammar::Textport, 2, &[7]);
        command(&mut b, Grammar::Textport, 5, &[0, 0, 3, 3]);
        command(&mut b, Grammar::Textport, 4, &[10, 10]);
        send(&mut b, Job::Bitmap { header: &[1, 1, 0, 0, 2, 0], payload: &[1 << 31] });
        assert_eq!(b.store.read(10, 10, 1), 7);
        assert_eq!(b.store.read(3, 3, 1), 7);
        assert_eq!(b.store.read(1, 1, 1), 7);
        assert_eq!(b.store.read(0, 0, 1), 0, "the near corner is out");
        let text = run(&mut b, &["status"]).1;
        assert!(text.contains("colour 7 raster (12, 10), 2 drawn"), "{text}");
        let drawn = run(&mut b, &["hash"]).1;
        assert_ne!(empty, drawn);
        b.reset();
        assert_ne!(run(&mut b, &["hash"]).1, drawn);
        assert_eq!(b.store.read(3, 3, 1), 0);
    }

    /// X state commands load the context, position, clip and scissor; the
    /// monitor shows what was understood.
    #[test]
    fn x_state_commands_are_taken_and_shown() {
        let mut b = backend();
        let gc = [0x5a000, 0x1ffe, 0xa5000, 0x1ffe, 0xf00fff, 0, 0x180c, 0x40001, 1, 6, 5, 1, 1];
        command(&mut b, Grammar::Kernel, 0x129, &gc);
        command(&mut b, Grammar::Kernel, 0x12a, &[240, 295, 100, 50]);
        command(&mut b, Grammar::Kernel, 0x12b, &[1, 10, 20, 110, 220]);
        command(&mut b, Grammar::Kernel, 0x12d, &[10, 20, 100, 200]);
        let text = run(&mut b, &["status"]).1;
        assert!(text.contains("MainIndex16 fg 0x5a00 bg 0xa500 mask 0xffff gx 3"), "{text}");
        assert!(text.contains("origin (240, 295), 1 clip rects (0 refused), scissor Some([10, 20, 100, 200])"), "{text}");
        // The same selector numbers in another grammar are not X state.
        command(&mut b, Grammar::Gl, 0x129, &[0; 13]);
        command(&mut b, Grammar::Kernel, 0x1f1, &[0, 0, 0]);
        assert!(run(&mut b, &["status"]).1.contains("MainIndex16"));
        b.reset();
        assert!(run(&mut b, &["status"]).1.contains("not decoded"));
    }

    /// An X box through `Backend::job` draws into the colour word of the store
    /// under the loaded context, and a command that is no X token leaves a
    /// waiting run alone only if it belongs to the run.
    #[test]
    fn an_x_box_draws_into_the_store() {
        let mut b = backend();
        let gc = [0x5a000, 0x1ffe, 0xa5000, 0x1ffe, 0xf00fff, 0, 0x180c, 0x40001, 1, 6, 5, 1, 1];
        command(&mut b, Grammar::Kernel, 0x129, &gc);
        command(&mut b, Grammar::Kernel, 0x154, &[2 << 16 | 3, 5 << 16 | 7]);
        assert_eq!(b.store.read(2, 1023 - 3, 1), 0x5a00);
        assert_eq!(b.store.read(5, 1023 - 3, 1), 0);
        // A data token, another command, then a box: the run did not survive.
        command(&mut b, Grammar::Kernel, 0x142, &[10 << 16 | 10, 12 << 16 | 12]);
        command(&mut b, Grammar::Kernel, 0x1f1, &[0, 0, 0]);
        command(&mut b, Grammar::Kernel, 0x154, &[20 << 16 | 20, 21 << 16 | 21]);
        assert_eq!(b.store.read(10, 1023 - 10, 1), 0);
        assert_eq!(b.store.read(20, 1023 - 20, 1), 0x5a00);
        let text = run(&mut b, &["status"]).1;
        assert!(text.contains("X fills           2 drawn, refused: 0 context, 0 target, 0 destination"), "{text}");
    }

    #[test]
    fn pix_shows_the_allocated_words_of_a_pixel() {
        let mut b = backend();
        let (known, text) = run(&mut b, &["pix", "3", "4"]);
        assert!(known && text.contains("0 word arrays allocated"), "{text}");
        b.store.write(3, 4, 2, 0xabcd);
        let text = run(&mut b, &["pix", "3", "4"]).1;
        assert!(text.contains("word  2  0x0000abcd") && text.contains("1 word arrays"), "{text}");
        assert!(run(&mut b, &["pix", "5000", "0"]).1.contains("outside"));
        assert!(!run(&mut b, &["pix", "x", "0"]).0);
        // A reset zeroes the store in place.
        b.reset();
        assert!(run(&mut b, &["pix", "3", "4"]).1.contains("word  2  0x00000000"));
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
