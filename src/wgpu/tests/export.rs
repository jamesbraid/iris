//! The ring of exportable scan-out buffers: the ownership rules, the
//! handles, and that composing into the ring gives the pixels composing into
//! one buffer does. They need a GPU (or lavapipe) that can export memory and
//! skip without one.

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};

use super::super::device::{Gpu, GpuOptions};
use super::super::export::{slots_for, ExportFrame, ExportHandle, ExportRing, DEFAULT_SLOTS, FOURCC_XR24, MAX_SLOTS};
use super::super::memory::Staging;
use super::super::scanout::{Scanout, ScanoutPipeline, ScanoutShaders};
use super::super::shader::Fragment;

const W: u32 = 37;
const H: u32 = 19;

/// A board whose display slot is one colour word: pixel (x, y) is the
/// colour plus x and y in the low bits.
const TINT: Fragment = Fragment::new(
    "tint.wgsl",
    "struct Slot { colour: u32 }
@group(1) @binding(0) var<storage, read> slot: Slot;
fn scanout_pixel(x: u32, y: u32) -> u32 { return slot.colour + (y << 8) + x; }",
);

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Tint {
    colour: u32,
}

fn gpu(export: bool) -> Option<Arc<Gpu>> {
    match Gpu::new(&GpuOptions { export_memory: export, ..GpuOptions::default() }) {
        Ok(g) if !export || g.export_handle_type().is_some() => Some(g),
        Ok(_) => {
            eprintln!("skipped: the adapter cannot export memory");
            None
        }
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

fn pipeline(gpu: &Gpu) -> ScanoutPipeline {
    ScanoutPipeline::new(gpu, &ScanoutShaders { label: "tint", body: &[TINT], library: &[], defines: &[], layouts: &[] }).unwrap_or_else(|e| panic!("{e}"))
}

fn expect(colour: u32) -> Vec<u32> {
    (0..H).flat_map(|y| (0..W).map(move |x| colour + (y << 8) + x)).collect()
}

/// The `W` x `H` frame in `buffer`, rows `stride` words apart.
fn read_at(gpu: &Gpu, buffer: &wgpu::Buffer, stride: u32) -> Vec<u32> {
    let bytes = Staging::default().read(gpu, buffer, &[(0, stride as u64 * H as u64 * 4)]).unwrap();
    let words: &[u32] = bytemuck::cast_slice(&bytes);
    (0..H as usize).flat_map(|y| words[y * stride as usize..][..W as usize].to_vec()).collect()
}

/// A buffer of a ring (rows padded to 256 bytes) or a single output.
fn read(gpu: &Gpu, buffer: &wgpu::Buffer) -> Vec<u32> {
    let ring = gpu.export_ring_of(buffer).is_some();
    read_at(gpu, buffer, if ring { W.next_multiple_of(64) } else { W })
}

fn compose(scan: &mut Scanout, pipe: &ScanoutPipeline, colour: u32) {
    scan.compose(pipe, W, H, bytemuck::bytes_of(&Tint { colour }), &[]).unwrap();
}

#[test]
fn without_the_option_there_is_no_ring() {
    let Some(gpu) = gpu(false) else { return };
    assert!(gpu.export_handle_type().is_none());
    let pipe = pipeline(&gpu);
    let mut scan = Scanout::new(&gpu, &pipe, 4096, 16);
    assert!(scan.export_ring().is_none());
    assert!(gpu.export_rings().is_empty());
    compose(&mut scan, &pipe, 0x0010_0000);
    assert_eq!(read(&gpu, scan.output()), expect(0x0010_0000));
}

#[test]
fn the_ring_composes_the_same_pixels() {
    let (Some(plain), Some(gpu)) = (gpu(false), gpu(true)) else { return };
    let (pp, ep) = (pipeline(&plain), pipeline(&gpu));
    let mut one = Scanout::new(&plain, &pp, 4096, 16);
    let mut ring = Scanout::new(&gpu, &ep, 4096, 16);
    assert!(ring.export_ring().is_some());
    for colour in [0x0001_0000, 0x0020_0000, 0x0300_0000, 0x0004_0000, 0x0050_0000, 0x0600_0000] {
        compose(&mut one, &pp, colour);
        compose(&mut ring, &ep, colour);
        assert_eq!(read(&gpu, ring.output()), read(&plain, one.output()), "colour {colour:#x}");
    }
    // The presenter's route finds the slot from the buffer it is handed.
    let (found, index) = gpu.export_ring_of(ring.output()).expect("the output is a ring slot");
    assert!(Arc::ptr_eq(&found, ring.export_ring().unwrap()));
    assert!(index < DEFAULT_SLOTS);
}

#[test]
fn a_held_slot_is_never_written() {
    let Some(gpu) = gpu(true) else { return };
    let pipe = pipeline(&gpu);
    let mut scan = Scanout::new(&gpu, &pipe, 4096, 16);
    let ring = scan.export_ring().unwrap().clone();

    compose(&mut scan, &pipe, 0x0010_0000);
    let held = ring.acquire_newest(0).expect("a frame to take");
    assert_eq!((held.width, held.height, held.stride, held.fourcc), (W, H, W.next_multiple_of(64) * 4, FOURCC_XR24));
    assert_eq!(held.stride % 256, 0, "rows are a multiple of 256 bytes apart");
    assert!(held.sample_ns > 0);
    assert!(ring.acquire_newest(0).is_none(), "a frame already held is not handed out twice");
    ring.wait_ready(&held).unwrap();

    // The board runs on, many frames past the ring's length.
    for n in 2..3 * DEFAULT_SLOTS as u32 {
        compose(&mut scan, &pipe, n << 20);
        assert_ne!(gpu.export_ring_of(scan.output()).unwrap().1, held.index);
    }
    assert_eq!(read(&gpu, &ring.buffer(held.index)), expect(0x0010_0000));

    // A late release of an older frame does not free it; its own does.
    ring.release(0, held.index, held.serial - 1);
    let next = ring.acquire_newest(0).expect("the newest frame");
    assert!(next.serial > held.serial);
    ring.release(0, held.index, held.serial);
    ring.release(0, next.index, next.serial);
    ring.release(0, next.index, next.serial);
    let st = ring.stats();
    assert_eq!((st.acquired, st.released), (2, 2));
}

#[test]
fn a_ring_held_whole_skips_the_compose() {
    let Some(gpu) = gpu(true) else { return };
    let pipe = pipeline(&gpu);
    let mut scan = Scanout::new(&gpu, &pipe, 4096, 16);
    let ring = scan.export_ring().unwrap().clone();

    let mut held = Vec::new();
    for n in 0..DEFAULT_SLOTS as u32 {
        compose(&mut scan, &pipe, (n + 1) << 20);
        held.push(ring.acquire_newest(0).expect("a frame to take"));
    }
    let before = ring.stats();
    let shown = scan.output().clone();
    compose(&mut scan, &pipe, 0x0700_0000);
    assert_eq!(ring.stats().skipped, before.skipped + 1);
    assert_eq!(ring.stats().composed, before.composed, "nothing was composed");
    assert_eq!(scan.output(), &shown, "the output stays on the last frame");
    gpu.check().unwrap();
    for (n, f) in held.iter().enumerate() {
        assert_eq!(read(&gpu, &ring.buffer(f.index)), expect((n as u32 + 1) << 20), "slot {}", f.index);
    }

    // One comes back and the next compose goes into it.
    ring.release(0, held[1].index, held[1].serial);
    compose(&mut scan, &pipe, 0x0700_0000);
    assert_eq!(gpu.export_ring_of(scan.output()).unwrap().1, held[1].index);
    assert_eq!(read(&gpu, scan.output()), expect(0x0700_0000));
}

#[test]
fn every_handle_is_a_descriptor_of_its_own() {
    let Some(gpu) = gpu(true) else { return };
    let ring = ExportRing::new(&gpu, "test ring", 1 << 16, 2).unwrap();
    let info = ring.info();
    assert!(info.alloc_size >= info.buffer_size && info.buffer_size == 1 << 16);
    assert_ne!(info.device_uuid, [0; 16]);
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let (ExportHandle::Fd(x), ExportHandle::Fd(y)) = (ring.handle(0).unwrap(), ring.handle(0).unwrap());
        assert_ne!(x.as_raw_fd(), y.as_raw_fd());
        for fd in [&x, &y] {
            // SAFETY: fcntl on a descriptor we own.
            assert!(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } >= 0);
        }
    }
    #[cfg(windows)]
    assert!(ring.handle(0).is_ok());
    assert!(ring.handle(2).is_err());
}

#[test]
fn slot_counts_follow_the_consumers() {
    assert_eq!(slots_for(&[]), 2);
    assert_eq!(slots_for(&[1]), 3);
    assert_eq!(slots_for(&[2]), 4);
    assert_eq!(slots_for(&[1, 2, 2]), 7);
    assert_eq!(slots_for(&[2, 2, 2, 2]), 10);
    assert_eq!(slots_for(&[2; 9]), 10, "no more than four consumers count");
    assert!(slots_for(&[9, 9]) <= MAX_SLOTS);
}

#[test]
fn the_owner_resizes_the_ring_while_it_runs() {
    let Some(gpu) = gpu(true) else { return };
    let pipe = pipeline(&gpu);
    let mut scan = Scanout::new(&gpu, &pipe, 4096, 16);
    let ring = scan.export_ring().unwrap().clone();
    assert_eq!((ring.len(), ring.active()), (DEFAULT_SLOTS, DEFAULT_SLOTS));
    compose(&mut scan, &pipe, 0x0010_0000);
    let held = ring.acquire_newest(0).unwrap();

    // Two showing consumers arrive: 2 + 2 + 2 slots, composed into at once.
    assert_eq!(scan.set_export_slots(slots_for(&[2, 2])).unwrap(), 6);
    assert_eq!(ring.len(), 6);
    let mut used = std::collections::BTreeSet::new();
    // Consumer 1 keeps four frames at a time, as a viewer that shows its
    // slot and is slow to let go does.
    let mut keeping = Vec::new();
    for n in 2..40u32 {
        compose(&mut scan, &pipe, n << 20);
        used.insert(gpu.export_ring_of(scan.output()).unwrap().1);
        let f = ring.acquire_newest(1).unwrap();
        ring.wait_ready(&f).unwrap();
        assert!(f.is_ready() || { gpu.poll(); f.is_ready() });
        keeping.push(f);
        if keeping.len() > 4 {
            let old = keeping.remove(0);
            ring.release(1, old.index, old.serial);
        }
    }
    for f in keeping {
        ring.release(1, f.index, f.serial);
    }
    assert!(used.iter().any(|&i| i >= DEFAULT_SLOTS), "the new slots are composed into: {used:?}");
    assert!(!used.contains(&held.index), "the held slot is still never written");
    assert_eq!(read(&gpu, &ring.buffer(held.index)), expect(0x0010_0000));
    assert!(ring.handle(5).is_ok(), "a new slot exports like the first ones");

    // Shrinking retires the high slots; a consumer's hold on one stays
    // good, and its memory is freed only after the hold ends.
    let high = ring.acquire_newest(2).unwrap();
    scan.set_export_slots(3).unwrap();
    for n in 40..60u32 {
        compose(&mut scan, &pipe, n << 20);
        let used = gpu.export_ring_of(scan.output()).unwrap().1;
        assert!(ring.active() == 3 && used != high.index || high.index < 3);
    }
    assert_eq!(read(&gpu, &ring.buffer(high.index)), expect(high_colour(&ring, &high)));
    ring.release(2, high.index, high.serial);
    ring.reap();
    compose(&mut scan, &pipe, 61 << 20);
    assert!(ring.handle(5).is_err() || high.index < 3, "a retired slot's memory is freed once released");
}

fn high_colour(_: &ExportRing, f: &ExportFrame) -> u32 {
    // The frames above are composed in order: serial n holds colour (n + 1) << 20 for n >= 2.
    if f.serial == 1 { 0x0010_0000 } else { (f.serial as u32) << 20 }
}

#[test]
fn a_slot_can_be_replaced_while_the_others_are_lent() {
    let Some(gpu) = gpu(true) else { return };
    let pipe = pipeline(&gpu);
    let mut scan = Scanout::new(&gpu, &pipe, 4096, 16);
    let ring = scan.export_ring().unwrap().clone();
    compose(&mut scan, &pipe, 0x0010_0000);
    let lent = ring.acquire_newest(0).unwrap();
    let other = (0..ring.len()).find(|&i| i != lent.index).unwrap();
    let new = ring.replace(other).unwrap();
    assert_ne!(new, other);
    assert!(ring.handle(new).is_ok());
    assert!(ring.handle(other).is_err(), "the old slot's memory is gone");
    for n in 2..12u32 {
        compose(&mut scan, &pipe, n << 20);
        assert_ne!(gpu.export_ring_of(scan.output()).unwrap().1, lent.index);
    }
    assert_eq!(read(&gpu, &ring.buffer(lent.index)), expect(0x0010_0000), "the lent slot is untouched");
    assert_eq!(ring.active(), DEFAULT_SLOTS);
}

#[test]
fn consumers_hold_a_slot_independently() {
    let Some(gpu) = gpu(true) else { return };
    let pipe = pipeline(&gpu);
    let mut scan = Scanout::new(&gpu, &pipe, 4096, 16);
    let ring = scan.export_ring().unwrap().clone();
    compose(&mut scan, &pipe, 0x0010_0000);
    let (a, b) = (ring.acquire_newest(0).unwrap(), ring.acquire_newest(1).unwrap());
    assert_eq!((a.index, a.serial), (b.index, b.serial));
    ring.release(0, a.index, a.serial);
    // Consumer 1 still holds it: it is not written.
    for n in 2..10u32 {
        compose(&mut scan, &pipe, n << 20);
        assert_ne!(gpu.export_ring_of(scan.output()).unwrap().1, b.index);
    }
    assert_eq!(read(&gpu, &ring.buffer(b.index)), expect(0x0010_0000));
    ring.release(1, b.index, b.serial);
    assert_eq!(ring.stats().released, 2);
}

#[test]
fn the_change_signal_skips_a_frame_that_did_not_change() {
    let Some(gpu) = gpu(true) else { return };
    let pipe = pipeline(&gpu);
    let mut scan = Scanout::new(&gpu, &pipe, 4096, 16);
    let ring = scan.export_ring().unwrap().clone();
    let mut last = |scan: &mut Scanout, colour| {
        compose(scan, &pipe, colour);
        let f = ring.acquire_newest(0).unwrap();
        let d = ring.damage(&f).unwrap();
        ring.release(0, f.index, f.serial);
        d
    };
    assert_eq!(last(&mut scan, 0x0010_0000), Some((0, H)), "the first frame is whole");
    assert_eq!(last(&mut scan, 0x0010_0000), None, "the same picture again");
    assert_eq!(last(&mut scan, 0x0010_0000), None);
    assert_eq!(last(&mut scan, 0x0020_0000), Some((0, H)));
    assert_eq!(last(&mut scan, 0x0020_0000), None);
}

#[test]
fn a_readback_is_recorded_with_the_compose() {
    let Some(gpu) = gpu(true) else { return };
    let pipe = pipeline(&gpu);
    let mut scan = Scanout::new(&gpu, &pipe, 4096, 16);
    let stride = scan.out_stride(W);
    let dst = gpu.buffer("readback", stride as u64 * H as u64 * 4, wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC);
    scan.set_export_readback(Some(dst.clone()));
    compose(&mut scan, &pipe, 0x0030_0000);
    assert_eq!(read_at(&gpu, &dst, stride), expect(0x0030_0000));
}
