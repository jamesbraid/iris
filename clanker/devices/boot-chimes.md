# Boot chimes of the Indy and Indigo2 PROMs, and how to check them

**Keywords:** fuel,tezro,ip34,ip53,ip35,no chime,boot chime,startup tune,hal2,hpc3,pbus,adpcm,karplus-strong,ip22,ip24,ip28,IRIS_HAL2_AUDIO_DUMP,070-1367-012,070-9101-011,070-1477-002

The Indy and Indigo2 chimes are not samples iris carries. The PROM builds the
tune in RAM with its own code and starts HAL2 playing it through an HPC3 PBUS
DMA channel, so the sound is right exactly when the CPU, FPU, HPC3 and HAL2
models are. Nothing in iris picks or substitutes a tune.

## Where each tune comes from

| Machine | PROM (embedded) | Tune | Length at 44.1 kHz |
|---|---|---|---|
| Indy IP24 | 070-9101-011 | 4-bit ADPCM, high nibble first, 89-entry step table, stored in the PROM | 92150 samples, 2.09 s |
| Indigo2 IP22 | 070-1367-012 | Synthesized: seven plucked strings, no stored sample | 85000 samples, 1.93 s |
| Indigo2 Impact IP28 | 070-1477-002 | The same seven-string tune, compiled for the R10000 | 85000 samples, 1.93 s |

The IP22 and IP28 PROMs hold the same voice table: delay lengths 200, 159,
133, 100, 79, 66 and 50 samples (an A major arpeggio, 220 Hz to 882 Hz) that
start 0, 6000, ... 36000 samples in, each 88200 samples long, mixed and cut at
85000. Each string is a delay line seeded with white noise from a linear
congruential generator (multiplier 0x41c64e6d, increment 12345), centred and
scaled to the voice's peak, then run through a two-point average, a decay
factor, and a first-order all-pass for tuning. All arithmetic is single
precision, and the sample added to the mix is the delay line's output truncated
toward zero. The IP22 PROM's noise seed is a stack variable it never sets (it
reads zero in iris, from the stack the PROM cleared); the IP28 PROM starts
every string's seed at 0x41c60000, which is what makes the model match. That
is why the two chimes are the same tune with different noise.

PROM code, for reading with a disassembler (addresses in the 0xbfc00000 PROM
window):

- IP22: the tune player is at 0xbfc03360 and the voice table is at 0xbfc5dba4.
  The player is called with tune 0 at power-on (0xbfc02358). Tunes 1 and 2 are
  other voice tables. HAL2 set-up is 0xbfc02ffc and 0xbfc0311c.
- IP28: the voice table is at 0xbfc5b200.
- IP24: the player is at 0x9fc030b4, called with tune 0 at 0x9fc02180. It copies
  its decoder into RAM and runs it from there; the step and index tables are at
  0x9fc55954 and 0x9fc55914, the sample at 0x9fc55ac0 and its length in bytes at
  0x9fc60ebc.

Both players return early when a value they read from NVRAM is zero (the volume
setting, inferred from the code, not confirmed). Observed in iris: the first
boot of a new Indy `nvram.bin` is silent and the next boots chime; an Indigo2
with no `nveeprom` file chimes on its first boot.

## Recording what HAL2 plays

`IRIS_HAL2_AUDIO_DUMP=<file>` writes every frame codec A plays, at the codec's
own rate and before resampling, with or without a host sound device. Records
are the rate (u32) and the frame count (u32), then that many left, right pairs
of little-endian i16. The chimes are mono, so left equals right.

```sh
IRIS_HAL2_AUDIO_DUMP=chime.dump iris --headless --config indigo2.toml
```

The PROMs take a while: expect the IP22 tune about 60 to 100 s after start on
the interpreter. The IP28 needs `banks = [512, 512, 0, 0]`: with 128 MB banks
it was still in a delay loop after three minutes and never chimed (cause not
investigated).

## Result of checking them (2026-11-05)

Each embedded PROM was run headless to its chime and the dump compared with an
independent model written from the PROM's table and arithmetic (float32 for the
synthesized tunes, a plain IMA-style decoder for the Indy), sample for sample:
0 of 85000 differ on the IP22, 0 of 85000 on the IP28, 0 of 92150 on the Indy.
The comparison covers the CPU and FPU through HPC3 PBUS DMA and HAL2's
rate and DMA handling; it does not cover the host audio device or resampler.

## Fuel (IP34) and Tezro (IP53): no power-on tune (2026-11-05)

Both machines run the same IP35 PROM 6.210 payload (`ip35prom.payload.bin`,
sha256 554924380189a03a512156af2faa38344902f251b49fd5c80c253c23ecaff27b,
matches the project asset manifest). Conclusion: this PROM plays nothing at
power-on, so there is nothing for iris to reproduce. What that rests on:

- Hardware. SGI's Tezro hardware user's guides (007-4564-001, 007-4643-003)
  put the audio jacks on the I/O daughtercard, an extension of the IO9 board,
  and show the audio option on a PCI bus. The Fuel hardware guide (007-4480-001)
  lists no audio jacks; the Fuel gets sound from a separate USB converter
  (007-4530-001). Both guides list the PROM power-on diagnostics, in order:
  CPU, Bedrock, PROM, DIMMs, secondary cache, PCI bridge (Fuel: Xbridge; Tezro:
  PIC), PCI slots, serial, SCSI, (Fuel: keyboard and mouse), VPro. Neither
  names an audio test, a tone or a beep. The only audible item either guide
  mentions is the "beeping keyboard" peripheral, which the keyboard makes
  itself. The Fuel claim of "no jacks" rests on a text search of the guide, not
  a read of the rear-panel figure.
- Strings. The PROM payload is a processor PROM followed by a gzip member at
  0x8bbb0 that unpacks to the 1468990-byte base I/O PROM (byte-identical to
  `io7prom.bin`). Neither holds a string about a chime, tone, beep, volume,
  sample, mixer or codec. "audio" appears only in the hardware-graph device
  type list (0x1012e0 in the unpacked image), "sgirad" beside it, "HAL2" and
  "Iris Audio Processor" in the shared inventory-print table (0x119050), and
  SGI_RAD / PSITECH_RAD1 in the processor PROM's table of PCI device names for
  its configuration-space diagnostic (0x84b38, 0x84b80). Those are labels, not
  playback code. This is string evidence only; I did not read the code.
- Runs. Headless iris (tezro-mp build), Fuel profile with 256+256 MiB and
  Tezro profile with one CPU, no real disk. The Fuel PROM prints the keyboard
  and mouse, graphics and SCSI diagnostics, the summary "ALL DIAGS PASSED" and
  "Starting up the system"; the Tezro PROM reaches the System Maintenance Menu.
  No sound-related line appears in either console log.

iris models no audio device for either machine: `no_audio = true` in
`fuel.toml.example` and `tezro.toml.example` only turns off the host sound
output; neither machine file instantiates HAL2 or any PCI audio function. So a
guest or PROM that did talk to audio hardware would hit an empty slot.

What is not established: whether the Tezro audio daughtercard's chips (the
guide does not name them) are visible to the PROM, and whether a later PROM
revision beeps. The Tezro run also logged 19 bus errors on uncached 32-bit
reads at PA 0x020000000f0xxxxx to 0x...f8xxxxx after the menu appeared (PC
0xbfc47e58 in the PROM window); I did not identify what the PROM was probing,
and the pattern (4 KiB steps) looks like a memory-size or DIMM walk, not audio.
A real Fuel or Tezro would settle the question of an audible chime at power-on:
a user's report that a unit is silent at power-on, or a recording of one that
is not, is the evidence that would change this.
