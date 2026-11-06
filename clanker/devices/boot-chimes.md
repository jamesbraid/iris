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

All three PROMs program the audio clock explicitly, early in start-up (0xbfc00ba0 to
0xbfc00be8 on the IP22, 0xbfc00c44 to 0xbfc00c8c on the Indy, 0xbfc00d64 to
0xbfc00d94 on the IP28): HAL2 Bresenham generator 1 is pointed at the 44.1 kHz
master with increment 1 and modulus 1, so 44100 Hz is what the PROM asks for,
not a default iris assumes. The IP28 reads the HAL2 revision register at
0xbfc00d1c (early return when its top bit is set, 0xbfc00d24) and again at
0xbfc03438 in its tune player at 0xbfc03404, which also returns early when a
board-revision test at 0xbfc03418 fails and when the NVRAM variable it fetches
at 0xbfc03448 is zero. A 1995 post by an SGI engineer (comp.sys.sgi.misc,
"How to get the Boot Tune...") says the Indy tune is ADPCM, about two seconds
long, and that the Power Indigo2 (R8000) had dropped its sampled tune for a
synthesized one when the PROM ran out of space, which fits what the Indy and
the later Indigo2 PROMs hold.

## Older and other revisions

A mirror of an SGI PROM collection (Library root irix-mirrors,
`mirror.rqsall.com/prom/`, on this host under
`/storage/software/irix/mirrors/`) holds more revisions than iris embeds. They
are third-party hosted firmware with no redistribution grant: use them in place
with `prom = "<path>"` and never commit them.

| Part | Version string | Chime |
|---|---|---|
| 070-8127-002 | 5.1 Rev B IP22, Sep 16, 1993 | stored ADPCM, 87492 samples (1.98 s) at 44.1 kHz; same decoder tables as the Indy |
| 070-9101-007 | 5.0 Rev B6 IP24, Sep 28, 1994 | has the ADPCM step table; not run |
| 070-1361-003, -004 | 6.0.1 Rev A IP26 (Power Indigo2), Dec 9 and Dec 23, 1994 | synthesized strings with a different table (about 234, 295, 330, 443, 467, 592, 663, 706 Hz); not run |
| 070-1477-001 | 6.2 Rev A IP28, Jun 10, 1996 | same first twelve voice rows as 070-1477-002 |

The older IP22 PROM was run in iris with `prom` set to it: codec A played
87492 frames at 44100 Hz, identical sample for sample to an IMA-style decode of
the PROM's own ADPCM data, and equal to the first 87492 samples of the
`ip22boottune.wav` that sits beside it in the mirror. A spectrogram comparison
of that wav against the user's recording scores 0.91 at the same pitch (every
other candidate scored 0.25 or less), so the user's tune is this
sampled one, from a PROM older than the 5.3 Rev E iris embeds. With a blank
NVRAM this PROM stayed silent for 280 s; a second boot on the NVRAM the first
had written chimed within 200 s, consistent with the NVRAM gate the Indy shows
(two runs, the gate itself not isolated).

## The user's recording

A user supplied a recording of a real Indigo2 (8-bit mono, 11025 Hz, 1.46 s,
quiet and noisy, no bass). It is not the Indy ADPCM tunes and not the IP22 or
IP28 tune 0 at its own pitch of the PROMs iris embeds; its best spectral fit
among those was tune 0 played 1.19 times faster, a coincidence of triad
partials. It is the sampled tune of the older IP22 PROM above. The embedded
5.3 Rev E PROM synthesizes instead, so iris plays a different tune than a
machine with an older PROM does, and is right for the PROM it carries.

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
