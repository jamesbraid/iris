//! The VPro board as an iris widget in the Octane's graphics port.
//!
//! Ported from hw/misc/sgi-odyssey.c and hw/mips/sgi/xio-widget.c in
//! jamesbraid/qemu-sgi-origami: the C device's MMIO entry points, its XIO
//! interrupt packets and its console, on iris's bus, HEART and display.
//!
//! The QEMU model runs under one lock (the big lock); here the whole
//! board state sits behind one mutex, which stands in for it. The CPU
//! thread's widget accesses take it; so does the display thread, which
//! counts the retrace and copies the scanout's frame to the window 60 times
//! a second; and so does the board's work thread ("odyssey-work",
//! sgi_gfx/head_work.rs), which runs queued command-FIFO stores in slices
//! and lets go of it where the C lets go of the big lock. Interrupt
//! packets go to HEART, the only interrupt target on the Octane: a packet
//! to widget 8 sets or clears the HEART pending bit of its vector, as the
//! packet write to HEART's widget space does in the QEMU machine.

use super::*;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use parking_lot::Mutex;

use crate::cpu::mips_core::CyclesPtr;
use crate::dev::ng1::rex3::Renderer;
use crate::disp::{Rex3Screen, FB_STRIDE};
use crate::ip30::{widget_large_base, widget_small_base};
use crate::irq::InterruptController;
use crate::traits::{BusDevice, BusRead16, BusRead32, BusRead64, BusRead8, Device, Resettable, BUS_ERR, BUS_OK};

/// The fabric packet's address bits; the widget sits above them.
pub const SGI_XIO_PACKET_ADDRESS_BITS: u32 = 48;
pub const SGI_XIO_WIDGETS: u32 = 16;
/// An interrupt packet's set bit, above the vector byte.
pub const SGI_XIO_INTERRUPT_SET: u64 = 1 << 8;

/// The fabric address of a widget/address pair: the widget in bits 51:48.
pub fn sgi_xio_compose(who: &str, widget: u32, address: u64, packet: &mut u64) -> bool {
    if widget >= SGI_XIO_WIDGETS || address >> SGI_XIO_PACKET_ADDRESS_BITS != 0 {
        crate::qlog!("{who}: packet fabric cannot encode widget {widget} address {address:#x}\n");
        return false;
    }
    *packet = (widget as u64) << SGI_XIO_PACKET_ADDRESS_BITS | address;
    true
}

thread_local! {
    /// Set while this thread is inside a widget access from the bus: QEMU's
    /// `current_cpu`, which is set for a processor's access and clear for
    /// qtest's (here, a unit test calling the MMIO entry points directly).
    static ODYSSEY_CURRENT_CPU: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the access in progress on this thread comes from a processor.
pub fn odyssey_current_cpu() -> bool {
    ODYSSEY_CURRENT_CPU.with(|c| c.get())
}

/// The board thread's name.
pub const ODYSSEY_WORK_THREAD: &str = "odyssey-work";

/// Start the board's work thread: its scheduling under the front lock, its
/// slices under the board lock.
pub fn odyssey_work_start(h: &OdysseyHandles) {
    let front: Arc<dyn SGIHeadWorkBql> = Arc::new(OdysseyFrontLock(h.front.clone()));
    let board: Arc<dyn SGIHeadWorkBql> = Arc::new(OdysseyBoardLock(h.board.clone()));
    let mut f = h.front.lock();
    sgi_head_work_start_split(&mut f.work, ODYSSEY_WORK_THREAD, front, board);
}

/// The machine stops: finish the waiting work (the C's run-state handler),
/// then stop the board's work thread. The board lock is let go first: the
/// thread may be waiting for it to start a slice.
pub fn odyssey_work_stop(h: &OdysseyHandles) {
    {
        let mut s = h.board.lock();
        let opaque = P::new(&mut **s).opaque();
        odyssey_work_vm_state(opaque, false, 0);
    }
    let mut f = h.front.lock();
    sgi_head_work_cleanup(&mut f.work);
}

/// HEART's interrupt-packet register in its widget space.
const HEART_PACKET: u64 = 0x80;

/// `sgi_xio_send_interrupt(&s->fabric_as, packet, number, set)`: the
/// interrupt-modify write. HEART takes it at 0x80 of widget 8.
pub fn sgi_xio_send_interrupt(s: &mut OdysseyFront, packet: u64, number: u8, set: bool) -> MemTxResult {
    let widget = (packet >> SGI_XIO_PACKET_ADDRESS_BITS) as u32;
    let address = packet & ((1u64 << SGI_XIO_PACKET_ADDRESS_BITS) - 1);
    if widget != s.heart_widget || address != HEART_PACKET {
        crate::qlog!("odyssey: interrupt packet to widget {widget} address {address:#x} has no receiver\n");
        return MEMTX_DECODE_ERROR;
    }
    match &s.irq {
        Some(irq) => {
            irq.set_line(number as u32, set);
            MEMTX_OK
        }
        None => MEMTX_DECODE_ERROR,
    }
}

/// The window the board shows the guest: what `GfxDisplay` hands out.
pub struct OdysseyDisplay {
    pub renderer: Mutex<Option<Box<dyn Renderer>>>,
    pub screen: Mutex<Rex3Screen>,
    pub cycles: Mutex<CyclesPtr>,
    pub screenshot: AtomicBool,
    pub frame: AtomicBool,
    local: Mutex<Option<Local>>,
}

struct Local {
    overlay: crate::debug_overlay::DebugOverlay,
    status: crate::disp::StatusBar,
    texture: crate::disp::StatusBarTexture,
    size: (usize, usize),
}

impl OdysseyDisplay {
    fn new() -> Self {
        Self {
            renderer: Mutex::new(None),
            screen: Mutex::new(Rex3Screen::new()),
            cycles: Mutex::new(CyclesPtr::dangling()),
            screenshot: AtomicBool::new(false),
            frame: AtomicBool::new(false),
            local: Mutex::new(None),
        }
    }

    /// Copy the scanout's frame (0x00RRGGBB) to the screen and present it.
    fn present(&self, frame: &[u32], width: usize, height: usize) {
        let mut screen = self.screen.lock();
        let w = width.min(FB_STRIDE);
        let h = height.min(screen.rgba.len() / FB_STRIDE);
        screen.width = w;
        screen.height = h;
        screen.prebuilt = true;
        screen.status_bar_only = false;
        for y in 0..h {
            let src = &frame[y * width..y * width + w];
            for (x, &rgb) in src.iter().enumerate() {
                let rgba = 0xff00_0000 | (rgb >> 16) & 0xff | rgb & 0xff00 | (rgb & 0xff) << 16;
                screen.fb_rgb[y * FB_STRIDE + x] = rgba;
                screen.rgba[y * FB_STRIDE + x] = rgba;
            }
        }
        screen.frame_seq = screen.frame_seq.wrapping_add(1);
        let mut renderer = self.renderer.lock();
        if let Some(renderer) = renderer.as_mut() {
            let mut local = self.local.lock();
            let local = local.get_or_insert_with(|| Local {
                overlay: crate::debug_overlay::DebugOverlay::new(),
                status: crate::disp::StatusBar::new(),
                texture: crate::disp::StatusBarTexture::new(),
                size: (0, 0),
            });
            if local.size != (w, h) {
                renderer.resize(w, h);
                local.size = (w, h);
            }
            let stats = crate::disp::BarStats {
                now: std::time::Instant::now(),
                cycles: self.cycles.lock().get(),
                fasttick: 0,
                decoded_delta: 0,
                l1i_hits: 0,
                l1i_fetches: 0,
                uncached: 0,
                hb: 0,
                count_hz: 0,
                gfifo_pending: 0,
            };
            renderer.present(&mut screen, &mut local.overlay, &mut local.status, &mut local.texture, &stats, false, None, None);
        }
        self.screenshot.store(false, Ordering::Release);
        self.frame.store(false, Ordering::Release);
    }

    fn stop(&self) {
        if let Some(renderer) = self.renderer.lock().as_mut() {
            renderer.stop();
        }
        self.local.lock().take();
    }
}

/// The VPro board on widget `slot`: a `BusDevice` for the widget's
/// windows.
pub struct OdysseyXio {
    small: u64,
    large: u64,
    pub state: OdysseyHandles,
    pub display: Arc<OdysseyDisplay>,
    running: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl OdysseyXio {
    /// A board of `product` (v12, v10, v8, v6) at `widget`: its interrupt
    /// packets reach `irq` (HEART), its DMA reaches `mem`.
    pub fn new(
        widget: u8,
        product: &str,
        irq: Arc<dyn InterruptController>,
        mem: Arc<dyn BusDevice>,
    ) -> Result<Arc<Self>, String> {
        set_address_space_memory(mem);
        let s = sgi_odyssey_new(product, widget as u32)?;
        s.front.lock().irq = Some(irq);
        Ok(Arc::new(Self {
            small: widget_small_base(widget),
            large: widget_large_base(widget),
            state: OdysseyHandles::new(s),
            display: Arc::new(OdysseyDisplay::new()),
            running: Arc::new(AtomicBool::new(false)),
            thread: Mutex::new(None),
        }))
    }

    pub fn display(&self) -> Arc<dyn crate::gfx_display::GfxDisplay> {
        self.display.clone()
    }

    pub fn set_cpu_cycles(&self, cycles: CyclesPtr) {
        *self.display.cycles.lock() = cycles;
    }

    /// The aperture offset of `addr`, in either window.
    fn offset(&self, addr: u64) -> Option<u64> {
        let off = if addr >= self.large { addr - self.large } else { addr.wrapping_sub(self.small) };
        (off < ODSY_APERTURE as u64).then_some(off)
    }

    fn read(&self, addr: u64, size: u32) -> (u32, u64) {
        match self.offset(addr) {
            Some(off) => {
                let cpu = ODYSSEY_CURRENT_CPU.with(|c| c.replace(true));
                let data = odyssey_handles_read(&self.state, off, size);
                ODYSSEY_CURRENT_CPU.with(|c| c.set(cpu));
                (BUS_OK, data)
            }
            None => (BUS_ERR, 0),
        }
    }

    fn write(&self, addr: u64, size: u32, val: u64) -> u32 {
        match self.offset(addr) {
            Some(off) => {
                let cpu = ODYSSEY_CURRENT_CPU.with(|c| c.replace(true));
                odyssey_handles_write(&self.state, off, val, size);
                ODYSSEY_CURRENT_CPU.with(|c| c.set(cpu));
                BUS_OK
            }
            None => BUS_ERR,
        }
    }

    /// One display frame: the retrace if it is due, then the dirty lines
    /// resolved and the frame presented.
    fn frame(state: &OdysseyHandles, display: &OdysseyDisplay) {
        let (frame, w, h) = {
            let mut s = state.board.lock();
            sgi_scanout_poll(&mut s.scanout, qemu_clock_virtual_ns());
            sgi_scanout_update(&mut s.scanout);
            (s.scanout.frame.clone(), s.scanout.frame_width as usize, s.scanout.frame_height as usize)
        };
        if w != 0 && h != 0 && frame.len() >= w * h {
            display.present(&frame, w, h);
        }
    }
}

impl crate::gfx_display::GfxDisplay for OdysseyDisplay {
    fn renderer_slot(&self) -> &Mutex<Option<Box<dyn Renderer>>> {
        &self.renderer
    }
    fn screen(&self) -> &Mutex<Rex3Screen> {
        &self.screen
    }
    fn request_screenshot(&self) {
        self.screenshot.store(true, Ordering::Release);
    }
    fn screenshot_pending(&self) -> bool {
        self.screenshot.load(Ordering::Acquire)
    }
    fn cycles(&self) -> CyclesPtr {
        *self.cycles.lock()
    }
    fn request_frame(&self) {
        self.frame.store(true, Ordering::Release);
    }
    fn frame_pending(&self) -> bool {
        self.frame.load(Ordering::Acquire)
    }
}

impl BusDevice for OdysseyXio {
    fn read8(&self, addr: u64) -> BusRead8 {
        let (status, data) = self.read(addr, 1);
        BusRead8 { status, data: data as u8 }
    }
    fn read16(&self, addr: u64) -> BusRead16 {
        let (status, data) = self.read(addr, 2);
        BusRead16 { status, data: data as u16 }
    }
    fn read32(&self, addr: u64) -> BusRead32 {
        let (status, data) = self.read(addr, 4);
        BusRead32 { status, data: data as u32 }
    }
    fn read64(&self, addr: u64) -> BusRead64 {
        let (status, data) = self.read(addr, 8);
        BusRead64 { status, data }
    }
    fn write8(&self, addr: u64, val: u8) -> u32 {
        self.write(addr, 1, val as u64)
    }
    fn write16(&self, addr: u64, val: u16) -> u32 {
        self.write(addr, 2, val as u64)
    }
    fn write32(&self, addr: u64, val: u32) -> u32 {
        self.write(addr, 4, val as u64)
    }
    fn write64(&self, addr: u64, val: u64) -> u32 {
        self.write(addr, 8, val)
    }
}

/// The display thread's frame period: the modelled mode's 60 Hz.
const FRAME_PERIOD: std::time::Duration = std::time::Duration::from_micros(16_667);

impl Device for OdysseyXio {
    fn step(&self, _cycles: u64) {}

    fn start(&self) {
        let mut t = self.thread.lock();
        if t.is_some() {
            return;
        }
        odyssey_work_start(&self.state);
        self.running.store(true, Ordering::Release);
        let (state, display, running) = (self.state.clone(), self.display.clone(), self.running.clone());
        *t = Some(
            std::thread::Builder::new()
                .name("odyssey-display".into())
                .spawn(move || {
                    let mut next = std::time::Instant::now();
                    while running.load(Ordering::Acquire) {
                        OdysseyXio::frame(&state, &display);
                        next += FRAME_PERIOD;
                        let now = std::time::Instant::now();
                        if next > now {
                            std::thread::sleep(next - now);
                        } else {
                            next = now;
                        }
                    }
                    display.stop();
                })
                .expect("odyssey display thread"),
        );
    }

    fn stop(&self) {
        self.running.store(false, Ordering::Release);
        if let Some(t) = self.thread.lock().take() {
            let _ = t.join();
        }
        odyssey_work_stop(&self.state);
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    fn get_clock(&self) -> u64 {
        0
    }
}

impl Drop for OdysseyXio {
    fn drop(&mut self) {
        // The work thread holds the state's lock through OdysseyBql; stop
        // it so the state can go.
        Device::stop(self);
    }
}

impl Resettable for OdysseyXio {
    fn power_on(&self) {
        odyssey_reset_hold(&mut self.state.board.lock());
    }
}

#[cfg(test)]
mod tests {
    //! The Octane VPro integration checks of QEMU's octane-vpro-test.c:
    //! the board answers through both widget-11 windows, the Crossbow
    //! carries its link only when it is fitted, and a routed interrupt
    //! sets and clears its vector in HEART's pending register.
    use super::*;
    use crate::dev::heart::{host_clock, Heart, HeartConfig, PIU_BASE};
    use crate::dev::heart_mem::HeartRam;
    use crate::dev::xbow::Xbow;

    const WIDGET: u8 = 11;
    const VECTOR: u32 = 18;

    fn board() -> (Arc<Heart>, Arc<OdysseyXio>) {
        let heart = Arc::new(Heart::new(HeartConfig::octane([128 << 20, 0, 0, 0]), host_clock()));
        let ram = Arc::new(HeartRam::try_new([128, 0, 0, 0]).expect("ram"));
        let v = OdysseyXio::new(WIDGET, "v12", heart.clone(), ram.dma_view()).expect("board");
        (heart, v)
    }

    fn pending(heart: &Heart) -> u64 {
        heart.read64(PIU_BASE + 0x10030).data
    }

    #[test]
    fn the_board_answers_through_both_widget_windows() {
        let (_heart, v) = board();
        for base in [widget_small_base(WIDGET), widget_large_base(WIDGET)] {
            assert_eq!(v.read32(base + 0x4).data, 0xac01_3047);
            assert_eq!(v.read32(base + 0x24).data & 0xf, WIDGET as u32);
        }
    }

    #[test]
    fn the_crossbow_carries_the_graphics_link_only_when_fitted() {
        let fitted = Xbow::new(4, (1 << 8) | (1 << 15) | (1 << WIDGET));
        let empty = Xbow::new(4, (1 << 8) | (1 << 15));
        assert!(fitted.carries(WIDGET));
        assert!(!empty.carries(WIDGET));
    }

    #[test]
    fn a_routed_interrupt_sets_and_clears_its_heart_vector() {
        let (heart, v) = board();
        let b = widget_small_base(WIDGET);
        v.write32(b + 0x1044, 8 << 25);
        v.write32(b + 0x104c, 0x80);
        v.write32(b + 0x1054, VECTOR);
        assert_eq!(pending(&heart) & 1 << VECTOR, 0);
        // The FIFO is empty, so its low-water flag is already pending.
        v.write32(b + 0x1034, 0x200);
        assert_ne!(pending(&heart) & 1 << VECTOR, 0);
        v.write32(b + 0x101c, 0x200);
        assert_eq!(pending(&heart) & 1 << VECTOR, 0);
    }
}
