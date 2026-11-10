# Park only a loop proven to be a spin

The PC-cluster detector says where a processor has been running, not whether
it is waiting. It also accepts busy waits, and a park that lasts until an
interrupt (or one slice) slows those down or hangs them: IRIX 6.5.30 on the
Tezro and 6.5 on the Octane did not reach login with it. The detector is now
only a filter. Before a park the run loop runs the loop once more under
observation and parks only on a proof; `src/cpu/idle_park.rs` carries the
argument.

## What the IRIX idle loop looks like on the R10000 family

Seen live through the probe, at an idle shell:

- Octane (IP30, R12000, 6.5): the idle path returns to the same registers and
  memory every 2-4 passes through the probe's starting PC; 121 PCs, 15-30
  lines. It stores every pass, to its stack only (return addresses, a flag
  set and cleared), and memory comes back each period.
- Tezro (IP35, R16000, 6.5.30): the same shape over 231 PCs and 22-47 lines.
  Stack words change within a pass and come back.

So a proof that forbids stores refuses the real idle loop; one that requires
memory to return with the registers accepts it. After an interrupt IRIX
re-enters idle with different values in registers the loop never uses (s1,
s2, lo, the LL state), so a proof cannot be reused by comparing whole states;
the run loop re-probes as soon as the PC is back in the proven loop's code.

## Measured (one boot each, host load 300-560)

Idle shell, iris utime+stime over 300 s: Octane 46 % of a host core (101 %
without parking), one-processor Tezro 67 % (103 %). A fresh disk's first boot
runs `/etc/rc2.d/S88configmsg` (makewhatis, manAdmin, sgreader) for many
minutes; measure idle only after it stops.

## Rules

- A loop that reads Count or Random is never a spin, even if its registers
  repeat now: it can leave by time alone.
- A successful SC on several processors is a host compare-exchange that
  bypasses the store hooks, so a loop with SC is refused.
- Memory a parked loop watches is compared every `IRIS_IDLE_POLL_US`
  (200 us): stores by other processors and devices reach memory without
  passing anything a sleeping thread can wait on. User-mode
  MONITORX/MWAITX wakes in microseconds on Zen 3 but keeps the thread on the
  CPU as the host counts it, so it does not save host CPU.
- The probe decodes each load and store itself before the interpreter runs
  it, so it needs no breakpoint checks and works in `lightning` builds.
