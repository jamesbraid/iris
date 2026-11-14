//! Test-only: the resident raster's seam to the scan-out. The scan-out
//! itself (`direct.rs`) reads two plane buffers and knows nothing of the
//! resident raster's epochs and submission positions; this checks them
//! around it.

use std::sync::Arc;

use super::direct::{
    device_error, did_history_error, DispatchCount, Ng1Scanout, ScanoutError, UploadCount,
};
use super::resident::{
    Position, ResidentError, ResidentScanoutBindings, ResidentStatus,
};
use crate::dev::ng1::rex3::Rex3;
use crate::dev::ng1::screen::Ng1DisplaySource;
use crate::wgpu::Gpu;

impl From<ScanoutError> for ResidentError {
    fn from(error: ScanoutError) -> Self {
        match error {
            ScanoutError::Unsupported(reason) => ResidentError::Unsupported(reason),
            ScanoutError::Invalid(reason) => ResidentError::InvalidState(reason),
            ScanoutError::DeviceMismatch => ResidentError::DeviceMismatch,
            ScanoutError::DeviceLost(e) => ResidentError::DeviceLost(e),
            ScanoutError::Failed(e) => ResidentError::Failed(e),
        }
    }
}

fn scanout_error(error: String) -> ResidentError {
    device_error(error).into()
}

pub(super) fn validate_initial_did(src: &Ng1DisplaySource<'_>) -> Result<(), ResidentError> {
    did_history_error(src).map_or(Ok(()), |error| Err(error.into()))
}

/// The narrow resident seam: no raw buffer handles or CPU planes are exposed.
pub(crate) trait ResidentScanoutSource {
    fn scanout_bindings(&self, gpu: &Arc<Gpu>) -> Result<ResidentScanoutBindings, ResidentError>;
    fn scanout_status(&mut self) -> Result<ResidentStatus, ResidentError>;
}

impl ResidentScanoutSource for super::resident::ResidentRaster {
    fn scanout_bindings(&self, gpu: &Arc<Gpu>) -> Result<ResidentScanoutBindings, ResidentError> {
        super::resident::ResidentRaster::scanout_bindings(self, gpu)
    }

    fn scanout_status(&mut self) -> Result<ResidentStatus, ResidentError> {
        self.status()
    }
}

impl ResidentScanoutSource for &Rex3 {
    fn scanout_bindings(&self, gpu: &Arc<Gpu>) -> Result<ResidentScanoutBindings, ResidentError> {
        self.resident_scanout_bindings(gpu)
    }

    fn scanout_status(&mut self) -> Result<ResidentStatus, ResidentError> {
        self.resident_status()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CaptureCount {
    pub(crate) attempts: u64,
    pub(crate) attempted_bytes: u64,
    pub(crate) successes: u64,
    pub(crate) successful_bytes: u64,
}

/// Traffic caused by this compositor, not by raster initialization or
/// separately requested composed-frame capture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResidentScanoutTraffic {
    pub(crate) display: UploadCount,
    pub(crate) did: UploadCount,
    pub(crate) did_expansions: DispatchCount,
    pub(crate) raw_plane_uploads: UploadCount,
    pub(crate) raw_plane_readbacks: UploadCount,
    pub(crate) composed_readbacks: CaptureCount,
    pub(crate) frames_submitted: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResidentFrameStamp {
    serial: u64,
    pub(crate) position: Position,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// A same-device output. Its contents remain valid until this scanout
/// composes again; queue its consumer before releasing the borrow.
pub(crate) struct Ng1ResidentFrame<'a> {
    pub(crate) gpu: &'a Arc<Gpu>,
    pub(crate) buffer: &'a wgpu::Buffer,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) position: Position,
    stamp: ResidentFrameStamp,
}

impl Ng1ResidentFrame<'_> {
    pub(crate) fn stamp(&self) -> ResidentFrameStamp {
        self.stamp
    }
}

#[derive(Clone, Copy)]
pub(crate) enum CaptureFault {
    BeforeCopy,
    AfterMap,
}

/// The scan-out over the resident raster's planes, with the checks that
/// keep a frame from a stale epoch or an unsubmitted prefix.
pub(crate) struct Ng1ResidentScanout {
    scan: Ng1Scanout,
    bindings: ResidentScanoutBindings,
    last_frame: Option<ResidentFrameStamp>,
    composed_readbacks: CaptureCount,
    capture_scratch: Vec<u32>,
    capture_fault: Option<CaptureFault>,
}

impl Ng1ResidentScanout {
    /// Create a direct compositor from the resident raster's same-device,
    /// read-only binding seam. This does not submit raster work.
    pub(crate) fn new(
        gpu: &Arc<Gpu>,
        resident: &impl ResidentScanoutSource,
    ) -> Result<Self, ResidentError> {
        let bindings = resident.scanout_bindings(gpu)?;
        Ok(Self {
            scan: Ng1Scanout::build(gpu)?,
            bindings,
            last_frame: None,
            composed_readbacks: CaptureCount::default(),
            capture_scratch: Vec::new(),
            capture_fault: None,
        })
    }

    pub(crate) fn gpu(&self) -> &Arc<Gpu> {
        self.scan.gpu()
    }

    pub(crate) fn traffic(&self) -> ResidentScanoutTraffic {
        let t = self.scan.traffic();
        ResidentScanoutTraffic {
            display: t.display,
            did: t.did,
            did_expansions: t.did_expansions,
            raw_plane_uploads: UploadCount::default(),
            raw_plane_readbacks: UploadCount::default(),
            composed_readbacks: self.composed_readbacks,
            frames_submitted: t.frames_submitted,
        }
    }

    pub(super) fn did_for_test(&self) -> &wgpu::Buffer {
        self.scan.did_for_test()
    }

    pub(crate) fn fail_next_capture(&mut self, fault: CaptureFault) {
        self.capture_fault = Some(fault);
    }

    /// Capture only the still-current composed output. An older frame token
    /// cannot read a newer composition, even when its dimensions are equal.
    pub(crate) fn capture(
        &mut self,
        stamp: ResidentFrameStamp,
        resident: &mut impl ResidentScanoutSource,
        out: &mut [u32],
    ) -> Result<(), ResidentError> {
        if self.last_frame != Some(stamp) {
            return Err(ResidentError::InvalidState("resident composed frame is stale"));
        }
        let status = resident.scanout_status()?;
        if status.submitted.epoch != stamp.position.epoch || status.submitted.sequence < stamp.position.sequence {
            return Err(ResidentError::InvalidState("resident composed frame belongs to an old epoch"));
        }
        let pixels = stamp.width as usize * stamp.height as usize;
        if out.len() < pixels {
            return Err(ResidentError::InvalidState("resident capture output is too small"));
        }
        let bytes = pixels as u64 * 4;
        self.composed_readbacks.attempts += 1;
        self.composed_readbacks.attempted_bytes += bytes;
        let fault = self.capture_fault.take();
        if matches!(fault, Some(CaptureFault::BeforeCopy)) {
            return Err(ResidentError::Failed("injected composed capture failure before copy".into()));
        }
        self.capture_scratch.resize(pixels, 0);
        self.scan.read(stamp.width, stamp.height, &mut self.capture_scratch)
            .map_err(scanout_error)?;
        if matches!(fault, Some(CaptureFault::AfterMap)) {
            return Err(ResidentError::Failed("injected composed capture failure after map".into()));
        }
        self.scan.gpu().check().map_err(scanout_error)?;
        out[..pixels].copy_from_slice(&self.capture_scratch);
        self.composed_readbacks.successes += 1;
        self.composed_readbacks.successful_bytes += bytes;
        Ok(())
    }

    /// Compose the submitted resident prefix; this method never submits
    /// pending raster work. The caller must serialize its submission, this
    /// compose and the frame consumer against later raster submissions.
    /// Epoch changes require constructing a new scanout.
    pub(crate) fn compose(
        &mut self,
        src: &Ng1DisplaySource<'_>,
        resident: &mut impl ResidentScanoutSource,
    ) -> Result<Ng1ResidentFrame<'_>, ResidentError> {
        let status = resident.scanout_status()?;
        if status.submitted.epoch != self.bindings.epoch {
            return Err(ResidentError::InvalidState("resident scanout bindings belong to an old or different epoch"));
        }
        if status.accepted != status.submitted {
            return Err(ResidentError::InvalidState("resident scanout requires all accepted work to be submitted"));
        }
        self.scan.compose(src, &self.bindings.planes)?;
        let (width, height) = (src.width as u32, src.height as u32);
        let stamp = ResidentFrameStamp {
            serial: self.scan.frames(),
            position: status.submitted,
            width,
            height,
        };
        self.last_frame = Some(stamp);
        Ok(Ng1ResidentFrame {
            gpu: self.scan.gpu(),
            buffer: self.scan.output(),
            width,
            height,
            position: status.submitted,
            stamp,
        })
    }
}
