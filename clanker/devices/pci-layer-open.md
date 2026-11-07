# PCI layer: open items

Status: open, 2026-11.

- **O2 RST# on cold reset** is inferred from the PCI specification's default.
  No O2 document states it. The O2 Device Driver Programming Guide has not been
  searched for a reset register; the reviewed MACE datasheet has none.
- **Bridge Control 27:24**: which pin serves which slot is unknown; any
  1-to-0 resets the whole bus. Whether the stock PROM or IRIX ever writes a pin
  low has not been observed.
- **Bridge type 1 aperture** (configuration cycles to buses behind a bridge)
  is not modelled: the window's offset is not in the documents we have.
- **MACE 64-bit PIO** is not in the driver guide's table; iris splits it into
  two word cycles, high word first. MACE rev 0 sends 4-byte-aligned addresses
  on I/O cycles, so byte and halfword I/O may fail on it (MACE specification
  p. 167); iris does not model that.
- **Octane pin rule** (A and C to the device's pin, B and D to device ^ 4) is
  inferred, not established.
- **Bridge DMA** keeps its `st` lock and the `Weak` upgrade per call; that is a
  separate project.
- **A BAR that straddles a bridge window's edge** is not forwarded; what
  hardware does is undefined.
