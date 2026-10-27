# IRIX panics early on a headless O2

Status: open (2026-10-26).

**Symptom.** `headless = true` (no PS/2 keyboard, serial console) with the
IRIX 6.5.22 disk: the PROM menu comes up on serial, "1) Start System"
loads the kernel, and right after the banner:

    PANIC: TLBMISS: KERNEL FAULT
    EXC code:8, `Read TLB Miss '
    Bad addr: 0xff800000

The PC varies between runs (0x800d1430, 0x800f7318, 0x802cdf14); the bad
address does not. Same at 504ca31d (before the 64-bit bus), with and without
`lightning`, with 256 MB or 1 GB. The graphics console (keyboard attached)
boots the same disk to login. Not a regression: IRIX was never booted
headless; the bring-up log has only the PROM and NetBSD on serial.

**Reading the address.** 0xff800000 is not what the kernel dereferenced.
At 0x802cdf14 the instruction is `lw v1, 40(s3)` with `s3 = 0`: a NULL
struct pointer, VA 0x28. The user refill handler fetches that page's PTE
through Context's PTEBase, which IRIX points at its K2 page-table window at
0xff800000; nothing is mapped there that early, so the nested miss is the
panic and BadVAddr is the window. `badvaddr == 0xff800000` therefore means
"NULL (or other low user address) dereference in kernel mode".

**Ruled out.** CRIME's revision (now always 0xa1) and MACE's revision bit
(now always set) — both were tied to the keyboard, both fixed, neither
changes the panic.

**Leads.** The faulting code loops with a retry count of 100 and touches
fields at 544/548 of the NULL struct; nearby registers hold CRIME
(0xb4000000), CRE (0xb5000000) and GBE (0xb6000000) addresses. Candidates:
the kernel's PS/2 driver meeting an empty port that answers differently from
real hardware, or the graphics driver expecting state the PROM sets up only
when it opened the graphics console. A kernel symbol map for the 6.5.22 IP32
`/unix` (see the `irix-kernel-debug` skill) would name the function.
