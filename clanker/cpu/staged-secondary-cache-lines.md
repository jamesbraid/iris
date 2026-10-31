# Staged cache lines: a hit until an explicit operation ends them

The R10000-family shadow cache serves a cached load, store or fetch from a
secondary line the guest staged with Index Store Tag (S) and Index Store Data
(S), and a fetch from a primary instruction-cache line staged with the (I)
forms. The module doc in `src/cpu/mips_cache_shadow.rs` says why this is the
one exception to "the shadow is not consulted"; the code and its comments say
how. This note keeps what is not obvious from them.

Serving is opt-in per type (`model::STAGING`, `R10000StagingShadowCache`); every
other shadow cache compiles the staged-line branches out, so a machine that
never stages a line pays nothing. A page with a staged line stays off the
micro-TLB and out of the JIT while the line lives, and each access to it takes
the slow path through the cache.

## The limit: no replacement

An ordinary cached miss never fills the shadow, so it never evicts. A staged
line stays until an invalidate, an Index Store Tag that overwrites it, or
power-on. A guest that reuses the memory without invalidating reads the stale
line, and its stores stay in the line, where hardware would have evicted it and
written a dirty one back. Shipped PROMs invalidate what they stage, so the
`staged_any()` branch is not taken once an OS runs.

## Why the page fast paths must be told

Translations are cached per page. The interpreter's and the JIT's inline loads
and stores reach RAM through the micro-TLB and ppmem's window without calling
the cache, and the JIT compiles code from memory. So:

- `nutlb_fill` declines a page that holds a staged line (`MipsCache::page_staged`);
- `jitv2_track_pcp` leaves such a page to the interpreter;
- a CACHE instruction that changes the set of staged pages calls
  `nanotlb_invalidate` (`take_staged_changed`), because earlier entries do not
  know;
- the decode cache reads such a page word by word (`DecodeCache::set_bus_only`).

Dropping any of the first three fails
`a_compiled_load_hits_a_staged_line_not_the_window`,
`a_compiled_store_to_a_staged_line_stays_in_the_line` or
`a_staged_line_is_fetched_as_code_instead_of_the_compiled_page`; they run under
`jitv2` and under `lightning,jitv2`.

## Inferred or absent

- The TagLo word of a staged doubleword lands at the lower address
  (`STAGED_TAGLO_AT_LOWER_ADDRESS`). Inferred from software, not measured.
- A store to a staged line makes it DirtyExclusive. The manual counts such
  requests (NEC U10278EJ4V0UM p.246) but does not state the transition.
- The R10000's manual is the evidence. The R12000, R14000 and R16000 use the
  same tag layout here, which is an inference.
- Not modelled: primary data cache staged hits, ECC and check bits, the purge
  a cached access makes when its VA[13:12] differs from the tag's recorded
  index, the BadVAddr/Context/EntryHi values after a CACHE-raised TLB
  exception, Status.CH, the tag rewrite on invalidate (only the state clears),
  and coherence with other CPUs and DMA.
- Staged lines are saved in snapshots (`save_cache_state`); the rest of the
  shadow arrays, which only diagnostics read, are not.
