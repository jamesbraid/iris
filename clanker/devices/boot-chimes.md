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

Both players return early when the volume setting in NVRAM is zero. The Indy
and Indigo2 (IP22) players read the NVRAM string at offset 232 (three bytes),
parse it as a decimal number and return before touching HAL2 if it is zero.
The IP22 and IP24 PROMs were checked this way, in the iris monitor (IP24
0x9fc030f4 to 0x9fc03124, IP22 0xbfc03380 to 0xbfc033a0). The IP28 player was
not disassembled; iris already seeded its EEPROM before the chime, which suggests the same gate.

## NVRAM and the first boot

The PROM fills in its default settings (volume "80", console "g", baud 9600,
time zone PST8PDT and a few flags) only when the NVRAM checksum is bad, and it
does so after it has played the tune. A zeroed `nvram.bin` (Indy) or an erased
`nveeprom.bin` (Indigo2 IP22) therefore reads as volume 0 at tune time: the
first boot is silent, the PROM then writes its defaults, and iris saves them
for the next boot. Nothing is wrong in the PROM or in the DS1386 or 93CS56
model. A real machine is programmed at the factory, so it chimes on first
power-on, and a blank battery-backed block is an artificial starting state.

iris now seeds the factory state for a block that is blank apart from the
Ethernet address (`src/dev/sgi_nvram.rs`): layout revision 8 for the Indy and
IP22, 9 for the IP28, the defaults above and a checksum. Existing files and
anything a guest has set are never touched. The checksum is XOR of every byte
but the first, seeded with 0xA5, rotated left once per 16-bit word; it
reproduces the 0xBA the Indy PROM wrote (checked on its whole image). The
text the PROM also leaves in unused slots is not copied.

Checked with `IRIS_HAL2_AUDIO_DUMP`, blank files, first boot: the Indy gives
92150 frames with the same PCM sha256 (4891e240...) as the earlier recording
and as a second boot; the IP22 gives 85000 frames, identical to the earlier
recording (59e2a08c...); the IP28 is unchanged (b434bab4...). Before the
change the first boots of the Indy and IP22 played nothing.

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
