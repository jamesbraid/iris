# Boot chimes: what the Indy and Indigo2 PROMs play

**Keywords:** boot chime,startup tune,power-on tune,hal2,hpc3,pbus,adpcm,karplus-strong,boottune,nvram,ip22,ip24,ip26,ip28

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

All figures below are for the PROM named. PROM code addresses are in the PROM's
own address window, for reading with iris's monitor.

## Indy IP24

Stored tune: 4-bit IMA-style ADPCM, high nibble first, 89-entry step table.
The PROM's decode step is `((mag * step) >> 2) + (step >> 3)`, not the
textbook bitwise IMA form; the two differ on most samples, by at most 52
counts.
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

## Indigo2 Impact IP28

The IP28 PROM has the same voice table as the IP22 (at 0xbfc5b200) with one
difference that makes it a different recording: it starts every string's noise
seed at 0x41c60000 instead of reading an unset stack word. Same tune, different
noise. Its player is at 0xbfc03404. It checks, in order, the processor
revision (0xbfc03418; the same test sits at 0xbfc00d1c), the HAL2 revision
register (0xbfc03438), the volume setting (0xbfc03448) and then `boottune`, and
returns early if any of the first three fails or the volume is zero.

The IP28 PROM holds four start tables and four stop tables, chosen by the
one-character `boottune` NVRAM setting (offset 242): `1` to `4` pick that tune,
`0` uses the low two bits of a free-running counter, and anything else picks
tune 1. iris honours the setting because it runs the PROM; it picks nothing
itself. It writes `boottune` only when it seeds a blank IP28 EEPROM, to `1`
(below); an existing setting is left alone. Measured with the setting at 1 to 4:
85000, 85000, 65000 and 71000 frames, with 0 samples different from an
independent model of each tune.

The IP28 needs `banks = [512, 512, 0, 0]` to chime: with 128 MB banks it was
still in a delay loop after three minutes (cause not investigated).

## NVRAM and the first boot (Indy and Indigo2)

The Indy and Indigo2 PROMs play the tune before they check the settings block,
and the player returns before touching HAL2 when the volume string at offset
232 parses as zero (checked in the IP22 and IP24 PROMs: IP24 0x9fc030f4 to
0x9fc03124, IP22 0xbfc03380 to 0xbfc033a0; the IP28 test is described above).
The string is decimal by default; `0x` and `0b` prefixes and a leading `0`
(octal) are accepted. The IP22 5.3 Rev E PROM writes `(v >> 1) + (v >> 2)` to
both HAL2 volume registers; the IP24, the IP22 5.1 Rev B PROM and the IP28 write
the value unchanged. The PROM writes its defaults (volume "80",
console "g", baud 9600, time zone PST8PDT, a few flags) only when the checksum
is bad, and only after the tune. A zeroed `nvram.bin` (Indy) or erased
`nveeprom.bin` (Indigo2) therefore read as volume 0 at tune time: the first
boot was silent and later boots chimed. A real machine is programmed at the
factory, so a blank block is an artificial starting state.

iris now seeds the factory state into a block that is blank apart from the
Ethernet address (`src/dev/sgi_nvram.rs`): layout revision 8 for the Indy and
IP22, 9 for the IP28, the defaults above and a checksum. The IP28 block also
gets `boottune` `1` (a blank IP28 EEPROM only). Existing files and anything a guest has set are never
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
at 44100 Hz, identical sample for sample to a decode of the PROM's own ADPCM
data with the PROM's decode step. It holds three sampled tunes (87492, 54700 and
32638 frames); the synthesizer arrived in a revision between 5.1 Rev B and 5.3
Rev E, which one is unknown.

A reporter's recording of a real Indigo2 (8-bit mono, 11025 Hz, 1.46 s)
resembles that sampled start tune: the two share spectral peaks near 328, 392,
524, 660 and 786 Hz. The match is not proven: the recording also has peaks at
294 and 589 Hz and none at 261 Hz. The best candidate PROM is 5.1 Rev B; a
reporter's banner string would settle it. With a blank NVRAM this PROM stayed silent for
280 s in an unseeded run, consistent with the volume gate above (two runs; the
gate not isolated). A 1995 newsgroup post by an SGI engineer says the Indy tune
is ADPCM about two seconds long and that the R8000 Power Indigo2 had dropped its
sampled tune for a synthesized one when the PROM ran out of space, which fits
what these PROMs hold.

