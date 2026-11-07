# Staged secondary-cache lines: a hit until an explicit operation ends them

The R10000-family shadow cache (`src/cpu/mips_cache_shadow.rs`) serves a cached
load, store or fetch from a secondary line the guest staged with Index Store Tag
(S) and Index Store Data (S): a valid state in TagLo[11:10] and a tag equal to
PA[39:18] of the access. The rules are the R10000 cache index operations datasheet's
datasheet. A guest that never stages a line pays nothing: no access is tested
for one.

## The limit: no replacement

An ordinary cached miss never fills the shadow, so it never evicts. A staged
line stays until Index Writeback Invalidate, Hit Invalidate, Hit Writeback
Invalidate, an Index Store Tag that overwrites it, or power-on. A guest that
reuses the memory without invalidating reads the stale line and its stores stay
in the line, where hardware would have evicted it, writing a dirty one back to
the tag's address.

Staged lines left at the login prompt, `l2 probe 0` in the monitor, CPU 0:
Octane (IP30 PROM, IRIX 6.5.30) 0, O2 R10000 (IRIX 6.5.30) 0. The shipped PROMs invalidate what they stage, so the machine is
back on the window paths and the JIT before IRIX runs.

## How staged lines reach the data path

No access is checked against them. While the cache holds any, it takes RAM
off its own fast paths instead (`serve_window`):

- `read`, `write` and the compare-and-swap paths stop using ppmem's window
  (`window_regions`), so every cached access takes the bus path, which goes
  through a `StagedBus`: staged lines answer for their addresses, memory for
  the rest;
- the decode cache gets no window (`fetch_window`), so it reads every word
  through the same route;
- the executor runs no compiled code (`staged_lines_changed`). The JIT's inline
  loads and stores reach RAM through the micro-TLB and ppmem's window without
  calling the cache, and it compiles from memory. The executor learns of the
  change from `take_staged_changed`, asked after each CACHE instruction.

The last line ending undoes all three. While lines are held the whole machine
runs in the interpreter on the bus path, which is slow; the shipped PROMs end
what they stage before the kernel runs.

Dropping the executor's part fails `a_compiled_load_hits_a_staged_line_not_the_window`,
`a_compiled_store_to_a_staged_line_stays_in_the_line` or
`a_staged_line_is_fetched_as_code_instead_of_the_compiled_page`; those run under
`jitv2` and under `lightning,jitv2`.

## Inferred or absent

- The TagLo word of a staged doubleword lands at the lower address
  (`STAGED_TAGLO_AT_LOWER_ADDRESS`). Inferred from software, not measured.
- A store to a staged line makes it DirtyExclusive. NEC U10278EJ4V0UM p.246
  counts such requests; the datasheet does not state the transition.
- Every CACHE operand is translated first (exceptions: TLB Refill/Invalid, privilege Address Error, never TLB Modified). The secondary set, way and quadword come from the operand's physical address, which the executor passes in place of the virtual one, the primaries' from its virtual address. An operand on an uncached page is undefined on the R10000 and goes ahead as if cached.
- The datasheet clears the R10000. The R12000 and R14000 use the same
  tag layout here, which is an inference.
- Not modelled: predecode, primary-cache staged hits, ECC and check bits, the
  virtual-index purge, the BadVAddr/Context/EntryHi values after a CACHE-raised TLB exception, Status.CH, the
  tag rewrite on invalidate (only the state clears), coherence with other CPUs
  and DMA.
- Staged lines are saved in snapshots (`save_cache_state`). The rest of the
  shadow arrays, which only diagnostics read, are not.
