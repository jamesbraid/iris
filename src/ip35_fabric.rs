//! Two Bedrock nodes joined by one NUMAlink cable, with no router.
//!
//! Address bits 39:33 name a node. An access whose node field is a number
//! some reachable node carries goes to that node, which decodes the rest of
//! the address as it would a local access of the same space; the processor
//! views' local forms (everything below 8 GiB) stay the reader's own. The
//! numbers are the nodes' local blocks' (`bedrock_lb`), so a number is claimed
//! by writing it, and a node reaches its own memory by its own number as well
//! as by the local forms.
//!
//! The decode is a table per node, indexed by the node field and written
//! when a number or the link changes. Accesses read one entry: no lock, and
//! no test of whether the machine has one node or two.
//!
//! # What is approximated here
//!
//! A remote access is performed directly on the peer's own decode in the same
//! space, so it reaches whatever the peer's processors would reach there; the
//! hardware's narrower list of remotely reachable blocks is not enforced. No
//! cache-coherence protocol runs between the nodes: both see the same host
//! memory. A node numbered other than zero still decodes its low 8 GiB
//! locally; the number-0 claim does not move to its peer. Its processors'
//! references that name node field zero do: outside the reader-relative forms
//! they reach whichever node carries 0 (`ip35::ZeroAway`), cached or uncached,
//! as the claim table sends them. Inbound DMA and the block-transfer
//! engines name memory by number, node field zero included
//! (`Fabric::dma_route`). A crosstalk register packet, such as an interrupt,
//! for the other node's hub is delivered to it by number (`Fabric::hub_of`).
//!
//! Ported from hw/mips/sgi/sn1-node.c and sn1-fabric.c in
//! jamesbraid/qemu-sgi-origami.

use crate::dev::bedrock_lb::VectorPeer;
use crate::dev::bedrock_ni::{BedrockNi, Cable};
use crate::ip35::Ip35Bus;
use crate::traits::*;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering::Relaxed};
use std::sync::{Arc, OnceLock, Weak};

/// The node field of a physical address starts here; a node's own window is
/// the 8 GiB below `1 << NODE_SHIFT << 1`.
pub const NODE_SHIFT: u32 = 33;
const NODE_COUNT: usize = 128;
const LOCAL_MASK: u64 = (1 << NODE_SHIFT) - 1;
/// The first address a node number can name; everything below is the local forms.
pub const REMOTE_BASE: u64 = 1 << NODE_SHIFT;
/// Where a processor's accesses end: 40 bits of physical address.
pub const REMOTE_END: u64 = 1 << 40;

const NONE: u8 = 0;
const SELF: u8 = 1;
const PEER: u8 = 2;

/// Which of a node's address spaces an access is in: one of the four
/// uncached views (raw bits 58:57), or cached memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space {
    Uncached(u8),
    Cached,
}

/// What a node offers the other end of the cable. Held weakly: the nodes
/// own the fabric.
struct Face {
    bus: Weak<Ip35Bus>,
    cached: Weak<dyn BusDevice>,
    registers: Weak<dyn BusDevice>,
    hub: Weak<dyn BusDevice>,
    ni: Weak<BedrockNi>,
}

struct End {
    number: AtomicU8,
    claims: [AtomicU8; NODE_COUNT],
    face: OnceLock<Face>,
}
impl End {
    fn new() -> Self {
        Self {
            number: AtomicU8::new(0),
            claims: std::array::from_fn(|_| AtomicU8::new(NONE)),
            face: OnceLock::new(),
        }
    }
}

/// What a node offers across the cable.
pub struct Node<'a> {
    pub bus: &'a Arc<Ip35Bus>,
    pub cached: &'a Arc<dyn BusDevice>,
    /// The node's register blocks at their hub-window offsets.
    pub registers: &'a Arc<dyn BusDevice>,
    /// Where a crosstalk register packet for the node, an interrupt for
    /// instance, is delivered.
    pub hub: &'a Arc<dyn BusDevice>,
    pub ni: &'a Arc<BedrockNi>,
}

/// The cable between two nodes and the claims each node holds on node numbers.
pub struct Fabric {
    ends: [End; 2],
    cabled: AtomicBool,
    /// Serialises writers of the claim tables. Readers take no lock.
    writers: Mutex<()>,
}

impl Fabric {
    pub fn new() -> Arc<Self> {
        let fabric = Arc::new(Self { ends: [End::new(), End::new()], cabled: AtomicBool::new(true), writers: Mutex::new(()) });
        fabric.derive();
        fabric
    }
    /// Make `node` the device at `end` of the cable.
    pub fn attach(&self, end: usize, node: Node<'_>) {
        let face = Face {
            bus: Arc::downgrade(node.bus),
            cached: Arc::downgrade(node.cached),
            registers: Arc::downgrade(node.registers),
            hub: Arc::downgrade(node.hub),
            ni: Arc::downgrade(node.ni),
        };
        assert!(self.ends[end].face.set(face).is_ok(), "fabric end {end} already attached");
        let _writer = self.writers.lock();
        self.derive();
    }
    fn peer(&self, end: usize) -> &End {
        &self.ends[end ^ 1]
    }
    fn attached(&self, end: usize) -> bool {
        self.ends[end].face.get().is_some()
    }
    /// The number the node at `end` carries.
    pub fn number(&self, end: usize) -> u8 {
        self.ends[end].number.load(Relaxed)
    }
    /// The node at `end` was given a number by its local block.
    pub fn set_number(&self, end: usize, number: u8) {
        let _writer = self.writers.lock();
        self.ends[end].number.store(number & 0x7f, Relaxed);
        self.derive();
    }
    /// Plug or unplug the cable; both ends see it at once.
    pub fn set_cabled(&self, cabled: bool) {
        let _writer = self.writers.lock();
        self.cabled.store(cabled, Relaxed);
        self.derive();
    }
    /// Whether the node at `end` reaches node field zero across the cable: it
    /// carries another number while its peer carries zero.
    pub fn zero_is_remote(&self, end: usize) -> bool {
        self.ends[end].claims[0].load(Relaxed) == PEER
    }
    pub fn link_up(&self) -> bool {
        self.cabled.load(Relaxed) && self.attached(0) && self.attached(1)
    }
    /// Place each node's claims: itself at its own number, and the peer at
    /// its number while the link is up. Two nodes carrying one number each
    /// reach only themselves there.
    ///
    /// Each table is computed whole and only the entries that change are
    /// stored, so a reader on another thread never sees an entry go away
    /// and come back, and a claim that moves is never absent. Callers hold `writers`, or run before the fabric is
    /// shared.
    fn derive(&self) {
        let up = self.link_up();
        for (i, end) in self.ends.iter().enumerate() {
            let own = end.number.load(Relaxed) as usize % NODE_COUNT;
            let other = self.peer(i).number.load(Relaxed) as usize % NODE_COUNT;
            let mut wanted = [NONE; NODE_COUNT];
            if up && other != own {
                wanted[other] = PEER;
            }
            wanted[own] = SELF;
            // New claims first, then the ones that go: a node that moves
            // from one number to another is at one of them throughout.
            for removing in [false, true] {
                for (claim, wanted) in end.claims.iter().zip(wanted) {
                    if (wanted == NONE) == removing && claim.load(Relaxed) != wanted {
                        claim.store(wanted, Relaxed);
                    }
                }
            }
        }
    }
    /// The device that sends accesses to node numbers from `end` in `space`.
    ///
    /// `rebase` is what the map the route sits in subtracts from addresses:
    /// [`REMOTE_BASE`] for a window based there, 0 for a table's default.
    pub fn route(self: &Arc<Self>, end: usize, space: Space, rebase: u64) -> Arc<Route> {
        Arc::new(Route { fabric: self.clone(), end, space, rebase, decode: Decode::Remote })
    }
    /// The device that sends accesses in `space` at node field zero from
    /// `end` to whichever node carries number 0, whatever their address's node
    /// field.
    pub fn zero_route(self: &Arc<Self>, end: usize, space: Space) -> Arc<Route> {
        Arc::new(Route { fabric: self.clone(), end, space, rebase: 0, decode: Decode::Zero })
    }
    /// The memory the node at `end`'s I/O interface reaches with inbound DMA
    /// and its block-transfer engines: every node by number, node field zero
    /// included, with none of a processor's reader-relative forms.
    pub fn dma_route(self: &Arc<Self>, end: usize) -> Arc<Route> {
        Arc::new(Route { fabric: self.clone(), end, space: Space::Cached, rebase: 0, decode: Decode::Named })
    }
    /// The hub of the other node, when it carries `number`.
    pub fn hub_of(&self, end: usize, number: u8) -> Option<Arc<dyn BusDevice>> {
        let peer = self.peer(end);
        (self.link_up() && peer.number.load(Relaxed) == number).then(|| peer.face.get()?.hub.upgrade()).flatten()
    }
    /// The cable as the NI at `end` sees it.
    pub fn cable(self: &Arc<Self>, end: usize) -> Arc<dyn Cable> {
        Arc::new(Port { fabric: Arc::downgrade(self), end })
    }
    /// Route 0 of the vector engine at `end`.
    pub fn vector_peer(self: &Arc<Self>, end: usize) -> Arc<dyn VectorPeer> {
        Arc::new(Port { fabric: Arc::downgrade(self), end })
    }
}

/// One node's side of the cable.
struct Port {
    fabric: Weak<Fabric>,
    end: usize,
}
impl Cable for Port {
    fn link_up(&self) -> bool {
        self.fabric.upgrade().is_some_and(|f| f.link_up())
    }
    fn send_reset(&self) {
        let Some(fabric) = self.fabric.upgrade() else { return };
        if !fabric.link_up() {
            return;
        }
        if let Some(ni) = fabric.peer(self.end).face.get().and_then(|face| face.ni.upgrade()) {
            ni.link_reset();
        }
    }
}
impl VectorPeer for Port {
    fn access(&self, route: u64, offset: u64, write: bool, data: u64) -> Option<u64> {
        let fabric = self.fabric.upgrade()?;
        if route != 0 || !fabric.link_up() {
            return None;
        }
        let registers = fabric.peer(self.end).face.get()?.registers.upgrade()?;
        // The register window ends below the hub window's top; an offset
        // past what the peer decodes is a refusal, with no reply.
        if write {
            (registers.write64(offset, data) == BUS_OK).then_some(0)
        } else {
            let r = registers.read64(offset);
            (r.status == BUS_OK).then_some(r.data)
        }
    }
}

/// Everything at or above [`REMOTE_BASE`] in one node's space: the node field
/// picks the claimant, which decodes the low 33 bits.
pub struct Route {
    fabric: Arc<Fabric>,
    end: usize,
    space: Space,
    rebase: u64,
    decode: Decode,
}

/// Which node a [`Route`] sends an address to.
#[derive(Clone, Copy)]
enum Decode {
    /// The one its node field names, for addresses at [`REMOTE_BASE`] and
    /// above; below it are the reader's local forms, and a hole in them is a
    /// hole.
    Remote,
    /// Whichever node carries number 0: how a node reaches number 0 while it
    /// carries another.
    Zero,
    /// The one its node field names, number 0 included.
    Named,
}

/// The device an access to `address` lands on, and the address there.
enum Target {
    Uncached(Arc<Ip35Bus>, u64),
    Cached(Arc<dyn BusDevice>, u64),
}
impl Route {
    fn target(&self, offset: u64) -> Option<Target> {
        let address = offset + self.rebase;
        let number = match self.decode {
            // The local forms are the reader's own, and a hole in them is a
            // hole: sending it to a claimant would find the same hole again.
            Decode::Remote if address < REMOTE_BASE => return None,
            Decode::Zero => 0,
            Decode::Remote | Decode::Named => (address >> NODE_SHIFT) as usize & (NODE_COUNT - 1),
        };
        let end = match self.fabric.ends[self.end].claims[number].load(Relaxed) {
            SELF => self.end,
            PEER => self.end ^ 1,
            _ => return None,
        };
        let local = address & LOCAL_MASK;
        let node = &self.fabric.ends[end];
        let face = node.face.get()?;
        match self.space {
            Space::Uncached(view) => {
                Some(Target::Uncached(face.bus.upgrade()?, (view as u64) << 57 | local))
            }
            Space::Cached => Some(Target::Cached(face.cached.upgrade()?, local)),
        }
    }
}
macro_rules! route_access {
    ($r:ident,$w:ident,$t:ty,$out:ident) => {
        fn $r(&self, a: u64) -> $out {
            match self.target(a) {
                Some(Target::Uncached(bus, at)) => bus.$r(at),
                Some(Target::Cached(bus, at)) => bus.$r(at),
                None => $out::err(),
            }
        }
        fn $w(&self, a: u64, v: $t) -> u32 {
            match self.target(a) {
                Some(Target::Uncached(bus, at)) => bus.$w(at, v),
                Some(Target::Cached(bus, at)) => bus.$w(at, v),
                None => BUS_ERR,
            }
        }
    };
}
impl BusDevice for Route {
    fn write64_masked(&self, a: u64, val: u64, mask: u64) -> u32 {
        match self.target(a) {
            Some(Target::Uncached(bus, at)) => bus.write64_masked(at, val, mask),
            Some(Target::Cached(bus, at)) => bus.write64_masked(at, val, mask),
            None => BUS_ERR,
        }
    }
    fn write_narrow_from(&self, a: u64, size: usize, gpr: u64) -> u32 {
        match self.target(a) {
            Some(Target::Uncached(bus, at)) => bus.write_narrow_from(at, size, gpr),
            Some(Target::Cached(bus, at)) => bus.write_narrow_from(at, size, gpr),
            None => BUS_ERR,
        }
    }
    // A store-conditional to another node's memory is the claimant's swap,
    // so two nodes' processors cannot both win the same word.
    fn cas32(&self, a: u64, old: u32, new: u32) -> Option<bool> {
        match self.target(a) {
            Some(Target::Cached(bus, at)) => bus.cas32(at, old, new),
            _ => None,
        }
    }
    fn cas64(&self, a: u64, old: u64, new: u64) -> Option<bool> {
        match self.target(a) {
            Some(Target::Cached(bus, at)) => bus.cas64(at, old, new),
            _ => None,
        }
    }
    fn read_block(&self, a: u64, out: &mut [u64]) -> u32 {
        match self.target(a) {
            Some(Target::Cached(bus, at)) => bus.read_block(at, out),
            _ => BUS_ERR,
        }
    }
    fn write_block(&self, a: u64, data: &[u64]) -> u32 {
        match self.target(a) {
            Some(Target::Cached(bus, at)) => bus.write_block(at, data),
            _ => BUS_ERR,
        }
    }
    route_access!(read8, write8, u8, BusRead8);
    route_access!(read16, write16, u16, BusRead16);
    route_access!(read32, write32, u32, BusRead32);
    route_access!(read64, write64, u64, BusRead64);
}

#[cfg(test)]
mod tests;
