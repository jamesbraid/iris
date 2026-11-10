# Plan: devices placed by location

Status: **in progress.** The machine-sharing refactor it builds on (Control,
GuestNet, Devices, Disks, Ip22Board) is the stack below it. Steps 1 to 3
of the order of work below are built: locations, the socket map, the `rows`
hook, the device list, the SCSI, GIO, PCI and XIO device types, with every
machine placing its disks and cards through the map, `--list-slots`, and the
config by location (`[[TYPE]]` entries, `--device`, migration, the GUI).
The IP27 machines place their disks; their XIO and PCI rows wait for the
IP27 work. The earlier attempt placed SCSI
disks and GIO cards by location but could not place anything below a PCI
card; the `rows` hook fixes that.

## Goal

One spelling for every place a device can go, used by the config file, the
command line, errors, snapshots, the monitor and the GUI, which works the
same on every machine and nests: an option board in an XIO slot, a PCI card
on that board, a disk on that card's SCSI bus, a card behind a PCI-to-PCI
bridge.

```toml
[[pci_cage]]
at = "xio13"

[[pci_bridge]]
at = "xio13.pci0.2"

[[nvme]]
at = "xio13.pci0.2.pci0.0"
path = "nvme.img"

[[hdd]]
at = "xio15.pci0.0.t1"
path = "irix.img"
```

## Locations

A location is a dotted path of hops from the machine root. A hop is a tag
and, for most tags, a number local to its parent. The grammar is the earlier
one, unchanged:

| Hop | Meaning |
|---|---|
| `xioN` | Port N of the XIO fabric the parent reaches (Crossbow, PXBOW): a widget |
| `pciN` | PCI bus N of the parent (a Bridge has 0; a PIC has 0 and 1; a PCI-to-PCI bridge's secondary is 0) |
| `N` | Device N on the parent bus, or controller N of an `hpc3` |
| `tN` | SCSI target N |
| `hpc3`, `gfx`, `gioN` | The Indy and Indigo2 I/O chip, graphics slot and GIO slots |

No node or module hop yet: every machine here has one XIO fabric that
carries I/O. A multi-module system adds `moduleN` in front when it needs it.

## Places per machine

Built-in parts are fixed rows; a row that offers a family takes a placed
device of that family.

| Machine | Fixed | Offers |
|---|---|---|
| Indy | `hpc3`, `hpc3.0` (wd33c93a), `gfx` | `hpc3.0.t1`-`t7` SCSI, `gio0`, `gio1` GIO |
| Indigo2, IP28 | as Indy plus `hpc3.1` | plus `hpc3.1.t1`-`t7` |
| O2 | `pci0`, `pci0.1`, `pci0.2` (aic7880) | `pci0.1.t1`-`t7`, `pci0.2.t1`-`t7` SCSI; `pci0.3` PCI (the "PCI option" slot) |
| Octane | `xio15` (Bridge), `xio15.pci0`, `.0`/`.1` (isp1040), `.2` (ioc3), `.3` (rad1), the graphics port `xio11` when a board is fitted | `xio15.pci0.0.t1`-`t15`, `.1.t1`-`t15` SCSI; `xio15.pci0.4` PCI (iris's own spare device, approximate); `xio9`-`xio12` XIO slots; `xio13` the PCI module's port |
| Origin 200 | `pci0` (the Bridge on the Hub's port; no Crossbow), `pci0.0`/`.1` (isp1040), `pci0.2` (ioc3) | `pci0.0.t1`-`t15`, `pci0.1.t1`-`t15` SCSI |
| Origin 2000 | `xio8` (BaseIO Bridge), `xio8.pci0`, `.0`/`.1` (isp1040), `.2` (ioc3) | targets as above, under `xio8.pci0` |
| Onyx2 | `xio15` (BaseIO-G Bridge), its `pci0` devices as above, `xio8` (graphics slot, test widget) | targets under `xio15.pci0` |

An option board adds its own rows when it is placed (below), so
`--list-slots` shows `xio13.pci0.1`-`3` only on a machine with a PCI module.

## Where rows come from

Rows are declared without building hardware, so `validate`, `--list-slots`
and the GUI see them. They are not a second description of the hardware:

- **Fixed tables are the model's own.** Each PCI host keeps its slot layout
  as data the model already uses: the Octane Bridge's onboard devices and
  spare device, `xio_pci::Kind::slots()`, `mace_pci::IP32_OPTION_SLOT`, the
  IP27 BaseIO's three devices. A machine's `fill` reads those tables; the
  constructor attaches to the same numbers. A bind to a row `fill` did not
  add is an error, so the two cannot drift.
- **Cards declare their rows.** `DeviceType` gains
  `fn rows(o: &Options, at: &Location, t: &Topology)`, called by
  `attach_device` in both the probe and the build pass, right after the claim.
  Placements attach by depth, so `xio13` adds `xio13.pci0` and its slots
  before `xio13.pci0.2` is claimed. A PCI-to-PCI bridge adds `pci0` and
  `pci0.0`-`pci0.31`; a PCI SCSI card adds its target rows.

## Families and owners

A placed device is built with the runtime object bound at its parent row:

| Family | Rows | Owner bound at the parent |
|---|---|---|
| Scsi | `…tN` | `ScsiPort` (the controller behind `DiskController`) at the controller row |
| Pci | `…pciN.D` | the `PciBus` handle, at the `pciN` row |
| Gio | `gioN` | the Indy-family GIO bus |
| Xio | `xioN` | `XioPorts`, at the root: maps a widget at a port, sets its Crossbow link, adds it to HEART's reset list |

`PciBus` is already a cheap handle (an `Arc` inside), so the row holds a
clone of the bus its host owns: `Bridge::bus()`, `MacePci::bus()`,
`PciToPciBridge::secondary()`. The host keeps its bus; nothing on a PCI
access path changes. Every PCI device type (`nvme`, `pci_bridge`,
`virtio_serial` on PCI, a future SCSI card) takes the bus as its owner and
its own last hop as the device number, so one type works on the O2, an
Octane or IP27 Bridge, an option board, behind a bridge and on a PIC.

## Device types

| Type | Family | Adds rows | Replaces |
|---|---|---|---|
| `hdd`, `cdrom`, `daynaport` | Scsi | - | `[scsi.N]` |
| `ultra64`, `test_device`, `virtio_serial` (GIO) | Gio | - | `[ultra64]`, `test_device`, `[virtio_serial]` |
| `virtio_serial` (PCI) | Pci | - | `[virtio_serial]` on the Octane |
| `pci_cage` | Xio (`xio13` only) | `pci0`, `pci0.1`-`3` | `[[xio_board]] kind = "pci-cage"` |
| `pci_shoehorn` | Xio (`xio9`-`12`) | `pci0`, `pci0.1` | `[[xio_board]] kind = "pci-shoehorn"` |
| `pci_bridge` | Pci | `pci0`, `pci0.0`-`31` | `[nvme] bridge = true` |
| `nvme` | Pci | - | `[nvme]` |

`nvme` binds a `ScsiPort`-like disk handle at its own row, so its namespace is
a disk keyed by its location like any other: the `SNAPSHOT_SLOT` sentinel in
`DiskRef` and the separate `cow_nvme.toml` go.

## The registry

Device types are listed in one place, `src/attach/registry.rs`:
`pub static MAKERS: &[Maker]`, one `Maker::of::<T>()` line per type. The
config has one typed field per type name, `Vec<toml::Table>` renamed to the
`[[TYPE]]` key, so `#[serde(deny_unknown_fields)]` stays on `MachineConfig`
and a misspelt top-level key is a parse error as before. A test checks the
two lists name the same types.

Decision: the explicit list, not a link-time registry (`linkme`). A
mistake in it is a compile error or fails the names test, and nothing
depends on link order. Adding a device type costs the type and its impl
beside the device, one line in `registry.rs` and one field in
`MachineConfig`.

The config side is `DeviceArray` (the entries, plus an older file's table or
bool under the same name, kept for `migrate` and never written) and one seam,
`MachineConfig::arrays()` and `array_mut()`, the only code that knows which
field holds which type.

## Config migration

`MachineConfig::migrate` rewrites the older tables once the machine is known,
printing one notice each: `[scsi.N]`, `[ultra64]`, `test_device`,
`test_device_dump`, `[virtio_serial]`, `[[xio_board]]` (to `pci_cage` at
`xio13`, `pci_shoehorn` at the port the older code picked) and
`[nvme]` (to `nvme` at `pci0.3` on the O2, or at that board's `xioW.pci0.S`,
first free slot when unset; `bridge = true` becomes a `pci_bridge` at the slot
and the card at `.pci0.0`). A file mixing old and new forms for the same
devices is an error.

## Snapshots

Disks, overlays and placed devices' state are keyed by location; the schema
goes one above trunk's. The O2 and the Octane already refuse older schemas
and keep doing so. The Indy-family reads older schemas today and keeps
doing so: its loader maps `scsiN` and `ctrlN/` to `hpc3.C.tT` and the fixed
device names to their locations before loading.

## Order of work

1. Topology, Location, DeviceType with `rows`, the registry, hdd/cdrom/
   daynaport. Machines place disks through the map; config is still read
   from `[scsi.N]`, translated on load. `--list-slots`.
2. PCI places: `PciHost`, `PciSlots`, `XioPorts`, Family::Xio, the GIO and PCI
   card types, `pci_cage`, `pci_shoehorn`, `pci_bridge`, `nvme`. The IP27
   machines' rows.
3. Config by location: typed `[[TYPE]]` fields, `--device`, migrate(), the GUI.
4. Snapshots by location, the schema bump, the Indy's older-schema mapping.

## What an IP35 machine adds

A machine on a Bedrock node (the Fuel, branch `fuel/ip35`, and the Tezro)
has no PIC model yet; the Fuel boots on its serial console without one.
When the PIC lands, the machine adds, and nothing in the framework changes:

- `MachineProfile::fill` dispatches to the machine's `fill`, which adds the
  XIO fabric's fixed rows: `xio15` for the IO9's PIC on both machines,
  `xio11` for the second PIC where the board has one, and the graphics port
  (`xio12` on the Tezro) as a fixed row naming the board when one is
  fitted, `Row::fixed("graphics").onboard(board)`.
- For each PIC, a `pci0` and a `pci1` row, and the onboard devices as fixed
  rows read from the board's own device table, the same table the
  constructor attaches from: the IOC4, the ISP12160 and the BCM5701, at the
  device numbers that table gives (not established in iris yet).
- The ISP12160's two buses as two controller rows below its device, each
  `Row::fixed(..).ctl(n).onboard("isp12160")` with `t.scsi(&row, 1..=15,
  false)`, numbered `ctl` 0 and 1 so the `[scsi.N]` translation and the
  legacy target ids find them.
- `impl PciHost for Pic` (DMA per requester, and where each host IRQ goes),
  so the PIC can own its two `PciBus` values as the Bridge does; then a
  bind of `pic.bus(n).clone()` at each `xioW.pciN` row and of a `ScsiPort`
  at each ISP12160 controller row.
- Its free PCI slots, if any, as `Row::offering(label, Family::Pci)` with
  the label the owner's guide gives them.

`[[hdd]] at = "xio15.pci0.<isp device>.<bus>.t1"` then works with no other
change, `pci_bridge` and `nvme` work on any free PIC slot, and
`--list-slots` lists the machine.
