# Boot chimes: what each machine's PROM plays, and how to check it

**Keywords:** boot chime,startup tune,power-on tune,hal2,hpc3,pbus,rad1,mace,ad1843,adpcm,karplus-strong,boottune,nvram,ip22,ip24,ip26,ip28,ip30,ip32,ip34,ip35,ip53,IRIS_HAL2_AUDIO_DUMP,IRIS_OCTANE_AUDIO_DUMP,IRIS_O2_AUDIO_DUMP

iris carries no chime samples. Each PROM builds or decodes its tune in RAM with
its own code and starts the audio hardware playing it, so the sound is right
exactly when the CPU, FPU, DMA and audio models are. Nothing in iris picks or
substitutes a tune. A different PROM revision plays a different tune; select
one with `prom = "<path>"` (see [Older PROM revisions](#older-prom-revisions)).

| Machine | PROM | Tune | Rate | Length |
|---|---|---|---|---|
| Indy IP24 | 070-9101-011 (embedded) | stored 4-bit ADPCM | 44.1 kHz | 92150 frames, 2.09 s |
| Indigo2 IP22 | 070-1367-012 (embedded) | synthesized, seven plucked strings | 44.1 kHz | 85000 frames, 1.93 s |
| Indigo2 Impact IP28 | 070-1477-002 (embedded) | same seven strings, R10000 build | 44.1 kHz | 85000 frames, 1.93 s |
| Octane IP30 | 6.5.30, supplied by the user | stored 4-bit ADPCM | 32 kHz | 72064 frames, 2.25 s |
| O2 IP32 | 4.18, supplied by the user | synthesized, seven plucked strings | 34 kHz | 62144 frames, 1.83 s |
| Fuel IP34, Tezro IP53 | IP35 PROM 6.210 | none | | |

All figures below are for the PROM named. PROM code addresses are in the PROM's
own address window, for reading with iris's monitor.

## Indy IP24

Stored tune: 4-bit IMA-style ADPCM, high nibble first, 89-entry step table.
The player is at 0x9fc030b4 and is called with tune 0 at 0x9fc02180. It copies
its decoder into RAM and runs it from there. The step and index tables are at
0x9fc55954 and 0x9fc55914, the sample at 0x9fc55ac0 and its length in bytes at
0x9fc60ebc. Two more ADPCM tunes follow (74460 and 30200 samples, at 0x9fc60ec0
and 0x9fc6a034); what calls them was not traced.

## Indigo2 IP22

Synthesized. The voice table (0xbfc5dba4) holds seven plucked strings with
delay lengths 200, 159, 133, 100, 79, 66 and 50 samples (an A major arpeggio,
220 Hz to 882 Hz) starting 0, 6000, ... 36000 samples in, each 88200 samples
long, mixed and cut at 85000. Each string is a delay line seeded with white
noise from a linear congruential generator (multiplier 0x41c64e6d, increment
12345), centred and scaled to the voice's peak, then run through a two-point
average, a decay factor and a first-order all-pass for tuning. Arithmetic is
single precision and the sample added to the mix is the delay line's output
truncated toward zero. The noise seed is a stack variable the PROM never sets;
it reads zero in iris, from the stack the PROM cleared.

The player is at 0xbfc03360, called with tune 0 at power-on (0xbfc02358). HAL2
set-up is at 0xbfc02ffc and 0xbfc0311c. Tunes 1 and 2 in the same table are the
stop tune and the bad-graphics tune, not extra start tunes: a five-string
falling chord (873, 741, 663, 555, 439 Hz, 8000 samples apart) and a strummed
six-string cluster (156 to 278 Hz, three strums 22050 samples apart). They
start at rows 7 and 12 of the 32-byte voice rows (delay length, sample count,
start offset, peak, four single-precision coefficients). Nothing in iris
reaches them.

## Indigo2 Impact IP28 and Power Indigo2 IP26

The IP28 PROM has the same voice table as the IP22 (at 0xbfc5b200) with one
difference that makes it a different recording: it starts every string's noise
seed at 0x41c60000 instead of reading an unset stack word. Same tune, different
noise. Its player is at 0xbfc03404; it reads the HAL2 revision register
(0xbfc03438) and returns early when a board-revision test (0xbfc03418) fails or
the NVRAM variable it fetches (0xbfc03448) is zero. The same revision test sits
at 0xbfc00d1c.

The IP26 and IP28 PROMs hold several start tunes and choose between them by
the one-character `boottune` NVRAM setting. iris does not override the choice.
It writes `boottune` only when it seeds a blank IP28 EEPROM (to `1`, below);
an existing setting is left alone and the PROM picks the tune. Which tune each
value selects was not traced in the PROM.

The IP28 needs `banks = [512, 512, 0, 0]` to chime: with 128 MB banks it was
still in a delay loop after three minutes (cause not investigated).

## Octane IP30

The Octane plays through the RAD1 PCI audio card, not HAL2. The PROM is not
embedded; checked with 6.5.30, sha256
`ee4722ca6f9b821ffadf156346c4d69bf5be71fc4b911cee75e36a1f51a49972`.

The tune is stored: 31941 bytes of 4-bit IMA-style ADPCM, high nibble first,
stored at 32 kHz (not 44.1 kHz) and decoded by the PROM into RAM. In the PROM
image (file offsets): the stream is at 0xa37c0, its byte count at 0xab488, the
16-entry index-adjust table at 0xa3608 and the 89-entry step table at 0xa3648.
Predictor and step index start at 0. Each decoded 16-bit sample is stored in
bits 23:8 of both words of a frame, so the tune is mono at full level (peak
27717 of 32767). 63882 samples are decoded; the buffer then ends in silence,
72064 frames in all. The tune sounds from frame 2955 to 63881.

PROM code (0xbfc00000 window): the tune is chosen by number at 0x9fc34e90
(three tunes; the boot chime is number 0, the others are 20194 and 14158 bytes,
read from the code and not run), the decoder at 0x9fc34f78, the RAD1
programming and start at 0x9fc3536c to 0x9fc35434. The PROM loads clock
generator 2 with control word 0x40000405 (divide by 6), remainder word 0xffff,
selects the 192 kHz master with a write of 1 to 0xac, points the output ring at
the buffer with one end-of-descriptor entry and starts it with 0x40000001 in
register 0x58. Decoded with the public Linux driver's rules (`snd_rad1_set_cg`)
that is exactly 32000 Hz. The stream carries no rate and nothing here compares
it with a recording of a real Octane.

`IRIS_OCTANE_AUDIO_DUMP=<file>` records the card's frames at its own rate, in
the HAL2 dump layout, with or without a host sound device.
`IRIS_OCTANE_AUDIO_WAV=<file>` writes 44.1 kHz stereo after the shared
resampler. Checked 2026-11-05:

- The dump is 72064 frames at 32000 Hz, left equals right, with 0 of 72064
  samples different from an independent decoder written from the PROM's tables.
  Nothing is cut off at the end and there is no extra silence.
- The WAV is 99314 frames at 44.1 kHz, with 0 of 99314 different from a
  separate float32 Catmull-Rom model of the resampler. A 48 kHz host uses the
  same code with another output rate.
- A first boot with blank NVRAM and flash chimes (the Octane has no volume gate
  like the Indy's); a later boot on the NVRAM the first wrote gives the same
  stream.
- The run needs the config's SCSI image files to exist. The tune is in the dump
  within a minute on the interpreter.

The card makes each frame when its clock says it is due, so the host ring never
held more than the last 10 ms pump interval, while a host callback asks for a
whole period. With a modelled 512-frame period at 48 kHz the first ten callbacks
of the tune found the ring short. RAD1 now holds back the first 50 ms of a run
before the host hears it (HAL2 has its own 20 ms prebuffer) and releases what it
holds when the run ends. The hold is per run, not per frame, so steady-state
output costs nothing extra. This is checked with a modelled host only: no sound
device was available, so the real stream (`IRIS_OCTANE_AUDIO_CAPTURE`) was not
recorded or listened to.

## O2 IP32

PROM: the installer's `ip32prom.image` from IRIX 6.5.30 (431,872 bytes, sha256
`11230c35...fb23c`), firmware revision 4.18. The O2 has no embedded PROM; point
`prom` at the file.

The file is a 256-byte header ("PROM", the payload length at +12) in front of a
431,616-byte payload. Loaded as is, the CPU spins in the PROM's fatal-error
loop at 0xbfc004a0 (observed; the section walk reading a wrong address is
inferred) before touching any audio register. iris now strips the header when
the magic and the length agree with the file size and pads the flash to 512 KiB
with erased bytes. A raw 512 KiB image is used unchanged.

The chime needs nothing beyond a booting PROM. With `headless = true`, no serial
client and a flash file that did not exist, the tune plays within seconds; the
PROM then reports that it cannot reach a keyboard and shows its maintenance
menu on the serial port. A second boot with the saved flash plays the same
samples.

The engine is the plucked-string synthesizer of the Indigo2 (same noise
generator, centring, scaling, two-point average and all-pass), with a different
table and player:

- Eleven sounds of 4 to 7 voices, 32 bytes each (delay length, sample count
  88200, start offset, peak, decay, two averaging weights of 0.5, tuning
  coefficient). The power-on tune is the seven-voice table at 0x81054ab4 once
  the firmware is copied to RAM (flash offset 0x5dcc4): delay lengths 168, 112,
  133, 112, 125, 112, 83; start offsets 0, 6013, 12026, 18039, 24052, 30065,
  36078 words (about 88 ms apart).
- The synthesizer fills a buffer of 32-bit words, left and right alternating,
  adding each sample to both words of a pair as `trunc(x) << 8`. Offsets are in
  words, so voices with an odd offset put their sample in the right word first.
  The buffer is 124,278 words rounded up to 64 bytes (0x79600): 62,144 stereo
  frames. The PROM copies it to the MACE audio output ring in 288-byte steps,
  waiting for room.
- Codec format: stereo, 16 bits at bits 23:8 of each word, 34000 Hz (AD1843
  clock generator 1, register 17 = 0x84d0). DAC1 analog gain is 13 in both
  fields (register 9 = 0x0d0d), 7.5 dB below unity; the dump holds samples
  after that gain.
- Voice pitches, one voice at a time in the model: about 202, 302, 254, 302,
  269, 302 and 388 Hz (G3, D4, B3, D4, C4, D4, G4, each roughly a quarter tone
  sharp of A = 440).
- The noise generator's start value is a leftover on the PROM's stack:
  0x810443f0 for the first three voices. Only the part of a voice's fill that
  handles delay lengths not a multiple of four writes the state back, so the
  133, 125 and 83 voices advance it by one and three steps and the others by
  none; later voices start at 0xdb921eb3 and 0x1a01e637. These were read from
  the stack in iris; a different earlier boot path would give different noise.
- Voices run 88200 samples against 62,139 frames played; the overrun lands on
  the PROM's scratch delay line and is never played.

`IRIS_O2_AUDIO_DUMP=<file>` records: output number (u8), rate (u32), pair count
(u32), then left, right i16 pairs, little-endian. It is paced by wall clock,
with silence for a ring that runs dry; this run had no gaps inside the tune.

Checked 2026-11-05: a model rebuilt from the flash bytes of the table and the
arithmetic above in float32 differs from the dump in 0 of 124,288 words, for the
first and second boot, and the dump is silent after the tune. The model depends
on the stack value and on the codec gain being as the PROM sets them; the gain
formula is the one iris already uses for the AD1843 (1.5 dB steps), so that part
is a shared assumption, not independent. Not covered: the host audio device and
resampler, a machine with a keyboard and video head, and any recording of a
real O2. No public recording or measurement of an O2 chime was found; forum
threads (comp.sys.sgi.hardware; an IRIX Network thread on SGI boot sounds being
recovered from PROMs, some generated on the fly) confirm a start-up sound but
not its rate, notes or length.

## Fuel IP34 and Tezro IP53

Both run the same IP35 PROM 6.210 payload (`ip35prom.payload.bin`, sha256
`554924380189a03a512156af2faa38344902f251b49fd5c80c253c23ecaff27b`, matching
the project asset manifest). It plays nothing at power-on, so there is nothing
to reproduce. The evidence:

- Hardware. SGI's Tezro hardware user's guides (007-4564-001, 007-4643-003)
  put the audio jacks on the I/O daughtercard and show the audio option on a PCI
  bus. The Fuel hardware guide (007-4480-001) lists no audio jacks; sound comes
  from a separate USB converter (007-4530-001; this rests on a text search of
  the guide, not the rear-panel figure). Both guides list the power-on
  diagnostics in order (CPU, Bedrock, PROM, DIMMs, secondary cache, PCI bridge,
  PCI slots, serial, SCSI, keyboard and mouse on the Fuel, VPro) and name no
  audio test, tone or beep. The only audible item they mention is the
  keyboard's own beep.
- Strings. The payload is a processor PROM followed by a gzip member at 0x8bbb0
  that unpacks to the 1468990-byte base I/O PROM. Neither holds a string about a
  chime, tone, beep, volume, sample, mixer or codec. Audio device names appear
  only in a hardware-graph device-type list and the inventory-print table, and
  in a PCI configuration-space diagnostic's name table: labels, not playback
  code. This is string evidence; the code was not read.
- Runs. Headless runs on a Fuel profile (256+256 MiB) and a one-CPU Tezro
  profile with no real disk: the Fuel PROM prints the keyboard, mouse,
  graphics and SCSI diagnostics, "ALL DIAGS PASSED" and "Starting up the
  system"; the Tezro PROM reaches the System Maintenance Menu. No
  sound-related line appears in either console log.

Neither machine instantiates HAL2 or any audio function in iris. Not
established: whether the Tezro audio daughtercard's chips are visible to the
PROM, and whether a later PROM revision beeps. A user's report that a unit is
silent at power-on, or a recording of one that is not, would settle it.

## NVRAM and the first boot (Indy and Indigo2)

The Indy and Indigo2 PROMs play the tune before they check the settings block,
and the player returns before touching HAL2 when the volume string at offset
232 parses as zero (checked in the IP22 and IP24 PROMs: IP24 0x9fc030f4 to
0x9fc03124, IP22 0xbfc03380 to 0xbfc033a0; the IP28 gate is the NVRAM test
above, not disassembled further). The PROM writes its defaults (volume "80",
console "g", baud 9600, time zone PST8PDT, a few flags) only when the checksum
is bad, and only after the tune. A zeroed `nvram.bin` (Indy) or erased
`nveeprom.bin` (Indigo2) therefore read as volume 0 at tune time: the first
boot was silent and later boots chimed. A real machine is programmed at the
factory, so a blank block is an artificial starting state.

iris now seeds the factory state into a block that is blank apart from the
Ethernet address (`src/dev/sgi_nvram.rs`): layout revision 8 for the Indy and
IP22, 9 for the IP28, the defaults above and a checksum. The IP28 block also
gets `boottune` `1`. Existing files and anything a guest has set are never
touched. The checksum is the XOR of every byte but the first, seeded with 0xA5,
rotated left once per 16-bit word; it reproduces the 0xBA the Indy PROM wrote.
The text the PROM leaves in unused slots is not copied.

Checked with blank files on first boot: the Indy gives 92150 frames with the
same PCM sha256 (4891e240...) as a second boot; the IP22 gives 85000 frames
(59e2a08c...); the IP28 is unchanged (b434bab4...). Before the change the first
boots of the Indy and IP22 played nothing. A blank IP22 NVRAM was silent on first
boot.

## Older PROM revisions

A public mirror of an SGI PROM collection holds more revisions
than iris embeds. They are third-party firmware with no redistribution grant:
point `prom` at the file and never commit it.

| Part | Version | Chime |
|---|---|---|
| 070-8127-002 | 5.1 Rev B IP22, Sep 16, 1993 | stored ADPCM, 87492 frames at 44.1 kHz (1.98 s), same decoder tables as the Indy |
| 070-9101-007 | 5.0 Rev B6 IP24, Sep 28, 1994 | has the ADPCM step table; not run |
| 070-1361-003, -004 | 6.0.1 Rev A IP26, Dec 1994 | synthesized strings with a different table (about 234, 295, 330, 443, 467, 592, 663, 706 Hz); not run |
| 070-1477-001 | 6.2 Rev A IP28, Jun 10, 1996 | same first twelve voice rows as 070-1477-002 |

The older IP22 PROM was run with `prom` set to it: codec A played 87492 frames
at 44100 Hz, identical sample for sample to an IMA-style decode of the PROM's
own ADPCM data. A reporter's recording of a real Indigo2 (8-bit mono, 11025 Hz,
1.46 s) matches it in a spectrogram comparison at the same pitch (0.91; every
other candidate scored 0.25 or less) and not the 5.3 Rev E tune iris embeds, so
that machine ran an older PROM. With a blank NVRAM this PROM stayed silent for
280 s in an unseeded run, consistent with the volume gate above (two runs; the
gate not isolated). A 1995 newsgroup post by an SGI engineer says the Indy tune
is ADPCM about two seconds long and that the R8000 Power Indigo2 had dropped its
sampled tune for a synthesized one when the PROM ran out of space, which fits
what these PROMs hold.

## Recording what HAL2 plays

`IRIS_HAL2_AUDIO_DUMP=<file>` writes every frame codec A plays, at the codec's
own rate and before resampling, with or without a host sound device. Records
are the rate (u32) and the frame count (u32), then that many left, right pairs
of little-endian i16. The chimes are mono, so left equals right.

```sh
IRIS_HAL2_AUDIO_DUMP=chime.dump iris --headless --config indigo2.toml
```

Expect the IP22 tune about 60 to 100 s after start on the interpreter.

Each embedded PROM was run headless to its chime and the dump compared with an
independent model written from the PROM's table and arithmetic (float32 for the
synthesized tunes, a plain IMA-style decoder for the Indy), sample for sample:
0 of 85000 differ on the IP22, 0 of 85000 on the IP28, 0 of 92150 on the Indy
(2026-11-05). The comparison covers the CPU and FPU through HPC3 PBUS DMA and
HAL2's rate and DMA handling; it does not cover the host audio device or
resampler.
