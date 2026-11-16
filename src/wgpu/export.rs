//! A ring of scan-out buffers another process can import, for a host that
//! shows the frame somewhere else without copying it.
//!
//! A board composes each frame into a buffer on the GPU (`scanout.rs`). By
//! default that is one buffer, reused every frame, which only this process
//! can read. With export on (`GpuOptions::export_memory`, or
//! `set_export_memory`) the scan-out composes into a small ring of buffers
//! instead, each one Vulkan memory allocated for export. The embedder asks a
//! slot for a handle (a file descriptor on Linux, an NT handle on Windows)
//! and passes it to the consumer, which imports the memory into its own
//! Vulkan device. Nothing is copied on the host; the consumer reads the
//! pixels the board's kernel wrote.
//!
//! Size. The ring is sized by its owner, at run time: `slots_for` gives the
//! count for a set of consumers (2, plus one for each consumer that copies a
//! slot out and two for each that shows the slot itself, up to four
//! consumers, at most 16 slots), and `set_slots` resizes the ring to it.
//! Growing allocates buffers; shrinking *retires* the highest slots: the
//! board stops writing them and their memory is freed by `reap` once no
//! consumer holds them. `replace` swaps one slot for a new one while the
//! others stay lent.
//!
//! Ownership. A slot is in one of four states and the board only ever writes
//! one that is *free*:
//!
//! - *free*: not the newest frame and not held; the next compose may take it;
//! - *writing*: a compose is recording into it;
//! - *newest*: the last frame composed; the next compose avoids it unless
//!   every other slot is held, so a consumer that comes for the newest frame
//!   gets a whole one;
//! - *held*: a consumer (numbered 0 to 3) took it with
//!   [`ExportRing::acquire_newest`] and has not given it back with
//!   [`ExportRing::release`]; several consumers may hold one slot. The board
//!   never writes it. If every slot is held the compose is skipped and counted
//!   ([`ExportStats::skipped`]); the picture does not update until a slot
//!   comes back, and nothing fails.
//!
//! Each frame has a serial that only grows, and `release` takes the serial
//! of the frame handed out, so a late or repeated release of a slot that
//! has since been reused does nothing. That is a release counter in the
//! sense of a consumer that stores "I am done with frame N".
//!
//! Change signal. Each compose is compared on the GPU with the frame before
//! it, and [`ExportRing::damage`] says which rows differ: `None` when none
//! do, so a still screen need not be published at all. (`Rex3Screen.frame_seq`
//! is no signal: the VPro's GPU path presents every frame and does not move
//! it.) The comparison reads two frames in GPU memory and returns four words.
//!
//! Ordering. The compose is a queue submission. [`ExportFrame::is_ready`]
//! says without blocking whether it has finished on the GPU (it turns true
//! when the device is next polled, which every submission does), and
//! [`ExportRing::wait_ready`] blocks until it has; only then may another
//! process read the slot. wgpu has no external-semaphore API, so the handoff is a
//! host wait followed by the consumer being told; a consumer on another
//! device must not read a frame before that.
//!
//! The memory is device-local, allocated dedicated, one buffer per
//! allocation, with the usage `STORAGE | TRANSFER_SRC` and no create flags;
//! the consumer imports it as a buffer of that size with the same usage.
//! The layout of the frame in a slot is [`ExportFrame`]: `width * height`
//! words of 0x00RRGGBB (DRM `XR24`), rows `stride` bytes apart, from offset
//! 0.
//!
//! Vulkan only. On other backends (Metal) there is no ring: `Scanout` keeps
//! its single buffer and `Gpu::export_handle_type` is None.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use super::device::Gpu;

/// DRM `XR24` (`XRGB8888`): little-endian words 0x00RRGGBB.
pub const FOURCC_XR24: u32 = 0x3432_5258;

/// Consumers a ring serves at once.
pub const MAX_CONSUMERS: usize = 4;

/// Most slots a ring has live at once.
pub const MAX_SLOTS: usize = 16;

/// Slots a scan-out's ring starts with: one being composed, the newest, and
/// one a single copying consumer holds. The owner resizes it with
/// `Scanout::set_export_slots` as consumers come and go.
pub const DEFAULT_SLOTS: usize = 3;

/// The slots a ring needs for consumers with these credits: 2, plus each
/// consumer's credit (1 for one that copies a slot out and releases it, 2
/// for one that shows the slot itself and so holds one while it waits for
/// the next), clamped to the most a ring has. At most four consumers count.
pub fn slots_for(credits: &[u32]) -> usize {
    (2 + credits.iter().take(MAX_CONSUMERS).sum::<u32>() as usize).clamp(2, MAX_SLOTS)
}

/// How exported memory is shared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandleType {
    /// A file descriptor for `VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD`
    /// (`VK_KHR_external_memory_fd`).
    OpaqueFd,
    /// An NT handle for `..._OPAQUE_WIN32` (`VK_KHR_external_memory_win32`).
    OpaqueWin32,
}

impl HandleType {
    /// The kind this platform exports with, when the adapter has the
    /// extension.
    pub fn of(have: wgpu::Features) -> Option<HandleType> {
        if cfg!(target_os = "linux") && have.contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_FD) {
            Some(HandleType::OpaqueFd)
        } else if cfg!(target_os = "windows") && have.contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_WIN32) {
            Some(HandleType::OpaqueWin32)
        } else {
            None
        }
    }

    /// The wgpu feature that turns the extension on.
    pub fn feature(self) -> wgpu::Features {
        match self {
            HandleType::OpaqueFd => wgpu::Features::VULKAN_EXTERNAL_MEMORY_FD,
            HandleType::OpaqueWin32 => wgpu::Features::VULKAN_EXTERNAL_MEMORY_WIN32,
        }
    }
}

/// An exported handle, owned by the caller: closing it does not free the
/// memory, which the ring owns, and the consumer's import holds its own
/// reference.
#[derive(Debug)]
pub enum ExportHandle {
    #[cfg(unix)]
    Fd(std::os::fd::OwnedFd),
    #[cfg(windows)]
    Win32(std::os::windows::io::OwnedHandle),
}

/// What a consumer needs to import a slot, the same for every slot of a ring.
#[derive(Clone, Debug)]
pub struct ExportInfo {
    pub handle_type: HandleType,
    /// The `VkBuffer` size: create the imported buffer with this.
    pub buffer_size: u64,
    /// The allocation size: import with this.
    pub alloc_size: u64,
    /// The memory type index on the exporting device; meaningful on an
    /// adapter with the same two UUIDs below.
    pub memory_type: u32,
    /// The allocation is a dedicated one for the buffer: import it the
    /// same way (`VkMemoryDedicatedAllocateInfo`).
    pub dedicated: bool,
    /// The `VkBufferUsageFlags` the buffer was created with.
    pub usage: u32,
    /// The `VkBufferCreateFlags` it was created with (none).
    pub create_flags: u32,
    /// `VkPhysicalDeviceIDProperties`: import only on a device with both.
    pub device_uuid: [u8; 16],
    pub driver_uuid: [u8; 16],
}

/// A frame a consumer holds, until it is released.
#[derive(Clone, Debug)]
pub struct ExportFrame {
    pub index: usize,
    /// Grows with every frame composed into the ring; pass it to `release`.
    pub serial: u64,
    pub width: u32,
    pub height: u32,
    /// Bytes between rows: `width * 4` rounded up to a multiple of 256.
    pub stride: u32,
    /// When the compose began (CLOCK_MONOTONIC nanoseconds on Unix).
    pub sample_ns: u64,
    pub fourcc: u32,
    /// The consumer that holds it.
    pub consumer: usize,
    submission: wgpu::SubmissionIndex,
    done: Arc<AtomicBool>,
}

impl ExportFrame {
    /// Whether the compose that wrote the frame has finished on the GPU.
    /// Does not block; it turns true when the device is polled after the
    /// work is done.
    pub fn is_ready(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
}

/// Counters, for logs and for a test of the ownership rules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExportStats {
    /// Frames composed into the ring.
    pub composed: u64,
    /// Composes skipped because every slot was held.
    pub skipped: u64,
    pub acquired: u64,
    pub released: u64,
}

#[derive(Clone, Default)]
struct SlotState {
    /// Serial of the frame in it (0: none yet).
    serial: u64,
    width: u32,
    height: u32,
    submission: Option<wgpu::SubmissionIndex>,
    done: Option<Arc<AtomicBool>>,
    stride: u32,
    sample_ns: u64,
    writing: bool,
    /// The consumers that hold it, one bit each.
    held: u8,
    /// No longer composed into; freed once nobody holds it.
    retired: bool,
    freed: bool,
}

#[derive(Default)]
struct State {
    slots: Vec<SlotState>,
    next_serial: u64,
    newest: Option<usize>,
    stats: ExportStats,
}

struct Slot {
    buffer: wgpu::Buffer,
    /// None once freed.
    raw: Option<sys::RawExport>,
    diff: Diff,
}

/// The change signal of one slot's frame: rows that differ from the frame
/// before it, written by `export_diff.wgsl` and copied to `read`.
struct Diff {
    out: wgpu::Buffer,
    read: wgpu::Buffer,
    params: wgpu::Buffer,
}

impl Diff {
    fn new(device: &wgpu::Device) -> Diff {
        let buf = |label, size, usage| device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size, usage, mapped_at_creation: false });
        Diff {
            out: buf("export diff", 16, wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST),
            read: buf("export diff read", 16, wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST),
            params: buf("export diff params", 16, wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST),
        }
    }
}

struct DiffPipeline {
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::ComputePipeline,
}

impl DiffPipeline {
    fn new(device: &wgpu::Device) -> DiffPipeline {
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry { binding, visibility: wgpu::ShaderStages::COMPUTE, ty, count: None };
        let buffer = |ty| wgpu::BindingType::Buffer { ty, has_dynamic_offset: false, min_binding_size: None };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("export diff"),
            entries: &[
                entry(0, buffer(wgpu::BufferBindingType::Uniform)),
                entry(1, buffer(wgpu::BufferBindingType::Storage { read_only: true })),
                entry(2, buffer(wgpu::BufferBindingType::Storage { read_only: true })),
                entry(3, buffer(wgpu::BufferBindingType::Storage { read_only: false })),
            ],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("export diff"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/export_diff.wgsl").into()),
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("export diff"), bind_group_layouts: &[Some(&layout)], immediate_size: 0 });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("export diff"),
            layout: Some(&pl),
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        DiffPipeline { layout, pipeline }
    }
}

/// The ring of exportable buffers behind one scan-out.
pub struct ExportRing {
    gpu: Arc<Gpu>,
    label: String,
    bytes: u64,
    /// Grows, never shrinks: a slot's index is fixed for the ring's life.
    slots: RwLock<Vec<Slot>>,
    diff: DiffPipeline,
    info: ExportInfo,
    state: Mutex<State>,
}

impl ExportRing {
    /// `slots` buffers of `bytes` each on `gpu`, which must have been opened
    /// for export. The ring is registered with the device
    /// ([`Gpu::export_ring_of`]).
    pub fn new(gpu: &Arc<Gpu>, label: &str, bytes: u64, slots: usize) -> Result<Arc<ExportRing>, String> {
        let kind = gpu.export_handle_type().ok_or("export: the device was not opened for exportable memory")?;
        let first = sys::create(&gpu.device, &format!("{label} 0"), bytes, usage(), kind)?;
        let info = first.2.clone();
        let ring = Arc::new(ExportRing {
            gpu: Arc::clone(gpu),
            label: label.to_string(),
            bytes,
            slots: RwLock::new(vec![Slot { buffer: first.0, raw: Some(first.1), diff: Diff::new(&gpu.device) }]),
            diff: DiffPipeline::new(&gpu.device),
            info,
            state: Mutex::new(State { slots: vec![SlotState::default()], ..State::default() }),
        });
        ring.set_slots(slots)?;
        gpu.register_ring(&ring);
        Ok(ring)
    }

    fn allocate(&self, slots: &mut Vec<Slot>, st: &mut State) -> Result<usize, String> {
        // A freed index first: no consumer names it any more.
        let i = st.slots.iter().position(|s| s.freed).unwrap_or(slots.len());
        let (buffer, raw, _) = sys::create(&self.gpu.device, &format!("{} {i}", self.label), self.bytes, usage(), self.info.handle_type)?;
        if i == slots.len() {
            slots.push(Slot { buffer, raw: Some(raw), diff: Diff::new(&self.gpu.device) });
            st.slots.push(SlotState::default());
        } else {
            slots[i] = Slot { buffer, raw: Some(raw), diff: Diff::new(&self.gpu.device) };
            st.slots[i] = SlotState::default();
        }
        Ok(i)
    }

    /// Make the ring `n` live slots (at least 2, at most [`MAX_SLOTS`]; see
    /// [`slots_for`]). Allocates the buffers it lacks; for fewer, retires the
    /// highest ones (free ones first), whose memory [`reap`](Self::reap)
    /// frees once no consumer holds them.
    pub fn set_slots(&self, n: usize) -> Result<usize, String> {
        let n = n.clamp(2, MAX_SLOTS);
        let mut slots = self.slots.write();
        let mut st = self.state.lock();
        let live = |st: &State| st.slots.iter().filter(|s| !s.retired && !s.freed).count();
        while live(&st) < n {
            // Bring a retired slot nobody has freed yet back before allocating.
            if let Some(i) = st.slots.iter().position(|s| s.retired && !s.freed) {
                st.slots[i].retired = false;
            } else {
                self.allocate(&mut slots, &mut st)?;
            }
        }
        while live(&st) > n {
            let pick = (0..st.slots.len())
                .rev()
                .filter(|&i| !st.slots[i].retired && !st.slots[i].freed)
                .min_by_key(|&i| (st.slots[i].held != 0, st.newest == Some(i)))
                .expect("a live slot");
            st.slots[pick].retired = true;
        }
        drop(st);
        drop(slots);
        self.reap();
        Ok(n)
    }

    /// Retire slot `index` and allocate a new live one to take its place,
    /// while the others stay lent. Returns the new slot's index.
    pub fn replace(&self, index: usize) -> Result<usize, String> {
        let mut slots = self.slots.write();
        let mut st = self.state.lock();
        if st.slots.get(index).map_or(true, |s| s.freed) {
            return Err(format!("export: no slot {index}"));
        }
        let new = self.allocate(&mut slots, &mut st)?;
        st.slots[index].retired = true;
        if st.newest == Some(index) {
            st.newest = None;
        }
        drop(st);
        drop(slots);
        self.reap();
        Ok(new)
    }

    /// Free the memory of retired slots no consumer holds and no compose is
    /// writing. Returns how many. The slot's last compose has finished, so
    /// the GPU is done with it.
    pub fn reap(&self) -> usize {
        let mut slots = self.slots.write();
        let mut st = self.state.lock();
        let mut n = 0;
        for i in 0..st.slots.len() {
            let s = &st.slots[i];
            let finished = s.done.as_ref().map_or(true, |d| d.load(Ordering::Acquire));
            if s.retired && !s.freed && s.held == 0 && !s.writing && finished && st.newest != Some(i) {
                slots[i].buffer.destroy();
                if let Some(r) = slots[i].raw.take() {
                    r.free();
                }
                st.slots[i].freed = true;
                n += 1;
            }
        }
        n
    }

    /// Slots allocated, live or not: the largest index plus one.
    pub fn len(&self) -> usize {
        self.slots.read().len()
    }

    /// Slots the board writes now.
    pub fn active(&self) -> usize {
        self.state.lock().slots.iter().filter(|s| !s.retired && !s.freed).count()
    }

    pub fn info(&self) -> &ExportInfo {
        &self.info
    }

    /// The wgpu buffer of slot `index`, for iris's own presenters.
    pub fn buffer(&self, index: usize) -> wgpu::Buffer {
        self.slots.read()[index].buffer.clone()
    }

    /// The slot `buffer` is, if it belongs to this ring.
    pub fn index_of(&self, buffer: &wgpu::Buffer) -> Option<usize> {
        self.slots.read().iter().position(|s| s.raw.is_some() && &s.buffer == buffer)
    }

    /// A new handle for slot `index`'s memory. Each call returns a handle of
    /// its own, to pass to one consumer; the ring keeps the memory.
    pub fn handle(&self, index: usize) -> Result<ExportHandle, String> {
        self.slots.read().get(index).and_then(|s| s.raw.as_ref()).ok_or_else(|| format!("export: no slot {index}"))?.handle()
    }

    pub fn stats(&self) -> ExportStats {
        self.state.lock().stats
    }

    /// Take the newest frame for `consumer` (0 to 3), if there is one it does
    /// not hold already. The slot is not written until it releases it.
    pub fn acquire_newest(&self, consumer: usize) -> Option<ExportFrame> {
        assert!(consumer < MAX_CONSUMERS, "export: consumer {consumer} of at most {MAX_CONSUMERS}");
        let bit = 1u8 << consumer;
        let mut st = self.state.lock();
        let i = st.newest?;
        let s = &mut st.slots[i];
        if s.held & bit != 0 {
            return None;
        }
        s.held |= bit;
        let frame = ExportFrame {
            index: i,
            serial: s.serial,
            width: s.width,
            height: s.height,
            stride: s.stride,
            sample_ns: s.sample_ns,
            fourcc: FOURCC_XR24,
            consumer,
            submission: s.submission.clone().expect("a composed slot has a submission"),
            done: s.done.clone().expect("a composed slot has a completion flag"),
        };
        st.stats.acquired += 1;
        Some(frame)
    }

    /// Give slot `index` back for `consumer`, having finished with frames up
    /// to `serial`. Does nothing for a slot it does not hold or whose frame
    /// is newer than `serial` (released already, and reused).
    pub fn release(&self, consumer: usize, index: usize, serial: u64) {
        let bit = 1u8 << consumer;
        let mut st = self.state.lock();
        if let Some(s) = st.slots.get_mut(index) {
            if s.held & bit != 0 && s.serial <= serial {
                s.held &= !bit;
                st.stats.released += 1;
            }
        }
    }

    /// Block until the compose that wrote `frame` has finished on the GPU.
    /// After it, a consumer on another device may read the slot.
    pub fn wait_ready(&self, frame: &ExportFrame) -> Result<(), String> {
        self.gpu.wait(Some(frame.submission.clone()))
    }

    /// The slot the next compose writes, or None when every slot is held (the
    /// compose is then skipped). Marks it writing; pair with
    /// [`finish_compose`](Self::finish_compose).
    /// The slot the next compose writes and the newest frame before it (slot,
    /// width, height) to compare it with, if it is a different slot.
    pub(super) fn begin_compose(&self) -> Option<(usize, Option<(usize, u32, u32)>)> {
        self.reap();
        let mut st = self.state.lock();
        let free = |st: &State, i: usize| {
            let s = &st.slots[i];
            s.held == 0 && !s.writing && !s.retired && !s.freed
        };
        let pick = (0..st.slots.len())
            .find(|&i| free(&st, i) && st.newest != Some(i))
            .or_else(|| st.newest.filter(|&i| free(&st, i)));
        let Some(i) = pick else {
            st.stats.skipped += 1;
            return None;
        };
        // Overwriting the newest frame: there is none until this one lands.
        let prev = st.newest.filter(|&n| n != i).map(|n| (n, st.slots[n].width, st.slots[n].height));
        if st.newest == Some(i) {
            st.newest = None;
        }
        st.slots[i].writing = true;
        st.slots[i].sample_ns = now_ns();
        Some((i, prev))
    }

    pub(super) fn finish_compose(&self, index: usize, width: u32, height: u32, stride: u32, submission: wgpu::SubmissionIndex) {
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        self.gpu.on_done(move || flag.store(true, Ordering::Release));
        let mut st = self.state.lock();
        st.next_serial += 1;
        let serial = st.next_serial;
        let sample_ns = st.slots[index].sample_ns;
        st.slots[index] = SlotState { serial, width, height, submission: Some(submission), done: Some(done), stride, sample_ns, ..SlotState::default() };
        st.newest = Some(index);
        st.stats.composed += 1;
    }

    /// A command buffer that hands slot `index` to the consumers' side of the
    /// queue (`VK_QUEUE_FAMILY_EXTERNAL`) after the compose, submitted just
    /// behind it, with no acquire before the next write: the compose writes
    /// the whole frame, so what was there does not matter. wgpu does not let
    /// raw commands share an encoder with its own, so it is a command buffer
    /// of its own.
    pub(super) fn release_to_consumers(&self, index: usize) -> wgpu::CommandEncoder {
        let mut enc = self.gpu.encoder("export release");
        sys::transfer(&self.gpu.device, &mut enc, &self.slots.read()[index].buffer, true);
        enc
    }

    /// Record the comparison of slot `target` with slot `prev` (the frame
    /// before it, same size) into `enc`, after the compose.
    pub(super) fn record_diff(&self, enc: &mut wgpu::CommandEncoder, target: usize, prev: Option<usize>, width: u32, height: u32, stride_words: u32) {
        let slots = self.slots.read();
        let d = &slots[target].diff;
        self.gpu.write(&d.out, 0, bytemuck::bytes_of(&[u32::MAX, 0u32, prev.is_some() as u32, 0]));
        if let Some(p) = prev {
            self.gpu.write(&d.params, 0, bytemuck::bytes_of(&[width, height, stride_words, 0u32]));
            let group = self.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("export diff"),
                layout: &self.diff.layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: d.params.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: slots[target].buffer.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 2, resource: slots[p].buffer.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 3, resource: d.out.as_entire_binding() },
                ],
            });
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("export diff"), timestamp_writes: None });
            pass.set_pipeline(&self.diff.pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.dispatch_workgroups(width.div_ceil(8), height.div_ceil(8), 1);
        }
        enc.copy_buffer_to_buffer(&d.out, 0, &d.read, 0, 16);
    }

    /// The rows of `frame` that differ from the frame composed before it:
    /// `Some((first, past_last))`, or `None` when none do (a frame a consumer
    /// need not be sent). The whole frame when there was no frame before it
    /// or it was a different size. Waits for the compose, then reads sixteen
    /// bytes back.
    pub fn damage(&self, frame: &ExportFrame) -> Result<Option<(u32, u32)>, String> {
        self.wait_ready(frame)?;
        let slots = self.slots.read();
        let read = &slots[frame.index].diff.read;
        let (tx, rx) = std::sync::mpsc::channel();
        read.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let _ = self.gpu.device.poll(wgpu::PollType::wait_indefinitely());
        rx.recv().map_err(|e| e.to_string())?.map_err(|e| format!("export: read the change signal: {e}"))?;
        let w: Vec<u32> = bytemuck::cast_slice(&read.slice(..).get_mapped_range().expect("mapped")).to_vec();
        read.unmap();
        Ok(match (w[2], w[0]) {
            (0, _) => Some((0, frame.height)),
            (_, u32::MAX) => None,
            _ => Some((w[0], w[1].min(frame.height))),
        })
    }
}

/// CLOCK_MONOTONIC nanoseconds, the clock a consumer on the same host reads.
fn now_ns() -> u64 {
    #[cfg(unix)]
    {
        let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: a valid out pointer.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) };
        t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
    }
    #[cfg(not(unix))]
    {
        static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        T0.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64
    }
}

fn usage() -> wgpu::BufferUsages {
    wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC
}

impl Drop for ExportRing {
    fn drop(&mut self) {
        // The memory outlives every use of the buffers: wgpu releases a
        // buffer once the GPU is done with it, so wait for that.
        let slots = std::mem::take(&mut *self.slots.write());
        for s in &slots {
            s.buffer.destroy();
        }
        let _ = self.gpu.wait(None);
        for s in slots {
            if let Some(r) = s.raw {
                r.free();
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
mod sys {
    //! Vulkan memory allocated for export, shared as a file descriptor on
    //! Linux and an NT handle on Windows.
    use ash::vk;

    use super::{ExportHandle, ExportInfo, HandleType};

    #[cfg(target_os = "linux")]
    type Loader = ash::khr::external_memory_fd::Device;
    #[cfg(target_os = "windows")]
    type Loader = ash::khr::external_memory_win32::Device;

    pub struct RawExport {
        device: ash::Device,
        memory: vk::DeviceMemory,
        loader: Loader,
    }

    impl RawExport {
        pub fn handle(&self) -> Result<ExportHandle, String> {
            // SAFETY: the memory was allocated for export on this device and
            // is alive; each call returns a new handle that we own.
            #[cfg(target_os = "linux")]
            unsafe {
                use std::os::fd::{FromRawFd, OwnedFd};
                let info = vk::MemoryGetFdInfoKHR::default().memory(self.memory).handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
                let fd = self.loader.get_memory_fd(&info).map_err(|e| format!("export: vkGetMemoryFdKHR: {e}"))?;
                Ok(ExportHandle::Fd(OwnedFd::from_raw_fd(fd)))
            }
            #[cfg(target_os = "windows")]
            unsafe {
                use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};
                let info = vk::MemoryGetWin32HandleInfoKHR::default().memory(self.memory).handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32);
                let h = self.loader.get_memory_win32_handle(&info).map_err(|e| format!("export: vkGetMemoryWin32HandleKHR: {e}"))?;
                Ok(ExportHandle::Win32(OwnedHandle::from_raw_handle(h as RawHandle)))
            }
        }

        pub fn free(self) {
            // SAFETY: the buffers bound to it are destroyed and the GPU is
            // idle (ExportRing::drop).
            unsafe { self.device.free_memory(self.memory, None) };
        }
    }

    /// Record an ownership transfer of `buffer` into `enc`: to
    /// `VK_QUEUE_FAMILY_EXTERNAL` when `to_external` (after the board wrote
    /// it), else back to this queue's family (before it writes again). wgpu
    /// has no call for it, so it goes into the command buffer through
    /// wgpu-hal. Without these transfers a buffer another device reads has
    /// undefined contents even with correct memory dependencies.
    pub fn transfer(wdevice: &wgpu::Device, enc: &mut wgpu::CommandEncoder, buffer: &wgpu::Buffer, to_external: bool) {
        // SAFETY: the callback records one barrier into the open hal command
        // buffer and leaves it open; the wgpu encoder is not touched while it
        // runs.
        unsafe {
            let Some(hal_device) = wdevice.as_hal::<wgpu::hal::api::Vulkan>() else { return };
            let Some(hal_buffer) = buffer.as_hal::<wgpu::hal::api::Vulkan>() else { return };
            let (dev, family, raw) = (hal_device.raw_device().clone(), hal_device.queue_family_index(), hal_buffer.raw_handle());
            enc.as_hal_mut::<wgpu::hal::api::Vulkan, _, _>(|e| {
                let Some(e) = e else { return };
                let (src_q, dst_q, src_a, dst_a, src_s, dst_s) = if to_external {
                    (family, vk::QUEUE_FAMILY_EXTERNAL, vk::AccessFlags::SHADER_WRITE, vk::AccessFlags::empty(), vk::PipelineStageFlags::COMPUTE_SHADER, vk::PipelineStageFlags::BOTTOM_OF_PIPE)
                } else {
                    (vk::QUEUE_FAMILY_EXTERNAL, family, vk::AccessFlags::empty(), vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::SHADER_READ, vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::COMPUTE_SHADER)
                };
                let barrier = vk::BufferMemoryBarrier::default()
                    .src_access_mask(src_a)
                    .dst_access_mask(dst_a)
                    .src_queue_family_index(src_q)
                    .dst_queue_family_index(dst_q)
                    .buffer(raw)
                    .offset(0)
                    .size(vk::WHOLE_SIZE);
                dev.cmd_pipeline_barrier(e.raw_handle(), src_s, dst_s, vk::DependencyFlags::empty(), &[], &[barrier], &[]);
            });
        }
    }

    /// A buffer of `size` bytes in device-local memory allocated for export,
    /// wrapped as a wgpu buffer; the memory is the caller's to free once the
    /// buffer is destroyed and the GPU idle.
    pub fn create(
        wdevice: &wgpu::Device,
        label: &str,
        size: u64,
        usage: wgpu::BufferUsages,
        kind: HandleType,
    ) -> Result<(wgpu::Buffer, RawExport, ExportInfo), String> {
        let handle = match kind {
            HandleType::OpaqueFd => vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD,
            HandleType::OpaqueWin32 => vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32,
        };
        let flags = super::super::host::vk::usage_flags(usage);
        // SAFETY: the raw device and instance create, bind and export a
        // buffer of our own, which wgpu then wraps; a failure frees what was
        // made so far.
        unsafe {
            let hal_device = wdevice.as_hal::<wgpu::hal::api::Vulkan>().ok_or("export: not a Vulkan device")?;
            let device = hal_device.raw_device().clone();
            let instance = hal_device.shared_instance().raw_instance().clone();
            let pd = hal_device.raw_physical_device();

            let mut props = vk::ExternalBufferProperties::default();
            instance.get_physical_device_external_buffer_properties(
                pd,
                &vk::PhysicalDeviceExternalBufferInfo::default().usage(flags).handle_type(handle),
                &mut props,
            );
            if !props.external_memory_properties.external_memory_features.contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE) {
                return Err(format!("export: the adapter cannot export a storage buffer as {kind:?}"));
            }

            let mut ext_info = vk::ExternalMemoryBufferCreateInfo::default().handle_types(handle);
            let info = vk::BufferCreateInfo::default().size(size).usage(flags).sharing_mode(vk::SharingMode::EXCLUSIVE).push_next(&mut ext_info);
            let buffer = device.create_buffer(&info, None).map_err(|e| format!("export: vkCreateBuffer: {e}"))?;
            let req = device.get_buffer_memory_requirements(buffer);

            // Device-local memory the CPU cannot see if there is any: the
            // consumer's GPU reads it, nothing on the host does.
            let mem = instance.get_physical_device_memory_properties(pd);
            let types = &mem.memory_types[..mem.memory_type_count as usize];
            let fits = |i: usize| req.memory_type_bits & (1 << i) != 0;
            let local = |t: &vk::MemoryType| t.property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL);
            let visible = |t: &vk::MemoryType| t.property_flags.contains(vk::MemoryPropertyFlags::HOST_VISIBLE);
            let pick = types
                .iter()
                .enumerate()
                .find(|(i, t)| fits(*i) && local(t) && !visible(t))
                .or_else(|| types.iter().enumerate().find(|(i, t)| fits(*i) && local(t)))
                .or_else(|| types.iter().enumerate().find(|(i, _)| fits(*i)));
            let Some((index, _)) = pick else {
                device.destroy_buffer(buffer, None);
                return Err("export: no memory type fits the buffer".into());
            };

            // Dedicated, so the consumer imports exactly this buffer's memory
            // whatever the driver prefers.
            let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(handle);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(buffer);
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(index as u32)
                .push_next(&mut export)
                .push_next(&mut dedicated);
            let memory = match device.allocate_memory(&alloc, None) {
                Ok(m) => m,
                Err(e) => {
                    device.destroy_buffer(buffer, None);
                    return Err(format!("export: vkAllocateMemory: {e}"));
                }
            };
            if let Err(e) = device.bind_buffer_memory(buffer, memory, 0) {
                device.destroy_buffer(buffer, None);
                device.free_memory(memory, None);
                return Err(format!("export: vkBindBufferMemory: {e}"));
            }

            let mut id = vk::PhysicalDeviceIDProperties::default();
            let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
            instance.get_physical_device_properties2(pd, &mut p2);

            let loader = Loader::new(&instance, &device);
            drop(hal_device);
            let hal_buffer = wgpu::hal::vulkan::Buffer::from_raw(buffer);
            let wbuffer = wdevice.create_buffer_from_hal::<wgpu::hal::api::Vulkan>(
                hal_buffer,
                &wgpu::BufferDescriptor { label: Some(label), size, usage, mapped_at_creation: false },
            );
            let inf = ExportInfo {
                handle_type: kind,
                buffer_size: size,
                alloc_size: req.size,
                memory_type: index as u32,
                dedicated: true,
                usage: flags.as_raw(),
                create_flags: 0,
                device_uuid: id.device_uuid,
                driver_uuid: id.driver_uuid,
            };
            Ok((wbuffer, RawExport { device, memory, loader }, inf))
        }
    }
}

/// No exportable memory off Linux and Windows: `Gpu::export_handle_type` is
/// never Some, so no ring is made.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod sys {
    use super::{ExportHandle, ExportInfo, HandleType};

    pub struct RawExport;

    impl RawExport {
        pub fn handle(&self) -> Result<ExportHandle, String> {
            Err("export: not supported on this platform".into())
        }

        pub fn free(self) {}
    }

    pub fn transfer(_: &wgpu::Device, _: &mut wgpu::CommandEncoder, _: &wgpu::Buffer, _: bool) {}

    pub fn create(_: &wgpu::Device, _: &str, _: u64, _: wgpu::BufferUsages, _: HandleType) -> Result<(wgpu::Buffer, RawExport, ExportInfo), String> {
        Err("export: not supported on this platform".into())
    }
}
