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

Both players return early when the HAL2 revision register's top bit is set or
when a value they read from NVRAM is zero (the volume setting, inferred from the
code, not confirmed). Observed in iris: the first
boot of a new Indy `nvram.bin` is silent and the next boots chime; an Indigo2
with no `nveeprom` file chimes on its first boot.

## Other tunes in the same PROMs, and the clocks they use

Each PROM holds more than the power-on tune, selected by the number the player
is called with (0 at power-on). What calls them was not traced; nothing
in iris reaches them today.

| PROM | Tune 0 | Tune 1 | Tune 2 |
|---|---|---|---|
| Indy 070-9101-011 | ADPCM, 92150 samples (2.09 s) | ADPCM, 74460 samples (1.69 s) | ADPCM, 30200 samples (0.68 s) |
| Indigo2 070-1367-012 | seven strings, A major, rising | five strings (873, 741, 663, 555, 439 Hz), 8000 samples apart, falling | six strings strummed three times 22050 samples apart (156 to 278 Hz, a dissonant cluster; probably the failure sound, not confirmed) |
| Indigo2 Impact 070-1477-002 | same as the IP22 | same as the IP22 | not in the table (its rows after tune 1 differ and are not voices) |

Indy tune 1 and 2 are stored at 0x9fc60ec0 (length word at 0x9fc6a030) and
0x9fc6a034 (length at 0x9fc6db30); each length is in bytes, two samples per
byte. IP22 voice rows are 32 bytes: delay length, sample count, start offset,
peak, then four single-precision coefficients. Tune 1 starts at row 7 of the
table and tune 2 at row 12.

Both PROMs program the audio clock explicitly, early in start-up (0xbfc00ba0 to
0xbfc00be8 on the IP22, 0xbfc00c44 to 0xbfc00c8c on the Indy): HAL2 Bresenham generator 1 is pointed at the
44.1 kHz master with increment 1 and modulus 1, so 44100 Hz is what the PROM
asks for, not a default iris assumes. A 1995 post by an SGI engineer (comp.sys.sgi.misc,
"How to get the Boot Tune...") says the Indy tune is ADPCM, about two seconds
long, and that the Power Indigo2 (R8000) had dropped its sampled tune for a
synthesized one when the PROM ran out of space, which fits what the Indy and
the later Indigo2 PROMs hold.

## A recording that does not match

A user supplied a recording of a real Indigo2 (8-bit mono, 11025 Hz, 1.46 s,
quiet and noisy, no bass). It is not the Indy ADPCM tune (any of the three
samples, after resampling over 0.6x to 1.8x: no better than chance) and not the
IP22 or IP28 tune 0 at its own pitch. The strongest partials sit at 261, 329,
392, 524, 660 and 786 Hz, a C major arpeggio, and the best spectral fit among
the tunes in the three local PROMs is tune 0 played about 1.19 times faster,
which is an A major arpeggio moved up a minor third. That fit is suggestive,
not proof, and the PROM does not program a clock that would produce it. The
likeliest explanation is a different PROM revision (or an R8000 Power Indigo2)
with its own table. The version string the machine prints at power-on would
settle it; none of the three embedded PROMs is that revision.

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
