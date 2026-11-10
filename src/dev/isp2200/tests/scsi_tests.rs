//! Commands to the disks on the loop, and the recovery commands that cancel
//! them.
//!
//! Ported from tests/qtest/qlogic-isp2200-test.c in
//! jamesbraid/qemu-sgi-origami.

use super::loop_tests::BRINGUP_NS;
use super::*;
use crate::scsi::{ScsiResponse, ScsiTarget};
use parking_lot::Condvar;
use std::io;
use std::sync::atomic::AtomicU32;
use std::time::{Duration, Instant};

const COMPLETION_COMPLETE: u16 = IOCB_COMPLETE;
const COMPLETION_UNDERRUN: u16 = IOCB_DATA_UNDERRUN;
const COMPLETION_ABORTED: u16 = IOCB_ABORTED;
const COMPLETION_RESET: u16 = IOCB_RESET;
const SCSI_CHECK_CONDITION: u16 = 0x02;
const ICB_OPT_PORT_DB: u16 = ICB_OPT_PORT_DB_EVENTS;
const BUF: u32 = 0x10000;

#[derive(Debug, Default)]
struct ScsiResult {
    scsi_status: u16,
    completion: u16,
    sense_len: u16,
    residual: u32,
    sense: Vec<u8>,
}

fn decode_status(e: &Entry, handle: u32) -> ScsiResult {
    assert_eq!(g32(e, 4), handle);
    // State and status flags: unknown meaning, written as zero.
    assert_eq!((g16(e, 12), g16(e, 14)), (0, 0));
    ScsiResult {
        scsi_status: g16(e, 8),
        completion: g16(e, 10),
        sense_len: g16(e, 18),
        residual: g32(e, 20),
        sense: e[32..64].to_vec(),
    }
}

const CDB_TUR: [u8; 6] = [0; 6];

fn cdb_inquiry(evpd: bool, page: u8, alloc_len: u16) -> Vec<u8> {
    let mut cdb = vec![0u8; 6];
    cdb[0] = 0x12;
    cdb[1] = u8::from(evpd);
    cdb[2] = page;
    cdb[3..5].copy_from_slice(&alloc_len.to_be_bytes());
    cdb
}

fn cdb_rw10(op: u8, lba: u32, blocks: u16) -> Vec<u8> {
    let mut cdb = vec![0u8; 10];
    cdb[0] = op;
    cdb[2..6].copy_from_slice(&lba.to_be_bytes());
    cdb[7..9].copy_from_slice(&blocks.to_be_bytes());
    cdb
}

impl Rig {
    /// A 64-bit command with at most two segments. `control` is the direction
    /// flag; the segments split `len` at `split`.
    fn scsi_entry(&self, handle: u32, loop_id: u8, lun: u16, cdb: &[u8], control: u16, buf: u64, len: u32, split: u32) -> Entry {
        let mut e = entry_command(ENTRY_CMD64, handle, loop_id, lun, cdb[0]);
        e[20..20 + cdb.len()].copy_from_slice(cdb);
        p16(&mut e, 12, control);
        p32(&mut e, 36, len);
        if len != 0 && split != 0 && split < len {
            p16(&mut e, 18, 2);
            put_seg64(&mut e, 40, buf, split);
            put_seg64(&mut e, 52, buf + u64::from(split), len - split);
        } else if len != 0 {
            p16(&mut e, 18, 1);
            put_seg64(&mut e, 40, buf, len);
        }
        e
    }

    /// One status entry: the adapter's thread has run the command by the time
    /// a register write returns, so the entry is there to read.
    fn wait_response(&self) -> Entry {
        assert_eq!(self.rd(reg::ISR) & ISR_RISC, ISR_RISC);
        assert_eq!(self.rd(reg::SEMAPHORE), 0);
        assert_eq!(self.rd(reg::RESPONSE_QUEUE), (self.rs_next.get() + 1) % self.rslen.get());
        let e = self.response(self.rs_next.get());
        assert_eq!(e[0], ENTRY_STATUS);
        self.consume(1);
        e
    }

    #[allow(clippy::too_many_arguments)]
    fn scsi(&self, handle: u32, loop_id: u8, lun: u16, cdb: &[u8], control: u16, buf: u64, len: u32, split: u32) -> ScsiResult {
        self.publish(&self.scsi_entry(handle, loop_id, lun, cdb, control, buf, len, split));
        decode_status(&self.wait_response(), handle)
    }

    fn tur_to(&self, handle: u32, loop_id: u8, lun: u16) -> ScsiResult {
        self.scsi(handle, loop_id, lun, &CDB_TUR, 0, 0, 0, 0)
    }

    /// The sense key a TEST UNIT READY draws, or 0 for good.
    fn tur_sense(&self, loop_id: u8, lun: u16) -> u8 {
        let r = self.tur_to(0x7e00, loop_id, lun);
        assert_eq!(r.completion, COMPLETION_COMPLETE);
        if r.scsi_status == 0 {
            return 0;
        }
        assert_eq!(r.scsi_status, SCSI_CHECK_CONDITION | SCSI_STATUS_SENSE_VALID);
        r.sense[2] & 0x0f
    }
}

/// INQUIRY reaches the disk at its loop ID, and a LUN the port lacks is
/// refused as the SCSI layer below refuses it.
#[test]
fn scsi_inquiry() {
    let r = Rig::with_disks("inquiry", &[5]);
    r.loop_up(ICB_OPT_PORT_DB, 0);
    let res = r.scsi(0x1201, 5, 0, &cdb_inquiry(false, 0, 36), 0x20, u64::from(BUF), 36, 0);
    assert_eq!((res.completion, res.scsi_status, res.residual, res.sense_len), (COMPLETION_COMPLETE, 0, 0, 0));
    let data = r.mem.get(BUF, 36);
    assert_eq!(data[0], 0x00);
    assert_eq!(&data[8..16], b"SGI     ");
}

/// WRITE(10) then READ(10) of the same eight blocks, with the segments split
/// differently each way: the data goes to the disk and comes back the same.
#[test]
fn scsi_read_write() {
    let r = Rig::with_disks("rw", &[5]);
    r.loop_up(ICB_OPT_PORT_DB, 0);
    let len = 8 * 512;
    let (out, back) = (0x10000u32, 0x20000u32);
    let pattern: Vec<u8> = (0..len).map(|i| (i * 7 + (i >> 9) + 1) as u8).collect();
    r.mem.put(out, &pattern);

    let res = r.scsi(0x2a2a, 5, 0, &cdb_rw10(0x2a, 16, 8), 0x40, u64::from(out), len as u32, 1536);
    assert_eq!((res.completion, res.scsi_status, res.residual), (COMPLETION_COMPLETE, 0, 0));

    // Read back through OpenBSD's segment-list form, three pieces.
    let mut list = [0u8; 36];
    put_seg64(&mut list, 0, u64::from(back), 1024);
    put_seg64(&mut list, 12, u64::from(back) + 1024, 1024);
    put_seg64(&mut list, 24, u64::from(back) + 2048, 2048);
    r.mem.put(0x6000, &list);
    let mut e = r.scsi_entry(0x2828, 5, 0, &cdb_rw10(0x28, 16, 8), 0x20, 0, len as u32, 0);
    e[0] = ENTRY_CMD_LIST;
    p16(&mut e, 18, 3);
    p16(&mut e, 40, 1);
    p64(&mut e, 46, 0x6000);
    r.publish(&e);
    let res = decode_status(&r.wait_response(), 0x2828);
    assert_eq!((res.completion, res.scsi_status), (COMPLETION_COMPLETE, 0));
    assert_eq!(r.mem.get(back, len), pattern);
}

/// Commands go to the loop ID they name: the disks tell themselves apart by
/// what each holds.
#[test]
fn scsi_addressing() {
    let r = Rig::with_disks("addressing", &[5, 1]);
    r.loop_up(ICB_OPT_PORT_DB, 0);
    r.mem.put(BUF, &[0x11; 512]);
    let res = r.scsi(0x8000, 5, 0, &cdb_rw10(0x2a, 3, 1), 0x40, u64::from(BUF), 512, 0);
    assert_eq!(res.scsi_status, 0);
    r.mem.put(BUF + 0x1000, &[0; 512]);
    r.scsi(0x8001, 5, 0, &cdb_rw10(0x28, 3, 1), 0x20, u64::from(BUF + 0x1000), 512, 0);
    assert_eq!(r.mem.get(BUF + 0x1000, 512), vec![0x11; 512]);
    r.scsi(0x8002, 1, 0, &cdb_rw10(0x28, 3, 1), 0x20, u64::from(BUF + 0x1000), 512, 0);
    assert_eq!(r.mem.get(BUF + 0x1000, 512), vec![0; 512]);
}

/// Approximation: completion precedence. A failed SCSI status completes
/// 0x0000 with the status, sense valid and the sense bytes, and with
/// residual-under and the residual when the transfer was short. A good short
/// transfer is underrun 0x0015 with the residual; a good full one is 0x0000
/// with no flags.
#[test]
fn scsi_check_condition() {
    let r = Rig::with_disks("cc", &[1]);
    r.loop_up(ICB_OPT_PORT_DB, 0);
    let buf = u64::from(BUF);

    // A READ to a LUN the port lacks moves nothing: all 512 are residual.
    let res = r.scsi(0x0603, 1, 7, &cdb_rw10(0x28, 0, 1), 0x20, buf, 512, 0);
    assert_eq!(res.completion, COMPLETION_COMPLETE);
    assert_eq!(res.scsi_status, SCSI_CHECK_CONDITION | SCSI_STATUS_SENSE_VALID | SCSI_STATUS_RESID_UNDER);
    assert_eq!((res.sense[2] & 0x0f, res.sense[12]), (0x05, 0x25));
    assert_eq!(res.residual, 512);

    // INQUIRY to that LUN: peripheral qualifier 3, no device.
    r.mem.put(BUF, &[0; 36]);
    let res = r.scsi(0x0604, 1, 7, &cdb_inquiry(false, 0, 36), 0x20, buf, 36, 0);
    assert_eq!(res.scsi_status & 0xff, 0);
    assert_eq!(r.mem.get(BUF, 1)[0] & 0xe0, 0x60);

    // A LUN past 255: CHECK CONDITION, the same sense, nothing moved.
    let res = r.scsi(0x0605, 1, 300, &cdb_rw10(0x28, 0, 1), 0x20, buf, 512, 0);
    assert_eq!(res.scsi_status, SCSI_CHECK_CONDITION | SCSI_STATUS_SENSE_VALID | SCSI_STATUS_RESID_UNDER);
    assert_eq!((res.sense[2] & 0x0f, res.sense[12], res.residual), (0x05, 0x25, 512));

    // A good transfer shorter than the host's buffer is an underrun.
    let res = r.scsi(0x0606, 1, 0, &cdb_inquiry(false, 0, 36), 0x20, buf, 255, 0);
    assert_eq!(res.completion, COMPLETION_UNDERRUN);
    assert_eq!(res.scsi_status, SCSI_STATUS_RESID_UNDER);
    assert_eq!((res.residual, res.sense_len), (255 - 36, 0));

    // So is one block read into two blocks' worth of segments.
    let res = r.scsi(0x0608, 1, 0, &cdb_rw10(0x28, 0, 1), 0x20, buf, 1024, 512);
    assert_eq!((res.completion, res.scsi_status, res.residual), (COMPLETION_UNDERRUN, SCSI_STATUS_RESID_UNDER, 512));

    // And a good full READ has no flags at all.
    let res = r.scsi(0x0609, 1, 0, &cdb_rw10(0x28, 0, 1), 0x20, buf, 512, 256);
    assert_eq!((res.completion, res.scsi_status, res.residual), (COMPLETION_COMPLETE, 0, 0));
}

/// Approximation: a command to a loop ID with no port, to the adapter's own
/// loop ID or past 125 completes "port unavailable" with no SCSI status. A
/// marker between commands answers nothing.
#[test]
fn scsi_unavailable() {
    let r = Rig::with_disks("unavailable", &[1, 5]);
    r.loop_up(ICB_OPT_PORT_DB, 0);
    for (i, id) in [2u8, 0, 126, 255].into_iter().enumerate() {
        let res = r.tur_to(0x2800 + i as u32, id, 0);
        assert_eq!((res.completion, res.scsi_status, res.residual), (COMPLETION_PORT_UNAVAILABLE, 0, 0));

        let mut e = [0u8; ENTRY_SIZE];
        e[0] = ENTRY_MARKER;
        e[1] = 1;
        e[9] = id;
        e[10] = 1;
        r.publish(&e);
        assert_eq!(r.rd(reg::REQUEST_QUEUE), r.rq_next.get());
        r.expect_no_event();
    }
}

/// The first command each disk takes after a soft reset reports the reset as
/// a unit attention, once. A bus reset the guest did not ask for does not.
#[test]
fn scsi_unit_attention_after_soft_reset() {
    let r = Rig::with_disks("ua", &[1]);
    r.loop_up(ICB_OPT_PORT_DB, 0);
    assert_eq!(r.tur_sense(1, 0), 0);
    r.soft_reset();
    r.wr(reg::HCCR, HCCR_RELEASE);
    r.loop_up(ICB_OPT_PORT_DB, 0);
    assert_eq!(r.tur_sense(1, 0), 0x06);
    assert_eq!(r.tur_sense(1, 0), 0);
}

/// A device that holds a READ until the test lets it go, counting how many
/// it has taken and finished.
struct Gate {
    open: Mutex<bool>,
    wake: Condvar,
    started: AtomicU32,
    finished: AtomicU32,
}

impl Gate {
    fn new() -> Arc<Gate> {
        Arc::new(Gate { open: Mutex::new(false), wake: Condvar::new(), started: AtomicU32::new(0), finished: AtomicU32::new(0) })
    }

    fn release(&self) {
        *self.open.lock() = true;
        self.wake.notify_all();
    }

    fn close(&self) {
        *self.open.lock() = false;
    }

    fn wait_for(&self, counter: &AtomicU32, n: u32) {
        let end = Instant::now() + Duration::from_secs(10);
        while counter.load(Ordering::Acquire) < n {
            assert!(Instant::now() < end, "the device did not get there");
            std::thread::yield_now();
        }
    }
}

struct SlowDisk {
    gate: Arc<Gate>,
    attention: bool,
}

impl ScsiTarget for SlowDisk {
    fn request(&mut self, req: &ScsiRequest) -> io::Result<ScsiResponse> {
        if req.cdb[0] == scsi_cmd::REQUEST_SENSE {
            let mut sense = vec![0u8; 18];
            sense[0] = 0x70;
            sense[2] = 0x06;
            sense[12] = 0x29;
            return Ok(ScsiResponse { status: 0, data: sense });
        }
        if std::mem::take(&mut self.attention) {
            return Ok(ScsiResponse { status: CHECK_CONDITION, data: Vec::new() });
        }
        if req.cdb[0] != scsi_cmd::READ_10 {
            return Ok(ScsiResponse { status: 0, data: Vec::new() });
        }
        self.gate.started.fetch_add(1, Ordering::AcqRel);
        let mut open = self.gate.open.lock();
        while !*open {
            self.gate.wake.wait(&mut open);
        }
        drop(open);
        self.gate.finished.fetch_add(1, Ordering::AcqRel);
        Ok(ScsiResponse { status: 0, data: vec![0x5a; 512] })
    }

    fn bus_reset(&mut self) {
        self.attention = true;
    }
}

impl Rig {
    /// An adapter with a slow disk at each of `ids`, behind one gate.
    fn with_slow_disks(ids: &[u8]) -> (Rig, Arc<Gate>) {
        let r = Rig::bare();
        let gate = Gate::new();
        for &id in ids {
            r.isp.bus().attach(id, Box::new(SlowDisk { gate: gate.clone(), attention: false })).unwrap();
        }
        (r, gate)
    }

    /// Publish a READ(10) and wait until the slow disk has it. The adapter's
    /// thread is then busy until the gate opens.
    fn submit_read(&self, gate: &Gate, handle: u32, loop_id: u8, lun: u16, buf: u32) {
        gate.close();
        let before = gate.started.load(Ordering::Acquire);
        let e = self.scsi_entry(handle, loop_id, lun, &cdb_rw10(0x28, 0, 1), 0x20, u64::from(buf), 512, 0);
        self.put_request(self.rq_next.get(), &e);
        self.rq_next.set((self.rq_next.get() + 1) % self.rqlen.get());
        self.isp.write(reg::REQUEST_QUEUE, u32::from(self.rq_next.get()), 2);
        gate.wait_for(&gate.started, before + 1);
        self.busy.set(true);
    }

    /// Open the gate and wait for the ring thread to finish what it held.
    fn release(&self, gate: &Gate, finished: u32) {
        gate.release();
        gate.wait_for(&gate.finished, finished);
        self.busy.set(false);
        self.isp.wait_idle();
    }

    fn abort(&self, loop_id: u8, lun: u16, handle: u32) -> u16 {
        self.mbox(&[mbox::ABORT_COMMAND, u16::from(loop_id) << 8, handle as u16, (handle >> 16) as u16, 0, 0, lun])
    }

    /// The posted mailbox result's code, released.
    fn take_result(&self) -> u16 {
        let code = self.rd(mb(0));
        self.ack();
        code
    }

    /// Exactly these entries, handle and completion, in order, as a driver
    /// walking the ring reads them.
    fn collect(&self, want: &[(u32, u16)]) {
        let mut seen = 0;
        while seen < want.len() {
            assert_eq!(self.rd(reg::ISR) & ISR_RISC, ISR_RISC, "entry {seen} of {want:?}");
            let (rslen, next) = (self.rslen.get(), self.rs_next.get());
            let n = (self.rd(reg::RESPONSE_QUEUE) + rslen - next) % rslen;
            assert!(n > 0 && seen + usize::from(n) <= want.len(), "{n} entries after {seen} of {want:?}");
            for i in 0..n {
                let e = self.response((next + i) % rslen);
                assert_eq!((g32(&e, 4), g16(&e, 10)), want[seen + usize::from(i)]);
            }
            self.consume(n);
            seen += usize::from(n);
        }
        assert_eq!(self.rd(reg::ISR), 0);
    }

    fn assert_untouched(&self, addr: u32, len: usize, fill: u8) {
        assert_eq!(self.mem.get(addr, len), vec![fill; len]);
    }
}

/// Retiring a command in flight: the mailbox result posts first, then the
/// command's own entry, with the completion the recovery owes.
#[test]
fn abort_command() {
    let (r, gate) = Rig::with_slow_disks(&[1, 5]);
    r.loop_up(ICB_OPT_PORT_DB, 0);
    r.mem.put(BUF, &[0xa5; 1024]);
    // Entry bytes 00 00 00 05, then 05 00 00 00.
    r.submit_read(&gate, 0x00000005, 5, 0, BUF);
    r.expect_no_event();

    // The right handle at the wrong LUN or loop ID matches nothing.
    assert_eq!(r.abort(5, 1, 5), mbox::COMMAND_ERROR);
    r.ack();
    assert_eq!(r.abort(1, 0, 5), mbox::COMMAND_ERROR);
    r.ack();
    assert_eq!(r.abort(5, 0, 0x0005_0000), mbox::COMMAND_ERROR);
    r.ack();

    // The mailbox result posts before any entry.
    assert_eq!(r.abort(5, 0, 5), mbox::COMPLETE);
    assert_eq!(r.take_result(), mbox::COMPLETE);
    r.collect(&[(5, COMPLETION_ABORTED)]);

    // Gone now: a second abort finds nothing, and when the disk lets go
    // nothing reaches the ring and no data lands.
    assert_eq!(r.abort(5, 0, 5), mbox::COMMAND_ERROR);
    r.ack();
    r.release(&gate, 1);
    r.assert_untouched(BUF, 1024, 0xa5);
    r.expect_no_event();
}

/// Approximation: abort target retires the command running at the loop ID
/// with "reset" and resets the device there; LUN reset does the same for the
/// one LUN the device has. Each leaves the other ports alone, and the reset
/// device reports it as a unit attention.
#[test]
fn target_and_lun_reset() {
    let (r, gate) = Rig::with_slow_disks(&[1, 5]);
    r.loop_up(ICB_OPT_PORT_DB, 0);

    r.submit_read(&gate, 0xa0, 5, 0, BUF);
    // Behind it in the ring, to another port.
    let e = r.scsi_entry(0xa2, 1, 0, &cdb_rw10(0x28, 0, 1), 0x20, u64::from(BUF + 512), 512, 0);
    r.publish(&e);
    assert_eq!(r.call(mbox::ABORT_TARGET, 5 << 8, 0, 0), mbox::COMPLETE);
    assert_eq!(r.take_result(), mbox::COMPLETE);
    r.collect(&[(0xa0, COMPLETION_RESET)]);
    r.release(&gate, 2);
    r.collect(&[(0xa2, COMPLETION_COMPLETE)]);
    assert_eq!(r.tur_sense(5, 0), 0x06);
    assert_eq!(r.tur_sense(1, 0), 0);

    r.submit_read(&gate, 0xb1, 5, 0, BUF);
    assert_eq!(r.call(mbox::LUN_RESET, 5 << 8, 0, 0), mbox::COMPLETE);
    assert_eq!(r.take_result(), mbox::COMPLETE);
    r.collect(&[(0xb1, COMPLETION_RESET)]);
    r.release(&gate, 3);
    assert_eq!(r.tur_sense(5, 0), 0x06);

    // An empty loop ID: nothing to retire, still complete.
    assert_eq!(r.call_ack(mbox::ABORT_TARGET, 7 << 8, 0, 0), mbox::COMPLETE);
    r.expect_no_event();
}

/// LIP reset retires the named loop ID's command, or any port's for 0xff,
/// resets those devices and brings the loop up again. Approximation: with
/// set-firmware-options bit 1 clear it posts loop up and port database
/// changed but no LIP event; with it set, LIP reset first.
#[test]
fn lip_reset() {
    let (r, gate) = Rig::with_slow_disks(&[1, 5]);
    r.start();
    assert_eq!(r.call_ack(mbox::LIP_RESET, 0xff00, 0, 0), mbox::COMMAND_ERROR);
    r.loop_up(ICB_OPT_PORT_DB, 0);

    r.submit_read(&gate, 0xc5, 5, 0, BUF);
    let e = r.scsi_entry(0xc1, 1, 0, &cdb_rw10(0x28, 0, 1), 0x20, u64::from(BUF + 512), 512, 0);
    r.publish(&e);
    assert_eq!(r.call(mbox::LIP_RESET, 5 << 8, 0, 0), mbox::COMPLETE);
    assert_eq!(r.take_result(), mbox::COMPLETE);
    r.collect(&[(0xc5, COMPLETION_RESET)]);
    r.release(&gate, 2);
    r.collect(&[(0xc1, COMPLETION_COMPLETE)]);
    assert_eq!(r.fw_state(), FW_STATE_WAIT_AL_PA);
    r.step(BRINGUP_NS);
    r.expect_events(&[event::LOOP_UP, event::PORT_DB_CHANGED]);
    assert_eq!(r.tur_sense(5, 0), 0x06);
    assert_eq!(r.tur_sense(1, 0), 0);

    assert_eq!(r.call_ack(mbox::SET_FW_OPTIONS, 0x0002, 0, 0), mbox::COMPLETE);
    assert_eq!(r.call_ack(mbox::LIP_RESET, 0xff00, 0, 0), mbox::COMPLETE);
    r.step(BRINGUP_NS);
    r.expect_events(&[event::LIP_RESET, event::LOOP_UP, event::PORT_DB_CHANGED]);
    assert_eq!(r.tur_sense(5, 0), 0x06);
    assert_eq!(r.tur_sense(1, 0), 0x06);
}

/// A soft reset cancels the command in flight without a status and resets
/// the loop's devices: once the disk's read is over, no data has reached the
/// host's buffer, no entry has reached the ring and ISR is clear.
#[test]
fn soft_reset_cancels() {
    let (r, gate) = Rig::with_slow_disks(&[1, 5]);
    r.loop_up(ICB_OPT_PORT_DB, 0);
    r.mem.put(BUF, &[0xa5; 1024]);
    r.submit_read(&gate, 0xe5, 5, 0, BUF);
    let e = r.scsi_entry(0xe1, 1, 0, &cdb_rw10(0x28, 0, 1), 0x20, u64::from(BUF + 512), 512, 0);
    r.publish(&e);
    r.soft_reset();
    assert_eq!(r.rd(reg::ISR), 0);
    r.wr(reg::HCCR, HCCR_RELEASE);
    r.release(&gate, 1);
    assert_eq!(r.rd(reg::ISR), 0);
    r.assert_untouched(BUF, 1024, 0xa5);

    r.loop_up(ICB_OPT_PORT_DB, 0);
    assert_eq!(r.tur_sense(5, 0), 0x06);
    assert_eq!(r.tur_sense(1, 0), 0x06);
    assert_eq!(r.tur_sense(1, 0), 0);
}

/// An HCCR RISC reset cancels every command in flight without a status, as a
/// soft reset does, but leaves the loop's devices alone: none reports a unit
/// attention afterwards.
#[test]
fn hccr_reset_cancels() {
    let (r, gate) = Rig::with_slow_disks(&[1, 5]);
    r.loop_up(ICB_OPT_PORT_DB, 0);
    r.mem.put(BUF, &[0xa5; 1024]);
    let slot = r.response(r.rs_next.get());
    r.submit_read(&gate, 0xe5, 5, 0, BUF);
    r.wr(reg::HCCR, HCCR_RESET);
    assert_eq!(r.rd(reg::ISR), 0);
    r.wr(reg::HCCR, HCCR_RELEASE);
    r.release(&gate, 1);
    assert_eq!(r.rd(reg::ISR), 0);
    r.assert_untouched(BUF, 1024, 0xa5);
    assert_eq!(r.response(r.rs_next.get()), slot);

    r.loop_up(ICB_OPT_PORT_DB, 0);
    assert_eq!(r.tur_sense(5, 0), 0);
    assert_eq!(r.tur_sense(1, 0), 0);
}

/// Initialise-firmware again with a command in flight cancels it without a
/// status. Once its disk's read is over, no data has reached the host's
/// buffer, the ring has no entry for it, ISR is clear, and the ring's first
/// entry is the next command's.
#[test]
fn init_firmware_cancels() {
    let (r, gate) = Rig::with_slow_disks(&[1, 5]);
    r.loop_up(ICB_OPT_PORT_DB, 0);
    r.mem.put(BUF, &[0xa5; 512]);
    r.submit_read(&gate, 0xf5, 5, 0, BUF);
    r.rings_with(16, 16, ICB_OPT_PORT_DB, 0, 0);
    r.step(BRINGUP_NS);
    r.expect_events(&[event::LIP_OCCURRED, event::LOOP_UP, event::PORT_DB_CHANGED]);
    r.release(&gate, 1);
    assert_eq!(r.rd(reg::ISR), 0);
    assert_eq!(r.rd(reg::RESPONSE_QUEUE), 0);
    r.assert_untouched(BUF, 512, 0xa5);
    assert_eq!(r.response(0), [0u8; ENTRY_SIZE]);

    // The new ring's first entry is this command's.
    r.tur_sense(1, 0);
    let e = r.response(0);
    assert_eq!((e[0], g32(&e, 4)), (ENTRY_STATUS, 0x7e00));
}

/// A data DMA the host bridge refuses completes the command 0x0002 (DMA
/// error) with no SCSI status, whether the adapter is delivering read data or
/// fetching write data, and the port keeps working.
#[test]
fn scsi_dma_error() {
    let r = Rig::with_disks("dma", &[5]);
    r.loop_up(ICB_OPT_PORT_DB, 0);
    let beyond = 1u64 << 40;
    let res = r.scsi(0xd0a0, 5, 0, &cdb_rw10(0x28, 0, 1), 0x20, beyond, 512, 0);
    assert_eq!((res.completion, res.scsi_status), (0x0002, 0));
    // The whole command's data, from an unmapped buffer.
    let res = r.scsi(0xd0a1, 5, 0, &cdb_rw10(0x2a, 0, 1), 0x40, beyond, 512, 0);
    assert_eq!((res.completion, res.scsi_status), (0x0002, 0));
    // A buffer shorter than the command's eight blocks, unmapped too.
    let res = r.scsi(0xd0a2, 5, 0, &cdb_rw10(0x2a, 0, 8), 0x40, beyond, 512, 0);
    assert_eq!((res.completion, res.scsi_status), (0x0002, 0));
    assert_eq!(r.tur_sense(5, 0), 0);
}
