//! The board's architectural state, encoded and restored apart from board
//! memory: what `Odyssey`'s snapshot parts and the engine's `save` and `load`
//! both build on.

use super::board::{Board, BoardSnapshot};
use super::display_fifo::DisplayFifoSnapshot;
use super::interface::HostInterfaceSnapshot;
use super::mmio::BoardLocks;
use super::reader::DisplayRegsSnapshot;
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;

pub(super) const ODYSSEY_SNAPSHOT_VERSION: u32 = 1;
pub(super) const ODYSSEY_STATE_MAX_BYTES: usize = 16 << 20;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OdysseyMetadata {
    version: u32,
    board: String,
    pub(super) memory_bytes: u64,
    /// The shell holds board memory and saves it as RAM.
    #[serde(default)]
    lent: bool,
}

#[derive(Serialize, Deserialize)]
struct OdysseyState {
    version: u32,
    board: BoardSnapshot,
    front: HostInterfaceSnapshot,
    display_fifo: DisplayFifoSnapshot,
    display_regs: DisplayRegsSnapshot,
    line_phase: u32,
}

fn decode_state(bytes: &[u8]) -> Result<OdysseyState, String> {
    if bytes.len() > ODYSSEY_STATE_MAX_BYTES {
        return Err(format!("VPro state is {} bytes, maximum is {ODYSSEY_STATE_MAX_BYTES}", bytes.len()));
    }
    let (state, rest) = postcard::take_from_bytes(bytes).map_err(|e| format!("decode VPro state: {e}"))?;
    if !rest.is_empty() {
        return Err(format!("VPro state has {} trailing bytes", rest.len()));
    }
    Ok(state)
}

impl BoardLocks {
    /// The metadata text and the encoded state, without board memory. The
    /// board and raster workers must already be stopped.
    pub(super) fn snapshot_state(&self, lent: bool) -> Result<(String, Vec<u8>), String> {
        if self.fifo.level() != 0 {
            return Err("VPro command FIFO is not empty".into());
        }
        if let Some(error) = self.shared.gpu_fault.lock().as_ref() {
            return Err(format!("VPro GPU raster unit is faulted: {error}"));
        }
        self.with_stopped_board(|board| board.gpu_sync_result(super::gpu_unit::Observation::Snapshot))?;
        if let Some(error) = self.shared.gpu_fault.lock().as_ref() {
            return Err(format!("VPro GPU raster unit is faulted: {error}"));
        }
        let (board_name, memory_bytes, board, line_phase) = self.with_stopped_board(|board| {
            Ok::<_, String>((board.widget.product.clone(), board.vram.size(), BoardSnapshot::capture(board)?, board.snapshot_line_phase()?))
        })?;
        let state = OdysseyState {
            version: ODYSSEY_SNAPSHOT_VERSION,
            board,
            front: self.front.lock().snapshot(),
            display_fifo: self.display_fifo.lock().snapshot(),
            display_regs: DisplayRegsSnapshot::capture(&self.shared.regs),
            line_phase,
        };
        let state = postcard::to_allocvec(&state).map_err(|e| format!("encode VPro state: {e}"))?;
        let metadata = toml::to_string(&OdysseyMetadata { version: ODYSSEY_SNAPSHOT_VERSION, board: board_name, memory_bytes, lent })
            .map_err(|e| format!("encode VPro snapshot metadata: {e}"))?;
        Ok((metadata, state))
    }

    /// Check a snapshot's metadata against the running board.
    pub(super) fn check_metadata(&self, metadata: &str, lent: bool) -> Result<OdysseyMetadata, String> {
        let metadata: OdysseyMetadata = toml::from_str(metadata).map_err(|e| format!("decode VPro snapshot metadata: {e}"))?;
        if metadata.version != ODYSSEY_SNAPSHOT_VERSION {
            return Err(format!("VPro snapshot version {} is not supported", metadata.version));
        }
        if metadata.lent != lent {
            return Err(format!("VPro snapshot {} board memory but this board {}", if metadata.lent { "leaves out" } else { "holds" }, if lent { "has it lent" } else { "holds its own" }));
        }
        self.with_stopped_board(|board| {
            if metadata.board != board.widget.product {
                return Err(format!("VPro snapshot is for {}, running board is {}", metadata.board, board.widget.product));
            }
            if metadata.memory_bytes != board.vram.size() {
                return Err(format!("VPro snapshot has {} bytes of memory, running board has {}", metadata.memory_bytes, board.vram.size()));
            }
            Ok(())
        })?;
        Ok(metadata)
    }

    /// Restore the encoded state after the machine's power-on reset. It is
    /// decoded and validated before live state changes; `memory` then stores
    /// board memory, ahead of the state that reads it.
    pub(super) fn restore_state(&self, state: &[u8], memory: impl FnOnce(&mut Board)) -> Result<(), String> {
        let state = decode_state(state)?;
        if state.version != ODYSSEY_SNAPSHOT_VERSION {
            return Err(format!("VPro state version {} is not supported", state.version));
        }
        self.with_stopped_board(|board| state.board.validate(board))?;
        let current_identity = self.front.lock().identity;
        state.front.validate(current_identity)?;
        state.display_fifo.validate()?;
        state.display_regs.validate()?;

        let OdysseyState { board, front, display_fifo, display_regs, line_phase, .. } = state;
        self.with_stopped_board(|running| {
            memory(running);
            running.restore_snapshot(board)?;
            running.restore_raster_snapshot(line_phase)
        })?;
        self.front.lock().restore_snapshot(front)?;
        self.display_fifo.lock().restore_snapshot(display_fifo)?;
        self.shared.regs.restore_snapshot(display_regs)?;
        let gpu_enabled = self.with_stopped_board(|board| board.gpu_unit());
        self.shared.covered.store(if gpu_enabled { self.fifo.pushed() } else { u64::MAX }, Ordering::Release);
        self.shared.swaps.fetch_add(1, Ordering::Release);
        Ok(())
    }
}
