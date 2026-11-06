# Boot chimes of the Indy and Indigo2 PROMs, and how to check them

**Keywords:** boot chime,startup tune,hal2,hpc3,pbus,adpcm,karplus-strong,ip22,ip24,ip28,IRIS_HAL2_AUDIO_DUMP,070-1367-012,070-9101-011,070-1477-002

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

## O2 (IP32)

PROM: the installer's `ip32prom.image` from IRIX 6.5.30 (431,872 bytes, sha256
11230c35...fb23c), firmware revision 4.18. The O2 has no embedded PROM; point
`prom` at the file.

**Why an earlier run heard nothing.** The file is not a bare flash image: a
256-byte header ("PROM", the payload length at +12) sits in front of the
431,616-byte payload. Loaded as is, the CPU (observed) spins in the PROM's fatal-error loop at 0xbfc004a0 (Status shows
a TLB-load exception, BadVAddr 0xfffffffffffffffc; the section
walk reading a wrong address is inferred), long before any audio
register is touched. `Ip32Bus::unwrap_prom_image` now strips the header and
pads the flash to 512 KiB with erased (0xff) bytes; a raw 512 KiB image is used
as it is.

**What the chime needs.** Nothing beyond a booting PROM. With `headless = true`
(no keyboard, no video head), no serial client, and a flash file that did not
exist, the tune plays within seconds of start; the PROM then reports that it
cannot reach a keyboard and shows its maintenance menu on the serial port. A
second boot with the saved flash plays the same samples.

**What it is.** Same plucked-string engine as the Indigo2 (the PROM's
noise generator, centring, scaling, two-point average and all-pass stage are
the same code), with a different table and a different player:

- The PROM has eleven sounds, each a table of 4 to 7 voices (32 bytes each:
  delay length, sample count 88200, start offset, peak, decay, two averaging
  weights 0.5, tuning coefficient). The power-on tune is the seven-voice table at
  0x81054ab4 in the firmware once it is copied to RAM (flash offset 0x5dcc4);
  delay lengths 168, 112, 133, 112, 125, 112, 83; start offsets 0, 6013, 12026,
  18039, 24052, 30065, 36078 words (about 88 ms apart).
- The synthesizer fills a buffer of 32-bit words, left and right alternating,
  each sample added to both words of a pair as `trunc(x) << 8`. Offsets are in
  words, so voices with an odd offset (1, 3, 5) put their sample in the right
  word first. The buffer is 124,278 words rounded up to 64 bytes (0x79600), so
  62,144 stereo frames. The PROM then copies it to the MACE audio output ring
  in 288-byte steps, waiting for room.
- Format at the codec: stereo, one sample pair per ring item, 16 bits at bits
  23:8 of each word. Rate 34000 Hz (AD1843 clock generator 1, register 17 =
  0x84d0), so 1.83 s. DAC1 analog gain is set to 13 in both fields (register 9 =
  0x0d0d), 7.5 dB below unity; the dump holds samples after that gain.
- Voice pitches measured in the model, one voice at a time: about 202, 302,
  254, 302, 269, 302 and 388 Hz (G3, D4, B3, D4, C4, D4, G4, each roughly a
  quarter tone sharp of equal temperament at A = 440).
- The noise generator's start value is a leftover on the PROM's stack, not a
  constant in its code: 0x810443f0 for the first three voices. The generator's
  state is written back only by the part of a voice's fill that handles delay
  lengths not a multiple of four, so the 133 and 125 voices (and the
  83) advance it by one and three steps and the others by none; later voices
  start at 0xdb921eb3 and 0x1a01e637. These values were read from the stack in
  iris; a different earlier boot path would give a different noise pattern.
- Voices run past the end of the buffer (88200 samples each against 62,139
  frames played); the overrun lands on the PROM's scratch delay line and is
  never played.

**Check (2026-11-05).** `modelo2.py` (evidence below) rebuilds the tune from the
flash bytes of the table and the arithmetic above in float32: 0 of 124,288
words differ from `IRIS_O2_AUDIO_DUMP`, for the first and the second boot,
and the dump is silent after the tune. The model depends on the stack value
and on the codec gain being as the PROM sets them; the gain formula is the one
iris already uses for the AD1843 (1.5 dB steps), so that part is a shared
assumption, not independent. Not covered: the host audio device and resampler,
a machine with a keyboard and video head, and any real O2 recording.

`IRIS_O2_AUDIO_DUMP=<file>` records: output number (u8), rate (u32), pair count
(u32), then left, right i16 pairs, little-endian. The dump is paced by wall
clock, with silence for a ring that runs dry; this run had no gaps inside the
tune.

**Real hardware.** No recording or measurement of a real O2 chime was found in
public sources. Forum threads confirm a working O2 plays a start-up sound at
power-on ([comp.sys.sgi.hardware](https://groups.google.com/g/comp.sys.sgi.hardware/c/N79_XRWMHsM)),
and an IRIX Network thread says SGI boot sounds were reverse engineered from
the PROMs and that some are generated on the fly instead of stored
([irixnet](https://forums.irixnet.org/thread-2291-page-3.html)). That fits what
this PROM does, but neither source states the O2's rate, notes or length.
