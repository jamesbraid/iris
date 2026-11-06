# Boot chimes of the Indy and Indigo2 PROMs, and how to check them

**Keywords:** boot chime,startup tune,hal2,hpc3,pbus,adpcm,karplus-strong,ip22,ip24,ip28,ip30,octane,rad1,IRIS_HAL2_AUDIO_DUMP,IRIS_OCTANE_AUDIO_DUMP,070-1367-012,070-9101-011,070-1477-002

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

## Octane (IP30)

The Octane plays its tune through the RAD1 PCI audio card, not HAL2. The PROM
is not embedded; checked here: 6.5.30, sha256
`ee4722ca6f9b821ffadf156346c4d69bf5be71fc4b911cee75e36a1f51a49972`.

The tune is stored, like the Indy's: 31941 bytes of 4-bit IMA-style ADPCM,
high nibble first, decoded by the PROM into RAM when it chimes. In the PROM
image (file offsets): the stream is at 0xa37c0, its byte count at 0xab488, the
16-entry index-adjust table at 0xa3608 and the 89-entry step table at 0xa3648.
Predictor and step index start at 0. Each decoded 16-bit sample is stored in
bits [23:8] of both words of a frame, so the tune is mono at full level (peak
27717 of 32767) and unattenuated. 63882 samples are decoded; the buffer then
ends in silence, 72064 frames in all (the decoded frames plus 0x10000 bytes,
rounded down to 128-byte units). The tune sounds from frame 2955 to 63881.

PROM code, as addresses in the 0xbfc00000 window: the tune chosen by number at
0x9fc34e90 (three tunes; the boot chime is number 0, the other two are 20194
and 14158 bytes, read from the code and not run), the decoder at 0x9fc34f78,
the RAD1 programming and start at 0x9fc3536c to 0x9fc35434. The PROM loads
clock generator 2 with control word 0x40000405 (divide by 6), remainder word
0xffff, selects the 192 kHz master with a write of 1 to 0xac, points the
output ring at the buffer with one end-of-descriptor entry and starts it with
0x40000001 in register 0x58. Decoded with the public Linux driver's rules
(`snd_rad1_set_cg`), that is exactly 32000 Hz, so the tune lasts 2.252 s. The
32 kHz rate is the PROM's programming; the stream itself carries no rate, and
nothing here compares it with a recording from a real Octane.

Iris plays the PROM's output unchanged. Checked 2026-11-05 with
`IRIS_OCTANE_AUDIO_DUMP=<file>` (records the card's frames at its own rate, in
the HAL2 dump layout, with or without a host sound device) and
`IRIS_OCTANE_AUDIO_WAV=<file>` (44.1 kHz stereo, after the shared resampler):

- The dump is 72064 frames at 32000 Hz, left equals right, and matches an
  independent decoder written from the PROM's tables and the arithmetic above
  with 0 of 72064 samples different. Nothing is cut off at the end (the whole
  ring plays once and the enable bit clears) and there is no extra silence.
- The WAV is 99314 frames at 44.1 kHz, and matches a separate float32
  Catmull-Rom model of the shared resampler with 0 of 99314 different.
  Resampling to a 48 kHz host uses the same code with another output rate.
- A first boot with a blank NVRAM and flash chimes (unlike the Indy, which is
  silent on a new NVRAM) and a later boot on the NVRAM the first one wrote gives
  the identical stream (same SHA-256 of the samples).
- The run needs the config's SCSI image files to exist. The tune is in the dump
  within a minute on the interpreter.

One real problem turned up in the host path. The card makes each frame when its
clock says it is due, so the host ring never held more than the last 10 ms
pump interval, but a host callback asks for a whole period. With a modelled
512-frame period at 48 kHz the first ten callbacks of the tune found the ring
short, and any late pump afterwards would again. RAD1 now holds back the first
50 ms of a run before the host hears it (HAL2 has its own 20 ms prebuffer), and
releases what it holds when the run ends. This is checked by a modelled host
only. No sound device was available here, so the real stream
(`IRIS_OCTANE_AUDIO_CAPTURE`) was not recorded or listened to.
