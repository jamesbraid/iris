# KONA token census of the two recorded streams

Status: measurement (2026-11-07). What the front end saw when QEMU's recordings
of an Onyx2 deskside's InfiniteReality2 (two raster managers) were replayed
into it with `NullBackend` behind a counting tap
(`src/dev/kona/back/census.rs`, `replay.rs`). The recorder is another model,
not a pipe, so the streams are what that model's guest sent.

Reproduce: `KONA_RECORDINGS=<dir with the .rec files> cargo test --lib
kona::back::replay -- --nocapture --test-threads=1`. Without the variable the
tests skip.

## How far the counts can be trusted

The front end frames only the commands in its selector tables, and answers
`Reply::Data(n)` only when a back end asks. Data a command carries that the
tables cannot size reaches the back end as `Job::Dropped` words. The record
where the 64th dropped word in a row came marks where a replay into
`NullBackend` stops being a count of the stream (the data run itself began a
little before): a data word that happens to equal a selector can open a
command that was never sent, and the register answers drift (a bogus
`context_deactivate` posts a completion reason the recording lacks).

- PROM recording: no such run. The whole stream is exact.
- IRIX recording: the graphics command stream starts at record 1,394,816 of
  1,727,527 (the earlier records are boot) and the 64th dropped word in a row
  came at record 1,415,538, about 20,700 records in. The data run it belongs to
  began a little before; the pixel write that carries the misframed word
  below is at 1,417,227, after it. Deterministic register answers and every
  byte the front end wrote to guest memory match the recording up to there.
  The first load that differs is at 1,419,153: the geometry-engine cause bit
  is set where the recording's is not, from a misframed word (record 1,417,231,
  the word 0x1fc inside the data of the pixel write that began at 1,417,227).
  Counts for the whole run are therefore upper-level estimates; the counts
  "up to the long run" are exact but cover little (1,642 jobs of 75,126).

## PROM recording (63,750 records)

Stores 52,362, loads 11,381, resets 6. Textport grammar only, no kernel or GL
command, no DMA, no interrupt, no dropped word, 9,499 jobs:

| Textport command | Count |
|---|---|
| 0x2 | 3,232 |
| 0x5 | 4,766 |
| 0x4 | 534 |
| 0x3 | 201 |
| 0x1 | 2 |
| 0x7, 0x8 | 1 each |
| bitmap blits | 762 (2,732 payload words) |

The replay is exact: no load differs, nothing dropped, the board does not fail.

## IRIX recording to the 4Dwm desktop (1,727,528 records)

Stores 1,103,572, loads 591,438, resets 2, two geometry-engine interrupts
(vector 12 both times, matched), 31,860 general interrupts (not reproduced:
the recorded model raises them, and no retrace request arrives in this stream). 75,126 jobs over the whole run.

Kernel and X commands, 44 selectors, 9,206 commands (whole run; an estimate
past the long run). The X path dominates: x_position 1,485, x_gc 1,195, x_clip
1,126, x_box 730, x_pixel_write 692, x_bdata4 514, x_pixel_setup 448,
x_point 411, x_rect 319, x_pixel_finish 267, x_spans6 248, x_load_stipple 230,
x_point6 176, x_data1 115, x_pixel_copy 113, x_store_stipple 113, x_data2 96,
x_spans2 84, x_rotate_stipple 84, x_data3 82, x_pixel_read 63, x_data4 58,
x_data6 50, x_spans 40, x_point24 23, x_box6 7, x_rect6 4. Also video 66,
display_mode 96, did_map 96, color_map 73, gamma 3, cursor 6, x_scissor 74,
six DMA packets (454,800 bytes of image data, all addressed through the map
RAM), select_feed start and finish 2 each, and one each of x_init, get_config,
reason_page, context_map and context_records, return_page 2 and
context_deactivate 2 (one real; the other is a misframe inside pixel data).
One context activation.

GL commands, 10 selectors, 210 commands (whole run): light parameter (0x7) 3,
light model (0xa) 36, texture env (0x12) 18, clear colour (0x1d) 36, clear
depth (0x1f) 3, pixel store (0x33) 2, unsigned pixel map header (0x35) 54,
colour4s (0x6d) 36, end (0x71) 21, texture coordinate1f (0x7c) 1. The tokens are lighting, texture
environment, pixel maps and clears, and 21 `end` with no `begin` (the front end
has no table entry for the begin forms).

Up to the long run (exact): 24 kernel selectors, 822 commands, one GL command
(light parameter), video 66, display_mode 96, did_map 96, color_map 34, gamma 3,
cursor 3, pixel setup 23, pixel write 23, pixel copy 17.

Data runs the front end sized itself: color map 73 runs (331 words), gamma 3
(12,288), cursor 6 (1,560).

## Words the front end could not size

65,085 dropped words in the whole run, by the command before them:

| After | Words |
|---|---|
| x_pixel_write (0x139) | 58,442 |
| gl_pixel_store (0x33) | 5,355 |
| gl_pixel_map_ui (0x35) | 486 |
| x_store_stipple (0x13d) | 454 |
| gl_tex_envi (0x12) | 256 |
| context_deactivate (0x1fc) | 68 (a misframe inside pixel data) |
| gl_clear_depth (0x1f) | 13 |
| gl_lightf (0x7) | 8 |

These are the pixel, stipple, pixel-map and texture-table data a back end must
size by answering `Reply::Data(n)`, from the token layouts of the setup and
header commands before them. The rule that sizes each is the command-grammar
dependency for the X pixel path (pixel setup and write), stipples, GL pixel
store and pixel maps, and the texture environment and table.

## Against the layouts QEMU's decoder marks unresolved

The decoder's table (33 selectors) marks five entries unresolved:

| Selector | Call | In the IRIX stream |
|---|---|---|
| 0x1f | clear depth | 3 commands framed (the front end frames two words); the argument layout is open |
| 0x0 | begin | not framed: the front end has no entry, the zero is a dropped word (6,735 dropped zeros, which also cover data and arguments) |
| 0x500 | begin | not framed; 0x500 is also the managed width in the position and clip commands, so the 4 dropped 0x500 words say nothing |
| 0x119 | context resolve | absent |
| 0x1d6 | context resolve | absent |

So the intersection is two layouts: clear depth (0x1f) and begin (0x0 or
0x500, 21 `end` tokens imply about 21 begins). The two context-resolve layouts
are not used by either stream.

Six of the ten GL selectors seen have no entry in QEMU's decoder table: light
parameter 0x7, light model 0xa, texture env 0x12, pixel store 0x33, colour4s
0x6d, texture coordinate1f 0x7c. Their meaning is not in the sources the
design names; the front end's table names are labels only.

Not in either stream: textport commands 0x6 and 0x9..0xc, GL vertex, array,
matrix, texture bind and query tokens, retrace requests, swap markers,
overlay buffer configuration, window loss. A GL demo recording is needed to
census those.
