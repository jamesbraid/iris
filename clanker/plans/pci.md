# PCI: what the shared layer delivers and what comes next

Status: layer in place for the O2 and the Octane's onboard Bridge, 2026-11.
Steps (a) and (b) ride with the Fuel and Tezro branches, (c) is a follow-up, (d)
waits for the machine rework. The description of the layer is `docs/pci.md`;
its invariants are in `clanker/devices/pci-layer-design.md`; what is still
approximate is in `clanker/devices/pci-layer-open.md`.

## What the layer delivers

- One `PciBus` (`src/pci/`) for MACE and the Bridge: the function table, BAR
  decode, interrupt combining, reset and snapshot state. A host decides only
  how a CPU access becomes configuration cycles and a `ByteOrder`
  (`src/pci/host.rs`), where an output goes, and what a function's DMA reaches.
- A PCI-to-PCI bridge function with a type 1 header. The Octane Bridge sends
  type 1 cycles through its `0x028000` aperture (Bridge ASIC specification
  rev 4.0, tables 26 and 43), and its 2 GB window carries the 32-bit alias of
  PCI memory, so a BAR behind a bridge is reachable.
- RST# to every function on the Octane's cold reset, and to one device when
  Bridge Control drops that device's reset pin.
- The `pci`, `bridge` and `heart` log modules and the Octane `pci` monitor
  command.

No machine fits a PCI-to-PCI bridge yet; the tests build one.

## Steps

### (a) Fuel moves its XBridge onto the layer

The Fuel's XBridge (Xbridge ASIC specification rev 0.5, in `docs/Octane/`)
becomes a host of `PciBus` the way the Octane's Bridge is: its own config
cycles, byte order and interrupt pins, and the layer's function table and
decode. This belongs in the Fuel branch, which already owns the XBridge model.

### (b) Tezro moves its PIC onto the layer

The same for the Tezro's PIC (the Origin 3000 family's Crosstalk-to-PCI
chip). Two things to settle in that branch: the PIC's specification says how many
buses it has (if two, one host holds two `PciBus` values), and how its byte
order is chosen per window.

### (c) A follow-up for real slots

One change after (a) and (b), in this order:

1. A generic XIO-to-PCI card: the XIO widget that carries a PCI bus behind it
   (the "shoehorn" carrier), built as a `PciBus` host of its own.
2. The Octane's PCI card cage on top of it, with slots a guest can find by
   scanning, through a `PciToPciBridge` where the hardware has one.
3. An NVMe controller placed in a card-cage slot. The onboard Bridge keeps only
   what is built into the machine (the SCSI controllers, IOC3, RAD, and
   virtio-serial when enabled); an add-in card never sits on it.

Open questions for this step: how IRIX numbers the buses behind the cage
(IRIX 6.5.30 already scans a bus behind a bridge through the type 1 aperture),
and which interrupt pins the cage's slots reach. Neither is in a document we
hold; do not infer them from the onboard wiring.

### (d) After the machine rework

- RST# on the O2's cold reset. MACE has a `PCIRst_N` output and CRIME drives
  system reset (MACE specification pp. 188 and 194), but no document says the
  board joins them, so it stays inferred.
- A snapshot restore that re-creates devices rather than resetting them, and
  the AIC-7880's three resets told apart: RST#, the chip's `CHIPRST`, and the
  SCSI bus reset. Today a load calls the bus reset to put targets back.

## Known open questions

- Which device number the Bridge names in a master time-out for a cycle nobody
  claims, and what an access to the reserved 16 MB to 1 GB of its 2 GB window
  does (Bridge specification p. 52). Both are bus errors today.
- Whether MACE rev 0's 4-byte-aligned I/O cycles break byte and halfword I/O
  (MACE specification p. 167), and the rule for MACE's 64-bit PIO.
- Which of the Bridge's four reset pins serves which device, and whether the
  PROM or IRIX ever drops one.
- Whether the Bridge's pins pulse on a core reset or a Crossbow link reset.
