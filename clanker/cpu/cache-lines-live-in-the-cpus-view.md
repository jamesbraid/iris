# Cache lines live in the CPU's own view of memory

The R10000-family PROMs use the caches as memory before memory works. They make
lines valid with Index Store Tag and Index Store Data and then load, store and
fetch through them, while the uncached path and every other master see memory
(NEC VR10000 user's manual p. 75). What the shipped PROMs do on iris:

| Machine | Lines | Memory behind them |
|---|---|---|
| O2 | secondary and primary data lines for page 0 and 0x1000 hold POST's stack for about 50-65 ms, while POST tests page 0 uncached | yes |
| Octane | a tag and data RAM test on secondary set 0, read back only with Index Load Tag/Data | no: the patterns name no memory |
| Fuel | primary data lines for 0x100000-0x107fff hold the stack; primary instruction lines hold PROM code | no, until memory is configured mid-phase |
| Tezro | the same as the Fuel, on every processor at once, with different stack contents at the same addresses | the same |

Every line ends by Index Store Tag. None is written back, and none is read back
with Index Load Data while it holds a page.

## How

- Each CPU reads memory through a window of its own (`PpMemSpace::add_cached_view`).
  Every mapping laid in the shared window is laid in each view too, and the view's
  region bitmap goes to that CPU's inline fields. The shared window, which
  `Physical`'s direct paths, CRIME's rendering engine and HEART read, never holds a
  line.
- When Index Store Tag makes a line valid, its page becomes private memory in that
  view, a copy of memory, and the line's data array is written into it
  (`ShadowCache::line_enters`). An invalid or different tag ends the line, and the
  page goes with its last line. The shadow's tags stay the only line state, with a
  held bit beside each. Index Store Data into a held line writes through.
- A page with no memory behind it holds nothing: the line stays in the arrays, as
  a tag test's patterns do. The region bitmap is all a CPU tests, so a held page's
  whole region is the view's while it stands, reading zeros wherever nothing is
  mapped. The O2's POST holds lines for page 0 before CRIME maps the rest of that
  region.
- A remap saves and restores held pages, so memory configuration mid-phase keeps
  the stack.
- Loads, stores, fetches, the decode cache and compiled code are untouched: they
  read the view through the base pointer and bitmap they always had. The decode
  cache compares every word it serves, so it sees a page change hands.

## Limits

Each is an approximation, stated where it is made.

- Page, not line: the rest of a held page shows memory as it was when the page was
  taken, uncached writes to it meanwhile are not seen by cached reads, cached stores
  to it are dropped at release, and a line that ends leaves its bytes until the
  page's last line goes. The page is the host granule: 4 KB on Linux and x86 macOS,
  16 KB on Apple silicon, 64 KB on Windows.
- Lines end only by a tag change the shadow already models: Index Store Tag, and
  Index Writeback Invalidate (D) zeroing the tag from the R12000 on. Nothing is
  written back. The hit operations and the other index invalidates leave a held
  line as they leave every shadow tag, untouched.
- Index Load Data reads the data array, not the page, so it does not see cached
  stores.
- A region holding a page reads zeros in its unmapped gaps for that CPU's cached
  accesses, where the bus would answer.
- A secondary doubleword puts TagLo at the lower address. Neither NEC manual says
  which half goes where; this order is the only one under which the Tezro's staged
  PROM code equals its own image. With the other order the PROM ran its long cache
  test uncached, taking twice as long.
- No replacement, parity or ECC, and no coherence with other CPUs or DMA.
- A snapshot taken while lines hold pages is refused at restore.

## Compiled code

Compiles read memory and compiled code is shared by every CPU, so a page some CPU
holds is not compiled. Holding a page adds `GEN_HELD` to the generation counter its
compiled code checks (the one behind the cache's bus), which retires what was
compiled before. The compile that finds the mark (`comp::snapshot_page`) publishes
no code for that generation and denies every offset, so arrivals interpret the page
without asking again. The last CPU's release takes the mark off with one ordinary
bump, which lowers the counter below that publish's generation, and the next
compile allows every offset again. All of it is at compile time: dispatch is
unchanged, and every other page stays compiled. Without the denial four Tezro
processors asking for the same held PROM page took 65-77 s to the BASEIO monitor,
against 38-42 s with it.

Where a ROM's pages share one counter, holding any of them keeps all of them from
being compiled while it stands.

## Seeing it

`l2 probe 0` in the monitor prints how many lines hold pages and which. Every
machine should show none at the IRIX login. `log l2c mask op` logs each page held
and released.
