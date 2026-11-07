# SGI Origin 3200 (IP35)

The Origin 3200 is the Fuel's Bedrock node logic in C-bricks: one or two
nodes, each a 400 MHz R12000 with an 8 MiB secondary cache and 512 MiB in
two fixed 256 MiB banks, and an I-brick on the first node's XIO connector.
Two C-bricks are joined by a direct NUMAlink cable with no router.

## Configuration

Select `machine.profile = "origin3200_one_c_ip35"` (one C-brick and the
I-brick, the smallest shipped fit) or `"origin3200_two_c_ip35"`, with
`machine.cpu = "r12000_400"` (`r12000` names it), `banks = [256, 256]`, `graphics.board = "none"`
and the PROM payload as for the [Fuel](fuel-ip34.md). The `[fuel]` section
is shared on purpose: the node's flash, L1 and brick identity are the Fuel's
Bedrock logic, so it takes the same keys (`flash`, `console`, `l1_revision`,
`brick_serial`) rather than a section of its own; only `banks = [256, 256]`
is fixed here. The console is the
first C-brick's L1 (`[fuel] console = "l1"`, port `l1_port`). Each node runs
one processor; the other three positions are disabled in the node's flash
as the PROM would after its third failed start of each.

Each C-brick is the shared Bedrock node (`src/ip35/node.rs`) with the C-brick's
processor part, firmware row, board identification word and L1 identity;
`src/ip35_machine.rs` adds the I-brick's base I/O to the first and the cable
between two.

## Nodes and the link

Address bits 39:33 name a node. Each node's local block holds the number the
PROM stores in its revision word, and the node is reachable at that number
from the other node while the cable is up (`src/ip35_fabric.rs`). The vector
engine's route 0 reads and writes the peer's registers, which is how the PROM
discovers the neighbour and numbers it; port status bit 1 reads link-up. The
tables the PROM writes to the network interface are stored and not consulted.

## Observed boot and limits

PROM 6.210 reaches the System Maintenance Menu on both machines and reports
`ALL DIAGS PASSED`: one node with 1 CPU and 512 MB, or two nodes with 2 CPUs
and 1024 MB. Start System does not boot: the I-brick's root disk is on a Fibre
Channel adapter and Iris has none. The I-brick's IEEE 1394 controller is also
absent, so the PROM's first diagnostic prints `No 1394 Firewire found on
BASEIO` and carries on.

- No cache coherence runs between the nodes; both see the same host memory.
- A node numbered other than zero reaches its own memory at its number.
  Its processors' references that name node field zero, and inbound DMA from
  its I/O interface, go to whichever node carries 0, apart from the
  reader-relative forms that stay on the node: the cached alias, the first
  512 MiB of the Hub-special space and the I/O space's local register form.
- A remote access is performed on the peer's own decode, so it reaches what
  the peer's processors would reach, not a narrower remote list.
- An interrupt packet for the other node's hub is delivered to it by number.

Gaps:

- A reset request from either node resets the whole machine; the network
  interface does not reset one node alone.
- A cached reference to the reset PROM's address window always stays on the
  node. It matters only where RAM overlaps that window, which the required
  256 MiB banks avoid.
