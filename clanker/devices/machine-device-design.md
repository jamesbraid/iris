# Places, device types and how a device is attached

Status: current as of the locations-and-config change. Check against
`src/location.rs`, `src/topology.rs`, `src/attach.rs`, `src/disks.rs` and the
machine files before relying on details. The user-facing description is
[docs/devices.md](../../docs/devices.md).

## Invariants

- **The rows decide.** Where a device may go is the machine's socket map: a
  `Topology` of rows keyed by `Location`, filled by the machine's `fill`
  (`Ip22Machine::fill`, `O2Machine::fill`, `OctaneMachine::fill`) from the
  profile and config alone, touching no hardware. `Topology::claim` is the one
  check that a place exists, offers the device's family, suits a network
  device and is free. Nothing else decides this; GUI, CLI, config, CI and the
  tests all reach it through `attach_device`.
- **A device registers itself.** A device type implements `DeviceType` and has
  one `register_device!` line beside it; `MAKERS` (a `linkme` list) is the only
  list of types. A name may register once per family (`virtio_serial` is on
  GIO rows and on PCI rows). Adding a type touches no central table.
- **Per-access data is built at attach.** Attach, probe, listing and reset run
  under the map's lock or take clones; no CPU, memory or device access path
  reads the map, `Maker` or `extra`.
- **The constructor binds, `fill` declares.** The constructor builds the
  hardware and calls `Topology::bind(at, handle)` for what it made; a bind to
  a place `fill` did not add is an error, so the two cannot drift. A SCSI row
  holds a `ScsiPort` (the controller behind `DiskController`, plus the closure
  that builds a DaynaPort), not the `ScsiBus`: the controller owns its bus by
  value and sharing it would put an `Arc` on every command.
- **State is keyed by location.** Disk records, overlay files (`<location>.overlay`,
  one flat directory, one `cow.toml`) and the snapshot keys of attached devices
  are location text (schema 6).

## Hazards

- **Attach order is (depth, device name, index).** toml tables are
  key-sorted, so file order across device types is not available; a card at
  `pci0.3` still attaches before `pci0.3.t1` because depth sorts first.
- **A top-level key can be an old setting or a device name.** `ultra64`,
  `test_device` and `virtio_serial` are both. Migration tells them apart by
  shape (table, bool, string against an array of tables) and takes only the
  old shape; a typed field of the same name would break parsing the new one.
- **`extra` is not checked by serde.** The flatten that collects the device
  arrays also swallows typos, so `check_keys` is the strictness: anything that
  parses a config file must go through `MachineConfig::from_toml` or
  `try_load`, not `toml::from_str`, or `mode = "pcap"` at the top level is
  silently accepted again.
- **A bind to a location with no row** is the error you get when `fill` and the
  constructor disagree; fix `fill`.
- **Defaults are applied in one place.** `with_default_media` adds the default
  disk and CD-ROM after migration and before the command line, only when the
  file placed no SCSI device and had no `[scsi]` table (an empty one means
  "none"). `MachineConfig::default()` places nothing.

## Rejected

- Holding the `ScsiBus` in the row (an `Arc<ScsiBus>` in each controller and a
  load per command).
- A pure pre-build layout plus a runtime map and a test to keep them equal:
  one map, filled once.
- One trait per family: one `DeviceType` and a family tag.
- A serde mirror struct for the config: `extra` plus `check_keys`.

## Open

- The listing's `IRIX` column is empty until a boot log shows the guest's
  names for the places.
- `linkme` entries could be dropped at link time on a target that treats an
  unreferenced static in an archive member as dead. Every binary that links
  the library has a test that its device types are listed; a missing device
  type also fails `--device TYPE@...` with an unknown-device error. The named fallback is an
  explicit `register_all()` returning the same list.
