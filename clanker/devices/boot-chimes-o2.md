# Boot chime of the O2 (IP32)

**Keywords:** boot chime,startup tune,ip32,o2,mace,ad1843,karplus-strong,ip32prom.image

iris carries no chime samples. The O2's PROM builds its tune in RAM with its
own code and starts the audio hardware playing it, so the sound is right
exactly when the CPU, FPU, DMA and audio models are. The tune is 62144 frames
at 34 kHz, 1.83 s.

PROM: the installer's `ip32prom.image` from IRIX 6.5.30 (431,872 bytes, sha256
`11230c35...fb23c`), firmware revision 4.18. The O2 has no embedded PROM; point
`prom` at the file.

The file is a 256-byte header ("PROM", the payload length at +12) in front of a
431,616-byte payload. Loaded as is, the CPU spins in the PROM's fatal-error
loop at 0xbfc004a0 (observed; the section walk reading a wrong address is
inferred) before touching any audio register. iris now strips the header when
the magic and the length agree with the file size and pads the flash to 512 KiB
with erased bytes. A raw 512 KiB image (`ip32prom.rev4.18.bin`) is used
unchanged, and a file that is neither form is refused with the reason. The
6.5.30 image is the one inside the IRIX 6.5.30 installer. Its payload equals
the 4.18 flash except for the environment section (blank here, a stock
environment in 4.18); run headless from empty flash files, both images played
a chime whose PCM is byte-identical (62,144 frames at 34 kHz, sha256
`4415360a...3833f`, 2026-11-06). The PROM filled the blank environment with the
same defaults itself.

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

`IRIS_O2_AUDIO_DUMP=<file>` records, for the O2, output number (u8), rate (u32),
pair count (u32), then left, right i16 pairs, little-endian. It is paced by wall clock,
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

