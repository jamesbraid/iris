# QLogic ISP2200 Fibre Channel adapter

iris can fit one QLogic ISP2200 Fibre Channel host adapter in an Octane's PCI
module or shoehorn, with a private loop of SCSI disks behind it. IRIX 6.5.30
brings it up with its own `qlfc` driver and shows the disks as a SCSI
controller of their own.

```toml
[[xio_board]]
kind = "pci-cage"        # or "pci-shoehorn"; see docs/pci.md

[fibre_channel]
slot = 2                 # the board's PCI device (the first free one by default)
# board = 0              # which [[xio_board]] (the first by default)
# bridge = false         # true: behind a PCI-to-PCI bridge in the slot

[fibre_channel.disk.5]   # the loop ID, 0 to 125
path = "frame0.raw"      # raw or CHD
overlay = false
node_name = "20:00:00:e0:8b:00:00:05"   # optional; see below
port_name = "21:00:00:e0:8b:00:00:05"
```

A `[fibre_channel.disk.N]` table is a `[scsi.N]` disk table with a loop ID for
its key (so `path`, `overlay`, `write_cache` and whatever else a disk table
takes apply, including the INQUIRY keys `vendor`, `product`, `revision` and
`serial`) and the two names. Only plain disks go on the loop. The adapter
takes the lowest loop ID no disk holds, or the hard address the guest asks
for.

Like a `[scsi.N]` table, it ignores a key it does not know: a misspelt
`overlay` or `write_cache` is dropped without an error, and so are
`controller`, `mac` and the other keys that mean nothing on the loop. serde
cannot reject unknown keys in a table that flattens another.

## Where it sits

The slot, interrupt and DMA are those of any card on an option board
([docs/pci.md](pci.md#option-boards-in-xio-slots)): INTA on the Bridge pin of
the slot's device. It is PCI vendor 1077, device 2200, class 0c04. Its monitor
commands are `scsi2` and `cow2` (the Octane's own two controllers are 0 and 1),
which act on the loop's disks as the other controllers' do; `scsi2 notes` lists
what a guest did that the model does not decode.

## What it models

The host interface and nothing behind it. A driver sees 24 mailboxes and two
rings of 64-byte entries; the firmware it downloads is kept so its checksum can
be verified and read back, and never run. Fibre Channel itself is not
modelled: no frames, logins or loop initialisation phases. A disk is the one
port at its loop ID and the guest sees loop IDs, names and SCSI commands, which
is all the firmware shows a host. The register, mailbox and ring facts come
from the public Linux, OpenBSD and NetBSD drivers (no programming manual for
the part was found) and from the IRIX 6.5.30 driver's use of it.

Where at least two drivers agree a fact is established. Where they are silent
or disagree the model makes a choice, and the comment in `src/dev/isp2200.rs`
says so. The names a port carries are the disk table's `node_name` and
`port_name`: 16 hex digits, colons allowed, not zero, and no two disks may
share a port name. A name left unset is made from the loop ID (node
`20:00:00:00:00:00:00:NN`, port `21:00:00:00:00:00:00:NN`) and the other name
of the pair stays so. The names belong to the loop; the disk's own INQUIRY data
does not carry them.

## Approximations

- INQUIRY answers the standard data and the vital product data pages 0x00 and
  0x80, the last only for a disk with a `serial`. There is no device
  identification page (0x83): the SCSI layer has no hook for a page the
  adapter would fill in from the loop's names, so a guest that asks for it
  gets Illegal Request and falls back to the standard data.
- Registers take 16-bit accesses. A 32-bit access is answered as a 16-bit one
  at its address; a byte access reads zero and is dropped. The Bridge gives the
  function no way to signal an error for either, and whether the chip takes
  them is unknown.
- Commands run one at a time in ring order on the adapter's own thread. An
  abort or reset matches the command running, not the ones queued behind it;
  the real firmware has many in flight.
- The loop is read off the bus as the machine built it. A disk attached or
  removed later is not announced; there is no LIP and no port database change
  event for it.
- A disk has one LUN, as the SCSI layer below it does.
- The loop takes a fixed 100 ms to come up after initialisation or a LIP. The
  real time is unknown.
- Every reset state, the posting rules, the command window that tells a ring
  pointer write from a mailbox input, and the error codes of each command are
  model choices the drivers leave open.
- Five mailbox commands come from IRIX alone and have no public definition, so
  their replies are approximations (`IRIS_DEBUG_LOG=scsi` marks each):
  - Get Port Queue Parameters (0x29) returns queue depth 256 and throttle 255
    for a loop ID that has a port. It is not answered for the adapter's own
    loop ID, nor while the loop is down: both get 0x4005.
  - Set Retry Count (0x32) and Reset Link Status (0x52) complete with 0x4000
    and change nothing.
  - Get Link Status (0x6b) writes 32 zero bytes, for the adapter's own loop ID
    as well as a port's.
  - Set Port Queue Parameters (0x39) completes with 0x4000 for any loop ID and
    echoes mailboxes 1 to 7 as the guest wrote them. IRIX writes only
    mailboxes 0 and 1, so the depth and throttle it means to send never
    reach the adapter, and the model keeps neither.
- A mailbox command the model does not know, or does not implement yet (abort
  device, bus reset, target reset, resource counts, stop firmware, the SCSI
  queue setup commands, the enhanced port database, login, logout and the
  name-server commands), answers 0x4001, and so does every command but the ROM
  ones before Execute Firmware. `scsi2 notes` lists the command.
- The NVRAM image is invented; the board's contents are unknown. The adapter's
  port name carries QLogic's company ID and a serial taken from a hash of
  `isp2200-{index}`, `index` being the adapter's number in the machine, so it
  stays the same from run to run and changes if the adapter number does.
  (The QEMU model hashes the device's QOM path instead, so the same card has a
  different name there.) A read at word address 128 or above returns 0xffff.
- The model reports the ISP2200A's value in mailbox 7 of the reset reply; the
  ISP2200 itself is not told apart. No reduced-interrupt or fast-post
  completions: every command posts a full status entry.
- Only IRIX 6.5.30 has been tried. Linux logs in through request-ring mailbox
  entries, which the model does not serve, so mainline Linux cannot bring a
  port online. No other guest has been tested.
- The PCI class and subsystem id are not read by any driver and are model
  choices.

A snapshot saves the adapter's registers, rings and loop state with the board
and each disk's overlay with the others, and records the disks in its manifest
at slot 64 plus the loop ID.

The GUI's Disks tab fits the adapter on an Octane and lists the loop disks by
loop ID; names, slot and bridge are set in the configuration file.
