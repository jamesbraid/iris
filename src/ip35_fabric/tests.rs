// Cases follow tests/qtest/sn1-cross-node-test.c and sn1-bedrock-link-test.c
// in jamesbraid/qemu-sgi-origami: the node-number decode and the router-less
// pair's link.
use super::*;
use crate::dev::bedrock_lb::reg as lb;
use crate::dev::bedrock_ni::{reg as ni, LINK_UP, UNCABLED};
use crate::dev::bedrock_flash::FirmwareRecord;
use crate::dev::bedrock_pi::Population;
use crate::dev::l1::L1Config;
use crate::ip35::node::{Node as BedrockNode, NodeConfig};
use crate::traits::BusDevice;

/// A node as the fabric tests need it: the shared Bedrock node and its cached
/// view.
struct Ip35Board(BedrockNode);
impl std::ops::Deref for Ip35Board {
    type Target = BedrockNode;
    fn deref(&self) -> &BedrockNode {
        &self.0
    }
}
impl Ip35Board {
    fn cached_bus(&self) -> Arc<dyn BusDevice> {
        self.cached.clone()
    }
}

const PROBE: u64 = 0x1000;
const WORD0: u64 = 0x0a00;
const WORD1: u64 = 0x0b11;
const SPACE_IO: u64 = 1 << 57;
/// The Bedrock's node-named register form in the I/O space.
const BEDROCK_NODE: u64 = SPACE_IO + 0x0180_0000;
const LB_BASE: u64 = 0x60_0000;
const NODE_REGION: u64 = LB_BASE + lb::NODE_REGION;
const SCRATCH2: u64 = LB_BASE + lb::PIO_WINDOW + 16;
const SENTINEL: u64 = 0xdead_beef_dead_beef;

fn node(link: Option<(Arc<Fabric>, usize)>, bay: u32) -> Ip35Board {
    let node = BedrockNode::build_linked(
        NodeConfig {
            name: "C-brick",
            payload: &[],
            clock: Arc::new(|| 0),
            flash_path: None,
            banks_mib: &[256, 256],
            population: Population::SINGLE,
            running: Population::SINGLE,
            firmware: FirmwareRecord::FUEL,
            flash_records: Vec::new(),
            board_id: 2,
            l1: L1Config { bay, mac: [8, 0, 0x69, 0, 0, bay as u8], ..Default::default() },
        },
        link,
    )
    .unwrap();
    let board = Ip35Board(node);
    // The memory unit's banks, and the cell that lets the register forms answer.
    assert_eq!(board.md.md_bus().write64(8, 0x33), BUS_OK);
    assert_eq!(board.md.md_bus().write64(0, 1), BUS_OK);
    board
}

/// Two nodes on a direct cable, each holding a word that says who answered
/// at the same node-local address.
fn pair() -> (Ip35Board, Ip35Board, Arc<Fabric>) {
    let fabric = Fabric::new();
    let a = node(Some((fabric.clone(), 0)), 4);
    let b = node(Some((fabric.clone(), 1)), 7);
    assert_eq!(a.md.memory_view().write64(PROBE, WORD0), BUS_OK);
    assert_eq!(b.md.memory_view().write64(PROBE, WORD1), BUS_OK);
    assert_eq!(a.lb.write64(lb::PIO_WINDOW + 16, 0x5c0), BUS_OK);
    assert_eq!(b.lb.write64(lb::PIO_WINDOW + 16, 0x5c1), BUS_OK);
    (a, b, fabric)
}

/// The master's number store: the revision word with the node field replaced.
fn number(board: &Ip35Board, n: u64) {
    assert_eq!(board.lb.write64(lb::NODE_REGION, n << 32), BUS_OK);
}
fn node_at(n: u64) -> u64 {
    n << NODE_SHIFT
}
fn cached(board: &Ip35Board, address: u64) -> Option<u64> {
    let r = board.cached_bus().read64(address);
    (r.status == BUS_OK).then_some(r.data)
}
fn uncached(board: &Ip35Board, space: u64, address: u64) -> Option<u64> {
    let r = board.bus.read64(space | address);
    (r.status == BUS_OK).then_some(r.data)
}

/// From reset both local blocks carry 0, as the PROM's first store leaves
/// them: each node reaches only itself at node field zero, and nothing
/// answers at node 1's placement.
#[test]
fn equal_numbers_stay_local() {
    let (a, b, _fabric) = pair();
    assert_eq!(cached(&a, PROBE), Some(WORD0));
    assert_eq!(cached(&b, PROBE), Some(WORD1));
    assert_eq!(cached(&a, node_at(1) + PROBE), None);
    assert_eq!(cached(&b, node_at(1) + PROBE), None);
    assert_eq!(uncached(&a, 3 << 57, node_at(1) + PROBE), None);
}

/// Once node 1 carries 1, node field 1 names node 1 from either node, in the
/// cached view, the uncached memory view and the I/O register forms.
#[test]
fn numbered_nodes_reach_each_other() {
    let (a, b, _fabric) = pair();
    number(&b, 1);
    assert_eq!(cached(&a, PROBE), Some(WORD0));
    assert_eq!(cached(&a, node_at(1) + PROBE), Some(WORD1));
    assert_eq!(cached(&b, node_at(1) + PROBE), Some(WORD1), "a node reaches itself by its own number");
    assert_eq!(uncached(&a, 3 << 57, node_at(1) + PROBE), Some(WORD1));
    assert_eq!(uncached(&a, BEDROCK_NODE, node_at(1) + SCRATCH2), Some(0x5c1));
    assert_eq!(uncached(&a, BEDROCK_NODE, node_at(0) + SCRATCH2), Some(0x5c0));
}

/// The claim follows the number: node 1 renumbered to 5 answers only at 5.
#[test]
fn renumbering_moves_the_claim() {
    let (a, b, _fabric) = pair();
    number(&b, 1);
    number(&b, 5);
    assert_eq!(cached(&a, node_at(5) + PROBE), Some(WORD1));
    assert_eq!(cached(&a, node_at(1) + PROBE), None);
    assert_eq!(cached(&b, node_at(5) + PROBE), Some(WORD1));
    assert_eq!(cached(&b, node_at(1) + PROBE), None);
}

/// Two nodes carrying one number each reach only themselves there.
#[test]
fn a_shared_number_reaches_only_the_reader() {
    let (a, b, _fabric) = pair();
    number(&a, 3);
    number(&b, 3);
    assert_eq!(cached(&a, node_at(3) + PROBE), Some(WORD0));
    assert_eq!(cached(&b, node_at(3) + PROBE), Some(WORD1));
}

/// A cable marked down leaves each node only itself.
#[test]
fn cable_down_makes_the_peer_unreachable() {
    let (a, b, fabric) = pair();
    number(&b, 1);
    assert_eq!(cached(&a, node_at(1) + PROBE), Some(WORD1));
    fabric.set_cabled(false);
    assert_eq!(cached(&a, node_at(1) + PROBE), None);
    assert_eq!(cached(&b, node_at(1) + PROBE), Some(WORD1));
    assert_eq!(cached(&a, PROBE), Some(WORD0), "the reader's own memory is unaffected");
    fabric.set_cabled(true);
    assert_eq!(cached(&a, node_at(1) + PROBE), Some(WORD1));
}

/// A remote store lands in the claimant's memory, in every width, and a
/// node nobody claims faults rather than stopping the machine.
#[test]
fn remote_stores_land_at_the_claimant() {
    let (a, b, _fabric) = pair();
    number(&b, 1);
    let cache = a.cached_bus();
    assert_eq!(cache.write64(node_at(1) + 0x2000, 0x1122_3344_5566_7788), BUS_OK);
    assert_eq!(cache.write32(node_at(1) + 0x2008, 0x99aa_bbcc), BUS_OK);
    assert_eq!(cache.write8(node_at(1) + 0x200c, 0xdd), BUS_OK);
    assert_eq!(b.md.memory_view().read64(0x2000).data, 0x1122_3344_5566_7788);
    assert_eq!(b.md.memory_view().read64(0x2008).data >> 32, 0x99aa_bbcc);
    assert_eq!(a.md.memory_view().read64(0x2000).data, 0, "the writer's own memory is untouched");
    assert_eq!(cache.write64(node_at(9) + 0x2000, 1), BUS_ERR);
}

/// A node that carries another number reaches node field zero across the
/// cable, except the cached alias: the first bytes of its own memory, sized by
/// the processor interface's cached-alias register.
#[test]
fn a_renumbered_node_reaches_node_zero_across_the_cable() {
    use crate::dev::bedrock_pi::reg as pi;
    let (a, b, _fabric) = pair();
    assert_eq!(cached(&b, PROBE), Some(WORD1), "both carry 0: each reaches itself");
    number(&b, 1);
    assert_eq!(cached(&b, PROBE), Some(WORD0), "node field zero is node 0's memory now");
    assert_eq!(cached(&b, node_at(1) + PROBE), Some(WORD1), "its own memory is at its number");
    assert_eq!(cached(&a, PROBE), Some(WORD0));
    // Code 4 is 32 KiB, which holds PROBE.
    assert_eq!(b.pi[0].write64(pi::CACHED_ALIAS, 4), BUS_OK);
    assert_eq!(cached(&b, PROBE), Some(WORD1), "inside the cached alias it stays on the node");
    assert_eq!(cached(&b, 0x8000 + PROBE), Some(0), "past the alias it is node 0's, which holds nothing there");
    assert_eq!(b.cached_bus().write64(0x9000, 5), BUS_OK);
    assert_eq!(a.md.memory_view().read64(0x9000).data, 5, "a store lands on node 0");
    assert_eq!(b.md.memory_view().read64(0x9000).data, 0);
}

/// A renumbered node's processor reaches node field zero across the cable in
/// the uncached spaces as well, outside the reader-relative forms: memory past
/// the cached alias, the memory-special space, the I/O space past its local
/// register form. The Hub-special space's first 512 MiB stays the node's own.
/// What the cable offers the peer is the node's own decode, so the peer's
/// references to it by number do not come back.
#[test]
fn a_renumbered_node_reaches_node_zero_uncached() {
    use crate::dev::bedrock_pi::reg as pi;
    const MEMORY: u64 = 3 << 57;
    const SPECIAL: u64 = 2 << 57;
    let processor = |board: &Ip35Board, address: u64| {
        let r = board.processor.read64(address);
        (r.status == BUS_OK).then_some(r.data)
    };
    let (a, b, _fabric) = pair();
    assert_eq!(processor(&b, MEMORY | PROBE), Some(WORD1), "both carry 0: each reaches itself");
    assert_eq!(processor(&b, BEDROCK_NODE | SCRATCH2), Some(0x5c1));
    number(&b, 1);
    assert_eq!(processor(&b, MEMORY | PROBE), Some(WORD0), "uncached memory is node 0's");
    assert_eq!(processor(&b, SPECIAL | PROBE), Some(WORD0), "and so is its memory-special space");
    assert_eq!(processor(&b, BEDROCK_NODE | SCRATCH2), Some(0x5c0), "node 0's node-named registers");
    assert_eq!(processor(&b, SPACE_IO | 0x0100_0000 | SCRATCH2), Some(0x5c1), "the local register form stays");
    assert_eq!(processor(&b, PROBE), Some(WORD1), "the Hub-special memory view stays");
    assert_eq!(processor(&b, MEMORY | node_at(1) | PROBE), Some(WORD1), "its own memory is at its number");
    assert_eq!(b.pi[0].write64(pi::CACHED_ALIAS, 4), BUS_OK);
    assert_eq!(processor(&b, MEMORY | PROBE), Some(WORD1), "inside the cached alias it stays on the node");
    assert_eq!(b.processor.write64(MEMORY | 0x9000, 5), BUS_OK);
    assert_eq!(a.md.memory_view().read64(0x9000).data, 5, "a store past the alias lands on node 0");
    assert_eq!(b.md.memory_view().read64(0x9000).data, 0);
    // Node 0 reaches node 1 by number, and the processor path of node 0, which
    // carries 0, is its own decode.
    assert_eq!(processor(&a, MEMORY | node_at(1) | PROBE), Some(WORD1));
    assert_eq!(processor(&a, MEMORY | PROBE), Some(WORD0));
    assert_eq!(uncached(&a, BEDROCK_NODE, node_at(1) + SCRATCH2), Some(0x5c1));
    // The memory-special space has no local form, whatever the offset.
    assert_eq!(b.pi[0].write64(pi::CACHED_ALIAS, 4), BUS_OK);
    let away = b.away.as_ref().unwrap();
    assert!(away.redirect_uncached(2, 0x10).is_some(), "inside the cached alias, still node 0's");
    assert!(away.redirect_uncached(3, 0x10).is_none(), "where uncached memory keeps the alias");
}

/// The direct RAM window of a node that carries another number than 0 is
/// taken off its processor's path, so its cached references reach the
/// redirect; node 0 keeps its window.
#[test]
fn a_renumbered_node_gives_up_its_direct_ram_window() {
    use crate::ppmem::ppmem::MappedMemory as _;
    let (a, b, _fabric) = pair();
    let (away_a, away_b) = (a.away.clone().unwrap(), b.away.clone().unwrap());
    let bitmap = |board: &Ip35Board| board.md.physical().ppmem_space().unwrap().mapped_bitmap();
    assert_ne!(bitmap(&b), 0, "the node's banks are mapped directly");
    away_b.sync_window(b.md.physical());
    assert_ne!(bitmap(&b), 0, "while node 0 is its own, the window stays");
    number(&b, 1);
    away_a.sync_window(a.md.physical());
    assert_ne!(bitmap(&a), 0, "node 0 keeps its window");
    away_b.sync_window(b.md.physical());
    assert_eq!(bitmap(&b), 0, "node 1's references go through the redirect");
    assert_eq!(cached(&b, PROBE), Some(WORD0));
}

/// A register packet for the other node's hub, an interrupt for a processor
/// there, is delivered to it by node number rather than refused.
#[test]
fn an_interrupt_packet_reaches_the_other_nodes_hub() {
    use crate::dev::bedrock_pi::reg as pi;
    let (a, b, _fabric) = pair();
    number(&b, 1);
    for board in [&a, &b] {
        assert_eq!(board.io.write64(crate::dev::bedrock_io::reg::INBOUND_PERMISSION, !0), BUS_OK);
    }
    assert_eq!(b.pi[0].write64(pi::OFFSET_0010, 1), BUS_OK);
    let hub = |n: u64| (1u64 << 47) | (n << 39) | 0x180_0090;
    assert_eq!(a.io.inbound_write(15, Some(0), hub(1), 8, 0x108), BUS_OK);
    assert_eq!(b.pi[0].read64(pi::PENDING_STATUS_A).data, 1 << 8, "node 1's processor interface took it");
    assert_eq!(a.pi[0].read64(pi::PENDING_STATUS_A).data, 0);
    assert_eq!(a.io.inbound_write(15, Some(0), hub(9), 8, 0x108), BUS_ERR, "nobody carries 9");
}

/// Inbound DMA names memory by node number, node field zero included and with
/// no cached alias: a widget on node 0
/// reaches node 1's memory by its number, and once node 1 carries 1 its own
/// widgets reach node 0's memory at node field zero.
#[test]
fn inbound_dma_reaches_memory_by_node_number() {
    use crate::dev::bedrock_io::reg as io;
    use crate::dev::bedrock_pi::reg as pi;
    let (a, b, _fabric) = pair();
    for board in [&a, &b] {
        assert_eq!(board.io.write64(io::INBOUND_PERMISSION, !0), BUS_OK);
    }
    let read = |board: &Ip35Board, address: u64| {
        let r = board.io.inbound_read(15, Some(0), address, 8);
        (r.status == BUS_OK).then_some(r.data)
    };
    assert_eq!(read(&b, PROBE), Some(WORD1), "both carry 0: each reaches itself");
    number(&b, 1);
    assert_eq!(b.pi[0].write64(pi::CACHED_ALIAS, 4), BUS_OK);
    assert_eq!(read(&a, PROBE), Some(WORD0));
    assert_eq!(read(&a, node_at(1) + PROBE), Some(WORD1), "node 1 by its number");
    assert_eq!(read(&b, node_at(1) + PROBE), Some(WORD1), "a node by its own number");
    assert_eq!(read(&b, PROBE), Some(WORD0), "node field zero is node 0's, alias or not");
    assert_eq!(a.io.inbound_write(15, Some(0), node_at(1) + 0x9000, 8, 6), BUS_OK);
    assert_eq!(b.md.memory_view().read64(0x9000).data, 6, "a DMA store lands on node 1");
    assert_eq!(read(&a, node_at(9) + PROBE), None, "nobody carries 9");
}

/// A store-conditional to another node's memory is the claimant's
/// compare-and-swap: it wins once, and loses when the word has moved on.
#[test]
fn remote_compare_and_swap_acts_on_the_claimant() {
    let (a, b, _fabric) = pair();
    number(&b, 1);
    let cache = a.cached_bus();
    let at = node_at(1) + 0x3000;
    assert_eq!(cache.write64(at, 0x1111_2222_3333_4444), BUS_OK);
    assert_eq!(cache.cas64(at, 0x1111_2222_3333_4444, 7), Some(true));
    assert_eq!(cache.cas64(at, 0x1111_2222_3333_4444, 9), Some(false));
    assert_eq!(b.md.memory_view().read64(0x3000).data, 7, "the claimant's memory holds the swap");
    assert_eq!(cache.write32(at + 8, 5), BUS_OK);
    assert_eq!(cache.cas32(at + 8, 5, 6), Some(true));
    assert_eq!(cache.cas32(at + 8, 5, 8), Some(false));
    // Uncached spaces and unclaimed nodes offer no swap.
    assert_eq!(cache.cas32(node_at(9) + 0x3000, 0, 1), None);
    // The node's own number reaches its own memory the same way.
    let own = b.cached_bus();
    assert_eq!(own.cas64(at, 7, 8), Some(true));
}

/// The port status follows the cable from reset with nothing written, and a
/// port that was never cabled reads bit 0 alone.
#[test]
fn port_status_follows_the_cable() {
    let (a, b, fabric) = pair();
    assert_eq!(a.ni.read64(ni::PORT_STATUS).data, LINK_UP);
    assert_eq!(b.ni.read64(ni::PORT_STATUS).data, LINK_UP);
    fabric.set_cabled(false);
    assert_eq!(a.ni.read64(ni::PORT_STATUS).data, 0);
    assert_eq!(b.ni.read64(ni::PORT_STATUS).data, 0);
    fabric.set_cabled(true);
    assert_eq!(a.ni.read64(ni::PORT_STATUS).data, LINK_UP);
    let lone = node(None, 4);
    assert_eq!(lone.ni.read64(ni::PORT_STATUS).data, UNCABLED);
}

/// A vector launch down route 0 performs the access on the peer's registers.
fn launch(board: &Ip35Board, params: u64, data: u64) -> u64 {
    board.lb.read64(lb::VECTOR_STATUS_ALIAS);
    board.lb.write64(lb::REGISTER_C030, SENTINEL);
    board.lb.write64(lb::REGISTER_C008, 0);
    board.lb.write64(lb::REGISTER_C010, data);
    board.lb.write64(lb::VECTOR_REQUEST, params);
    board.lb.read64(lb::VECTOR_STATUS).data
}

/// The master reads the neighbour's identity down route 0, then numbers it
/// by a vector write of its revision word: the peer changes, the launcher
/// does not, and the peer is reachable at its number from then on.
#[test]
fn the_master_numbers_the_peer_over_route_zero() {
    let (a, b, _fabric) = pair();
    let read = NODE_REGION | (0x1c8 << 40);
    let status = launch(&a, read, 0);
    assert_eq!(status >> 63, 1);
    let revision = a.lb.read64(lb::REGISTER_C030).data;
    assert_eq!(revision, b.lb.read64(lb::NODE_REGION).data);
    let write = NODE_REGION | (0x7b << 40) | 1;
    let status = launch(&a, write, (revision & !(0xff << 32)) | (1 << 32));
    assert_eq!(status >> 63, 1);
    assert_eq!(b.lb.node_number(), 1);
    assert_eq!(a.lb.node_number(), 0);
    assert_eq!(cached(&a, node_at(1) + PROBE), Some(WORD1));
}

/// A launch with no cable latches no reply, and one down a nonzero route
/// finds nothing to forward it.
#[test]
fn a_lone_node_and_a_nonzero_route_get_no_reply() {
    let lone = node(None, 4);
    assert_eq!(launch(&lone, NODE_REGION | (0x1c8 << 40), 0), 0);
    let (a, _b, fabric) = pair();
    lone.lb.write64(lb::REGISTER_C008, 1);
    a.lb.write64(lb::REGISTER_C008, 1);
    a.lb.write64(lb::VECTOR_REQUEST, NODE_REGION | (0x1c8 << 40));
    assert_eq!(a.lb.read64(lb::VECTOR_STATUS).data, 0);
    fabric.set_cabled(false);
    assert_eq!(launch(&a, NODE_REGION | (0x1c8 << 40), 0), 0, "a downed cable carries nothing");
}

/// A port reset goes over the cable and resets the far node only when its
/// reset enable is set; the sender is left alone.
#[test]
fn a_port_reset_arrives_gated_by_the_far_enable() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let (a, b, _fabric) = pair();
    let near = Arc::new(AtomicU64::new(0));
    let far = Arc::new(AtomicU64::new(0));
    let (n, f) = (near.clone(), far.clone());
    a.ni.connect_local_reset(Arc::new(move || { n.fetch_add(1, Ordering::SeqCst); }));
    b.ni.connect_local_reset(Arc::new(move || { f.fetch_add(1, Ordering::SeqCst); }));
    a.ni.write64(ni::LOCAL_RESET, 2);
    assert_eq!(far.load(Ordering::SeqCst), 0);
    b.ni.write64(ni::CONTROL_0, 1);
    a.ni.write64(ni::LOCAL_RESET, 2);
    assert_eq!(far.load(Ordering::SeqCst), 1);
    assert_eq!(near.load(Ordering::SeqCst), 0);
}

/// A node's own claim never goes away while its peer is renumbered, and
/// two threads renumbering the two nodes at once leave a table that matches
/// the final numbers.
#[test]
fn renumbering_never_hides_a_claim_from_a_reader() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (a, b, fabric) = pair();
    number(&a, 3);
    number(&b, 1);
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let stop = stop.clone();
        let cache = a.cached_bus();
        std::thread::spawn(move || {
            let mut misses = 0u32;
            while !stop.load(Ordering::Relaxed) {
                if cache.read64(node_at(3) + PROBE).status != BUS_OK {
                    misses += 1;
                }
            }
            misses
        })
    };
    let writer = {
        let fabric = fabric.clone();
        std::thread::spawn(move || {
            for _ in 0..2000u64 {
                fabric.set_number(0, 3);
            }
        })
    };
    for round in 0..2000u64 {
        fabric.set_number(1, 1 + (round & 1) as u8);
    }
    writer.join().unwrap();
    stop.store(true, Ordering::Relaxed);
    assert_eq!(reader.join().unwrap(), 0, "a node lost sight of its own memory");
    fabric.set_number(1, 5);
    assert_eq!(cached(&a, node_at(5) + PROBE), Some(WORD1));
    assert_eq!(cached(&a, node_at(3) + PROBE), Some(WORD0));
    assert_eq!(cached(&a, node_at(1) + PROBE), None);
    assert_eq!(cached(&b, node_at(5) + PROBE), Some(WORD1));
    assert_eq!(cached(&b, node_at(3) + PROBE), Some(WORD0));
}
