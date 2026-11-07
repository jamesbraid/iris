# The PCI layer: invariants, hazards, and what v1 got wrong (design)

Status: in place for MACE and the Bridge, 2026-11. The user-facing description
is `docs/pci.md`.

## Invariants

- **Headers change only through the bus** (`config_write`, `reset`, snapshot
  load). Each republishes the decode table. A function that resets its own
  header leaves a stale BAR run mapped on the physical bus.
- **Lanes are per-window values**, `Copy`, chosen by the host and held by
  value in each `PciBar`. The bus never sees byte order.
- **`pci/` imports no card, no SCSI and no host.** The one thing it takes from
  outside is the physical bus's page size for the run logic.
- **Snapshot restore is not RST#.** It runs `power_on_devices`, loads, and
  ends with `PciBus::redrive`. A function's load must overwrite what the
  snapshot does not name (the AIC-7880 puts its SCSI targets back with
  `reset_targets`).
- **`delivering` is released with a SeqCst store.** The delivery protocol is a
  store-then-load handshake; a Release store lets the holder's read of
  `dirty` pass it on x86 and a change is lost. The design pseudocode said
  Release.

## Hazards

- A stale run table: anything that changes a BAR, a command register, a
  bridge window or a host window must republish. A change behind a bridge
  republishes the buses in front of it.
- Delivery ordering: the host's own lock must not be taken by anything that
  calls into the combiner with that lock held. The Bridge decides a packet
  under `st` and sends it after dropping `st`.
- Lock order: publish mutex, then the host's lock (`refresh`'s `make`). The
  Bridge drops `st` before `refresh`. A bridge's secondary holds its parent
  weakly; the parent holds the bridge function, which holds the bus.
- loom cannot check the combiner: it models SeqCst as acquire-release and
  reports the handshake broken even with SeqCst on both sides. The tests use a
  stalled host and a hook between the release and the re-check.

## What v1 (`pci-addin`) got wrong

- Cards, SCSI and the snapshot format leaked into the core (`CardRef`,
  `Origin`, `attach_card`, `ScsiController`).
- Three copies of the level state in the Bridge and a lock held across the
  host call in MACE's router.
- A lane trait object per host and a duplicate swizzle in each.
- Reset was whatever each function's power-on did, and the O2 had none.

## Open

See [pci-layer-open.md](pci-layer-open.md).
