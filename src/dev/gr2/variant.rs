//! iris's two GR2 boards: which probe answers each gives.

use super::hq2;
use super::{AuxWindow, Gr2Probe};

/// Which of iris's two boards is installed. Same hardware family; only the
/// probe answers differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gr2Variant {
    /// 2 GE7s. Indigo2 XZ.
    Xz,
    /// 4 GE7s. Indigo2 Elan, and every Indy XZ (hinv says GR3-XZ on Indy,
    /// GR3-Elan on Indigo2; libGLcore names both GR3-Elan from the GE count).
    Elan,
    /// 8 GE7s. Indigo2 Extreme.
    Extreme,
}

impl Gr2Variant {
    pub fn ges(self) -> u32 {
        match self { Self::Xz => 2, Self::Elan => 4, Self::Extreme => 8 }
    }
    /// Board revision in port 0: 4 for the Xz, 6 for the Extreme.
    pub fn board_rev(self) -> u32 {
        match self { Self::Xz | Self::Elan => 4, Self::Extreme => 6 }
    }
    pub fn name(self) -> &'static str {
        match self { Self::Xz => "XZ", Self::Elan => "Elan", Self::Extreme => "Extreme" }
    }
    /// iris's probe answers: monitor code 0 (a 60 Hz 1280 by 1024 CRT), Z
    /// and 24-bit colour present, no other boards.
    pub fn probe(self) -> Gr2Probe {
        let port0 = !self.board_rev() & 0xf;
        let port1 = 0x3c | (!1 & 3);
        Gr2Probe { ges: self.ges(), ports: [port0, port1, 0xff, 0xff], hq_rev: hq2::HQ2_REV, aux_window: AuxWindow::Legacy }
    }
}
