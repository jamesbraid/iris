//! Newport's scan-out on the GPU: the one direct kernel over two plane
//! buffers, whoever draws into them.
//!
//! The CPU raster uploads its planes (whole, or the row bands the damage
//! tracker names) into an [`Ng1Planes`]; the test-only resident raster
//! draws into its own. Either way the window-id map is expanded on the GPU
//! from the VC2 table, the display state is rewritten only for what changed
//! (a cursor move writes 16 bytes), and the frame lands in the scan-out
//! driver's output buffer, 0xAABBGGRR, for a renderer on the same device.

use std::sync::Arc;

use super::{cursor_header, Ng1Slot, SCANOUT, VC2_WORDS};
use crate::dev::ng1::rex3_dirty::{DamagePlane, DamageRect};
use crate::dev::ng1::screen::{Ng1DisplayGenerations, Ng1DisplaySource};
use crate::dev::ng1::vc2::{
    VC2_CTRL_BLACKOUT, VC2_CTRL_DID_EN, VC2_CTRL_VIDEO_TIMING_EN, VC2_REG_CURRENT_CURSOR_X,
    VC2_REG_DID_ENTRY_PTR, VC2_REG_DISPLAY_CONTROL, VC2_REG_SCANLINE_LEN,
};
use crate::disp::FB_STRIDE;
use crate::wgpu::{Fragment, Gpu, Scanout, ScanoutPipeline, ScanoutShaders, Source};

const HEIGHT: usize = 1024;
/// A plane: 2048 x 1024 words.
const PLANE_WORDS: usize = FB_STRIDE * HEIGHT;
const DID_BYTES: usize = FB_STRIDE * HEIGHT;
const MAX_PIXELS: u32 = (FB_STRIDE * HEIGHT) as u32;
const BINDINGS: Fragment = Fragment::new("ng1_bindings.wgsl", include_str!("shaders/scanout_bindings.wgsl"));
const DID_EXPAND: Fragment = Fragment::new("ng1_did_expand.wgsl", include_str!("shaders/did_expand.wgsl"));

/// Why a frame could not be composed. The kinds stay apart so a caller can
/// tell a device that is gone from state it was handed wrongly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScanoutError {
    Unsupported(&'static str),
    Invalid(&'static str),
    DeviceMismatch,
    DeviceLost(String),
    Failed(String),
}

impl std::fmt::Display for ScanoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(reason) => write!(f, "unsupported: {reason}"),
            Self::Invalid(reason) => write!(f, "invalid state: {reason}"),
            Self::DeviceMismatch => write!(f, "state belongs to another device"),
            Self::DeviceLost(e) | Self::Failed(e) => f.write_str(e),
        }
    }
}

pub(super) fn device_error(error: String) -> ScanoutError {
    if error.starts_with("wgpu device lost (") {
        ScanoutError::DeviceLost(error)
    } else {
        ScanoutError::Failed(error)
    }
}

/// The two plane buffers the kernel reads, 2048 x 1024 words each. Whoever
/// replaces them bumps `generation`, so the scan-out binds the new ones.
/// They must be on the scan-out's device.
#[derive(Clone)]
pub struct Ng1Planes {
    pub rgb: wgpu::Buffer,
    pub aux: wgpu::Buffer,
    pub generation: u64,
}

impl Ng1Planes {
    /// Empty planes for a CPU raster to upload into.
    pub fn new(gpu: &Gpu) -> Ng1Planes {
        let usage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
        Ng1Planes {
            rgb: gpu.buffer("ng1 rgb plane", 4 * PLANE_WORDS as u64, usage),
            aux: gpu.buffer("ng1 aux plane", 4 * PLANE_WORDS as u64, usage),
            generation: 0,
        }
    }

    fn target(&self, plane: DamagePlane) -> &wgpu::Buffer {
        match plane {
            DamagePlane::Rgb => &self.rgb,
            DamagePlane::Aux => &self.aux,
        }
    }

    /// Bring the planes up to the CPU's: the rows `damage` names (RGB's
    /// rectangles, then the aux plane's), or both whole planes when there is
    /// no damage report. Returns the bytes written.
    pub(crate) fn sync(&self, gpu: &Gpu, rgb: &[u32], aux: &[u32], damage: Option<(&[DamageRect], &[DamageRect])>) -> u64 {
        match damage {
            None => self.write_all(gpu, DamagePlane::Rgb, rgb) + self.write_all(gpu, DamagePlane::Aux, aux),
            Some((r, a)) => self.write_bands(gpu, DamagePlane::Rgb, rgb, r) + self.write_bands(gpu, DamagePlane::Aux, aux, a),
        }
    }

    /// The whole of `words`, a CPU plane. Returns the bytes written.
    pub(crate) fn write_all(&self, gpu: &Gpu, plane: DamagePlane, words: &[u32]) -> u64 {
        let n = words.len().min(PLANE_WORDS) & !1;
        gpu.write(self.target(plane), 0, bytemuck::cast_slice(&words[..n]));
        4 * n as u64
    }

    /// The rows any of `rects` touches, whole rows, one write to a run of
    /// touching ones: a write per rectangle row costs more than the bytes
    /// it saves once there are a few hundred of them. Returns the bytes
    /// written.
    pub(crate) fn write_bands(&self, gpu: &Gpu, plane: DamagePlane, words: &[u32], rects: &[DamageRect]) -> u64 {
        let mut bands: Vec<(usize, usize)> = rects.iter().map(|r| (r.y, r.y + r.height)).collect();
        bands.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (a, b) in bands {
            match merged.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        let rows = (words.len() / FB_STRIDE).min(HEIGHT);
        let mut bytes = 0;
        for (a, b) in merged {
            let b = b.min(rows);
            if a < b {
                let band = &words[a * FB_STRIDE..b * FB_STRIDE];
                gpu.write(self.target(plane), 4 * (a * FB_STRIDE) as u64, bytemuck::cast_slice(band));
                bytes += 4 * band.len() as u64;
            }
        }
        bytes
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct UploadCount {
    pub(crate) attempts: u64,
    pub(crate) attempted_bytes: u64,
    pub(crate) submitted: u64,
    pub(crate) submitted_bytes: u64,
}

impl UploadCount {
    fn attempt(&mut self, count: u64, bytes: u64) {
        self.attempts += count;
        self.attempted_bytes += bytes;
    }

    fn submit(&mut self, count: u64, bytes: u64) {
        self.submitted += count;
        self.submitted_bytes += bytes;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DispatchCount {
    pub(crate) attempts: u64,
    pub(crate) submitted: u64,
}

/// Traffic of the display state, not the planes. `did` counts decoded-map
/// uploads and must remain zero; `did_expansions` counts GPU expansion
/// passes. Submitted counts mean queued with the frame, not GPU completion.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ScanoutTraffic {
    pub(crate) display: UploadCount,
    pub(crate) did: UploadCount,
    pub(crate) did_expansions: DispatchCount,
    pub(crate) frames_submitted: u64,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct DidParams {
    did_ptr: u32,
    scan_len: u32,
    width: u32,
    height: u32,
    enabled: u32,
    display_on: u32,
    pad: [u32; 2],
}

/// The display's window-id history, kept across scan-out replacement: rows
/// the VC2 table stops covering keep what they last held, as the CPU's
/// decoded map does.
pub(crate) struct ResidentDid {
    gpu: Arc<Gpu>,
    buffer: wgpu::Buffer,
    initialized: bool,
}

/// Why a fresh, zeroed GPU map cannot stand in for the history of `src`:
/// the CPU's map must hold nothing outside what the table decodes.
pub(super) fn did_history_error(src: &Ng1DisplaySource<'_>) -> Option<ScanoutError> {
    let control = src.vc2_regs[VC2_REG_DISPLAY_CONTROL as usize];
    let active = control & (VC2_CTRL_BLACKOUT | VC2_CTRL_VIDEO_TIMING_EN)
        == (VC2_CTRL_BLACKOUT | VC2_CTRL_VIDEO_TIMING_EN);
    if active && control & VC2_CTRL_DID_EN == 0 {
        return None;
    }

    let Some(did) = src.did.get(..DID_BYTES) else {
        return Some(ScanoutError::Invalid("DID history is unavailable"));
    };
    // A zero-initialized GPU map can retain only zeroes outside this decode.
    let rows = if active {
        let table = src.vc2_regs[VC2_REG_DID_ENTRY_PTR as usize] as usize;
        (0..src.height).take_while(|&y| {
            src.vc2_ram.get(table + y).is_some_and(|&line| line != u16::MAX)
        }).count()
    } else {
        0
    };
    let scan_len = (src.vc2_regs[VC2_REG_SCANLINE_LEN as usize] >> 5) as usize;
    let written = if scan_len > 0 && scan_len <= FB_STRIDE { scan_len } else { src.width };
    for y in 0..HEIGHT {
        let start = y * FB_STRIDE + if y < rows { written } else { 0 };
        if did[start..(y + 1) * FB_STRIDE].iter().any(|&value| value != 0) {
            return Some(ScanoutError::Unsupported("initial DID history requires a seed"));
        }
    }
    None
}

struct DidBindings {
    did: wgpu::Buffer,
    expand: wgpu::BindGroup,
    display: wgpu::BindGroup,
}

/// Newport's display on the GPU. It owns the pipeline, the output buffer and
/// the copies of the display state the kernel reads; the planes are the
/// caller's.
pub struct Ng1Scanout {
    pipeline: ScanoutPipeline,
    scanout: Scanout,
    planes_layout: wgpu::BindGroupLayout,
    display_layout: wgpu::BindGroupLayout,
    did_layout: wgpu::BindGroupLayout,
    /// The planes' bind group and the generation of the planes it binds.
    planes_group: Option<(u64, wgpu::BindGroup)>,
    /// The window-id buffer and its bindings, and the display source they are for.
    bound: Option<(u64, DidBindings)>,
    vc2: wgpu::Buffer,
    did_params: wgpu::Buffer,
    did_pipeline: wgpu::ComputePipeline,
    uploaded: Option<Ng1DisplayGenerations>,
    dimensions: Option<(u32, u32)>,
    traffic: ScanoutTraffic,
}

impl Ng1Scanout {
    pub fn new(gpu: &Arc<Gpu>) -> Result<Ng1Scanout, String> {
        Self::build(gpu).map_err(|e| e.to_string())
    }

    pub(super) fn build(gpu: &Arc<Gpu>) -> Result<Ng1Scanout, ScanoutError> {
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer { ty, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        };
        let read = wgpu::BufferBindingType::Storage { read_only: true };
        let planes_layout = gpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ng1 scanout planes"),
            entries: &[entry(0, read), entry(1, read)],
        });
        let display_layout = gpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ng1 display state"),
            entries: &[entry(0, read), entry(1, read)],
        });
        let pipeline = ScanoutPipeline::new(gpu, &ScanoutShaders {
            label: "ng1",
            body: &[SCANOUT],
            library: &[BINDINGS],
            defines: &[],
            layouts: &[&planes_layout, &display_layout],
        }).map_err(device_error)?;
        let scanout = Scanout::new(gpu, &pipeline, MAX_PIXELS, size_of::<Ng1Slot>() as u64);
        let usage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
        let vc2 = gpu.buffer("ng1 vc2 ram", (2 * VC2_WORDS) as u64, usage);
        let did_params = gpu.buffer(
            "ng1 DID parameters",
            size_of::<DidParams>() as u64,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );
        let did_layout = gpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ng1 DID expansion"),
            entries: &[
                entry(0, read),
                entry(1, wgpu::BufferBindingType::Storage { read_only: false }),
                entry(2, wgpu::BufferBindingType::Uniform),
            ],
        });
        let did_pipeline = Source::new("ng1 DID expansion")
            .body(DID_EXPAND)
            .compute_pipeline(&gpu.device, "expand_did", &[&did_layout])
            .map_err(device_error)?;
        gpu.check().map_err(device_error)?;
        Ok(Ng1Scanout {
            pipeline, scanout, planes_layout, display_layout, did_layout, planes_group: None,
            bound: None, vc2, did_params, did_pipeline, uploaded: None, dimensions: None, traffic: ScanoutTraffic::default(),
        })
    }

    /// The device it runs on.
    pub fn gpu(&self) -> &Arc<Gpu> {
        self.scanout.gpu()
    }

    pub(crate) fn traffic(&self) -> ScanoutTraffic {
        self.traffic
    }

    #[cfg(test)]
    pub(super) fn did_for_test(&self) -> &wgpu::Buffer {
        &self.bound.as_ref().expect("DID buffer is bound after composition").1.did
    }

    /// The buffer the last frame was composed into: 0xAABBGGRR, rows of
    /// that frame's width.
    pub fn output(&self) -> &wgpu::Buffer {
        self.scanout.output()
    }

    /// Frames composed so far.
    pub fn frames(&self) -> u64 {
        self.scanout.frames
    }

    /// Read the last composed frame back into `out` (0xAABBGGRR, rows of
    /// `width`), for a consumer that wants words.
    pub fn read(&mut self, width: u32, height: u32, out: &mut [u32]) -> Result<(), String> {
        self.scanout.read_output(width, height, out)
    }

    /// Seed the GPU's window-id history from the CPU's decoded map, for a
    /// board whose earlier frames were composed on the CPU. A no-op once the
    /// history is the GPU's own.
    pub(crate) fn seed_did(&self, src: &Ng1DisplaySource<'_>) -> Result<(), ScanoutError> {
        let mut state = src.resident_did.lock();
        if state.as_ref().is_some_and(|did| did.initialized) {
            return Ok(());
        }
        let did = self.did_state(&mut state)?;
        let map = src.did.get(..DID_BYTES).ok_or(ScanoutError::Invalid("DID history is unavailable"))?;
        self.gpu().write(&did.buffer, 0, map);
        did.initialized = true;
        Ok(())
    }

    /// The display's window-id buffer, made on first use and kept on one device.
    fn did_state<'a>(&self, state: &'a mut Option<ResidentDid>) -> Result<&'a mut ResidentDid, ScanoutError> {
        let gpu = self.scanout.gpu();
        if let Some(did) = state.as_ref() {
            if !Arc::ptr_eq(&did.gpu, gpu) {
                return Err(ScanoutError::DeviceMismatch);
            }
        } else {
            let buffer = gpu.buffer(
                "ng1 decoded DID",
                DID_BYTES as u64,
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            );
            gpu.check().map_err(device_error)?;
            *state = Some(ResidentDid { gpu: Arc::clone(gpu), buffer, initialized: false });
        }
        state.as_mut().ok_or(ScanoutError::Invalid("DID state is missing"))
    }

    fn expand_did(&self, group: &wgpu::BindGroup, params: &DidParams) -> Result<(), ScanoutError> {
        let gpu = self.scanout.gpu();
        gpu.write(&self.did_params, 0, bytemuck::bytes_of(params));
        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ng1 DID expansion"),
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("ng1 DID expansion"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.did_pipeline);
            pass.set_bind_group(0, group, &[]);
            pass.dispatch_workgroups((HEIGHT as u32).div_ceil(64), 1, 1);
        }
        gpu.submit(encoder);
        gpu.check().map_err(device_error)
    }

    /// Compose the frame of `src` over `planes` and submit it, with no
    /// readback; the buffer it is in, for a renderer on this device.
    ///
    /// Only what changed since the last frame goes up: the VC2 RAM, the
    /// window-id expansion and the display tables each follow their own
    /// revision, and a cursor move alone writes its 16-byte header.
    pub(crate) fn compose(&mut self, src: &Ng1DisplaySource<'_>, planes: &Ng1Planes) -> Result<wgpu::Buffer, ScanoutError> {
        if src.width == 0 || src.width > FB_STRIDE || src.height == 0 || src.height > HEIGHT {
            return Err(ScanoutError::Invalid("scanout dimensions exceed the Newport planes"));
        }
        let cursor_x = src.vc2_regs[VC2_REG_CURRENT_CURSOR_X as usize] as i64
            - 31 + src.cursor_x_adjust as i64;
        if src.vc2_ram.len() != VC2_WORDS || src.topscan.checked_add(1).is_none()
            || !(i32::MIN as i64..=i32::MAX as i64).contains(&cursor_x)
        {
            return Err(ScanoutError::Invalid("scanout display state is incomplete"));
        }
        self.scanout.gpu().check().map_err(device_error)?;

        let generation = src.generations;
        let mut did_state = src.resident_did.lock();
        if did_state.as_ref().is_none_or(|did| !did.initialized) {
            if let Some(error) = did_history_error(src) {
                return Err(error);
            }
        }
        let resident = self.did_state(&mut *did_state)?;
        if self.bound.as_ref().is_none_or(|(source, _)| *source != generation.source) {
            let gpu = self.scanout.gpu();
            let did = resident.buffer.clone();
            let expand = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("ng1 DID expansion"),
                layout: &self.did_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: self.vc2.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: did.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 2, resource: self.did_params.as_entire_binding() },
                ],
            });
            let display = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("ng1 display state"),
                layout: &self.display_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: did.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: self.vc2.as_entire_binding() },
                ],
            });
            gpu.check().map_err(device_error)?;
            self.bound = Some((generation.source, DidBindings { did, expand, display }));
        }
        if self.planes_group.as_ref().is_none_or(|(g, _)| *g != planes.generation) {
            let gpu = self.scanout.gpu();
            let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("ng1 scanout planes"),
                layout: &self.planes_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: planes.rgb.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: planes.aux.as_entire_binding() },
                ],
            });
            gpu.check().map_err(device_error)?;
            self.planes_group = Some((planes.generation, group));
        }
        let previous = self.uploaded;
        let changed = |select: fn(Ng1DisplayGenerations) -> u64| {
            previous.is_none_or(|old| old.source != generation.source || select(old) != select(generation))
        };
        let vc2_changed = changed(|g| g.vc2_ram);
        let vc2_regs_changed = changed(|g| g.vc2_regs);
        let geometry_changed = changed(|g| g.geometry);
        let dimensions = (src.width as u32, src.height as u32);
        let header_changed = self.dimensions != Some(dimensions);
        let did_changed = vc2_changed || vc2_regs_changed || header_changed;
        let slot_changed = vc2_regs_changed
            || changed(|g| g.xmap)
            || changed(|g| g.cmap)
            || changed(|g| g.ramdac)
            || geometry_changed;
        // Nothing else moved: the cursor's header is the slot's first 16 bytes.
        let cursor_only = !slot_changed && changed(|g| g.cursor);
        let slot = slot_changed.then(|| Ng1Slot::of_display(src));
        let cursor = cursor_only.then(|| cursor_header(src.vc2_regs, src.cursor_x_adjust));

        let control = src.vc2_regs[VC2_REG_DISPLAY_CONTROL as usize];
        let did_params = DidParams {
            did_ptr: src.vc2_regs[VC2_REG_DID_ENTRY_PTR as usize] as u32,
            scan_len: (src.vc2_regs[VC2_REG_SCANLINE_LEN as usize] >> 5) as u32,
            width: dimensions.0,
            height: dimensions.1,
            enabled: u32::from(control & VC2_CTRL_DID_EN != 0),
            display_on: u32::from(
                control & (VC2_CTRL_BLACKOUT | VC2_CTRL_VIDEO_TIMING_EN)
                    == (VC2_CTRL_BLACKOUT | VC2_CTRL_VIDEO_TIMING_EN),
            ),
            pad: [0; 2],
        };
        let expansion_uploads = vc2_changed as u64 + did_changed as u64;
        let expansion_bytes = if vc2_changed { (2 * VC2_WORDS) as u64 } else { 0 }
            + if did_changed { size_of::<DidParams>() as u64 } else { 0 };
        let slot_len = if slot_changed { size_of::<Ng1Slot>() as u64 } else if cursor_only { 16 } else { 0 };
        let scanout_uploads = (slot_changed || cursor_only) as u64 + header_changed as u64;
        let scanout_bytes = slot_len + if header_changed { 16 } else { 0 };
        self.traffic.display.attempt(
            expansion_uploads + scanout_uploads,
            expansion_bytes + scanout_bytes,
        );
        if vc2_changed {
            self.scanout.gpu().write(&self.vc2, 0, bytemuck::cast_slice(src.vc2_ram));
        }
        if did_changed {
            self.traffic.did_expansions.attempts += 1;
            let Some((_, bound)) = &self.bound else { unreachable!("bound above") };
            self.expand_did(&bound.expand, &did_params)?;
            if let Some(state) = did_state.as_mut() {
                state.initialized = true;
            }
            self.traffic.display.submit(expansion_uploads, expansion_bytes);
            self.traffic.did_expansions.submitted += 1;
        }
        let before_submission = self.scanout.frames;
        let (Some((_, bound)), Some((_, planes_group))) = (&self.bound, &self.planes_group) else { unreachable!("bound above") };
        let display_group = &bound.display;
        let update: Option<&[u8]> = match (&slot, &cursor) {
            (Some(slot), _) => Some(bytemuck::bytes_of(slot)),
            (None, Some(cursor)) => Some(bytemuck::bytes_of(cursor)),
            (None, None) => None,
        };
        let result = self.scanout.compose_cached(
            &self.pipeline,
            dimensions.0,
            dimensions.1,
            update,
            &[planes_group, display_group],
        );
        if self.scanout.frames != before_submission {
            self.traffic.display.submit(scanout_uploads, scanout_bytes);
            self.traffic.frames_submitted += 1;
        }
        result.map_err(device_error)?;

        self.uploaded = Some(generation);
        self.dimensions = Some(dimensions);
        Ok(self.scanout.output().clone())
    }
}
