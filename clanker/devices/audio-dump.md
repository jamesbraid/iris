# Recording what the audio chips play, and the Octane chime

**Keywords:** audio,boot chime,rad1,octane,ip30,hal2,mace,IRIS_AUDIO_DUMP,wav

iris carries no chime samples. Each PROM builds or decodes its tune in RAM with
its own code and starts the audio hardware playing it. The Octane's is stored
4-bit ADPCM at 32 kHz, 72064 frames, 2.25 s.

## Octane IP30

The Octane plays through the RAD1 PCI audio card, not HAL2. The PROM is not
embedded; checked with 6.5.30, sha256
`ee4722ca6f9b821ffadf156346c4d69bf5be71fc4b911cee75e36a1f51a49972`.

The tune is stored: 31941 bytes of 4-bit IMA-style ADPCM (the same decode step
as the Indy), high nibble first,
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

`IRIS_AUDIO_DUMP=<file>` records the card's frames at its own rate, in
the HAL2 dump layout (see "Recording what the audio chips play"), with or
without a host sound device.
`IRIS_AUDIO_DUMP=<file>.wav` writes 44.1 kHz stereo after the shared
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

## Recording what the audio chips play

`IRIS_AUDIO_DUMP=<file>` records what the machine's audio output plays, with
or without a host sound device. One recorder (`src/dev/audio_dump.rs`) serves
HAL2, the Octane's RAD1 card and the O2's MACE audio, and the file name picks
the layout:

- Any name but `*.wav`: raw records of every frame at the chip's own rate,
  before resampling, for comparing a PROM's tune sample for sample. A record is
  the rate (u32) and the frame count (u32), then that many left, right pairs
  of little-endian i16. The O2, which has two output channels, puts the channel
  number (u8) ahead of each record. The Indy and Indigo2 chimes are mono, so
  left equals right.
- `*.wav`: a 44.1 kHz stereo 16-bit WAV of the same frames, to listen to. A
  WAV holds one stream, so of the O2's two outputs only output 0 goes in.

The host-capture variables (`IRIS_O2_AUDIO_CAPTURE`,
`IRIS_OCTANE_AUDIO_CAPTURE`) record what the host callback consumed instead,
need a real host device, and are unchanged.

```sh
IRIS_AUDIO_DUMP=chime.dump iris --headless --config indigo2.toml
```

Expect the IP22 tune about 60 to 100 s after start on the interpreter.

Each embedded PROM was run headless to its chime and the dump compared with an
independent model written from the PROM's table and arithmetic (float32 for the
synthesized tunes, a decoder using the PROM's decode step for the Indy), sample for sample:
0 of 85000 differ on the IP22, 0 of 85000 on the IP28, 0 of 92150 on the Indy
(2026-11-05). The comparison covers the CPU and FPU through HPC3 PBUS DMA and
HAL2's rate and DMA handling; it does not cover the host audio device or
resampler.
