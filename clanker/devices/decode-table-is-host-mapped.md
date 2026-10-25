# The decode table is host-mapped and never written in place

## Status: landed 2026-10-24 (`src/ppmem/devmap.rs`)

`Physical::device_map`, the 64 KB-slot array of `*const dyn BusDevice`, is
gone. The table is a `SparseDeviceMap`: a 256 MB `AddrSpace` reservation
covering 40 bits of physical address, mapped page by page from shared
constant pages (one per device pointer) and a pool of mixed pages. Design
record: `docs/devmap-design.md`.

## Rules

- **Change entries only through `fill` / `fill_bytes`.** Every table view is
  read-only; a store through `base()` segfaults. `fill` builds the new page
  off-line and swaps it in, which is what keeps concurrent DMA dispatch
  safe. Do not add a `*mut` path.
- **Set the default before anything can dispatch.** `set_default` creates
  the constant page the fault handler maps. Without it a fault on an
  untouched page prints "no default device" and chains to the previous
  handler, which kills the process. `Physical::init` does this first.
- **A partial write to an untouched page pads with the default.** Writing a
  device into a region nobody filled gives you that device there and the
  default in the rest of the 256 MB page. That is what a 40-bit machine
  wants; it means `fill` on an untouched page fails if no default is set.
- **Pages are 64 KB on every host** (`granularity().max(64 KB)`), so one
  table page covers 256 MB of guest space and every mapping is one page.
  That is the Windows constraint (placeholders split on 64 KB, views cannot
  be partially unmapped) applied everywhere, so Linux exercises the same
  shape Windows needs.
- **Compare entries with `entry_eq`, not `addr_eq`.** `UnmappedRam` is
  zero-sized and shares its address with the next field of `Physical`; only
  the vtable half of the fat pointer tells them apart. `addr_eq` would call
  it equal to `AliasBus`.
- **The fault handler must stay allocation-free.** It takes the table's
  spin lock (bounded spin, gives up with a message), maps one page, writes
  one line with `write(2)`. Keep it that way; a `println!` or `Vec` in there
  is a deadlock waiting for a fault on the thread that holds the lock.
- **Debuggers stop on SIGSEGV.** `handle SIGSEGV nostop noprint pass` (and
  `SIGBUS`) in gdb. Only faults above 4 GB reach it on an Indy, so this
  rarely matters until the Octane.

## Not done

- The `BusDevice` trait still takes `u32`. The table is 40-bit so the bus
  can be widened without redoing the decode; nothing above 4 GB is reachable
  from the CPU yet.
- Windows backend written against the API, not run: same status as
  `map_windows.rs`. The devmap unit tests are the acceptance test.
- Shared pages are never freed (bounded by the number of distinct devices);
  pool pages are. Fine for now.
