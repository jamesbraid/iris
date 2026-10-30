# VPro: the GL test suites (pixtest, glstate, blendtest, vissweep, glprim)

First run of the board-neutral suites in `test/gltest` on the emulated
Octane2 V12. Build: `octane-vpro` at 77bbe1b1 (`[graphics] board = "v12"`),
release `--features jitv2`, thin-LTO "fast" profile. Guest: IRIX 6.5.30m,
MIPSpro 7.4.4m, `glxinfo`: renderer `VPRO/B/128`, OpenGL 1.2 Irix 6.5, GLX
1.3. Results as of 2026-10-30. The suites were not written for VPro: their
XFAIL lists are IMPACT's (VPro is not GR2, so `gr2_only` entries are off),
and they have no VPro list yet. Read every FAIL below as "differs from the
suite's expectation", not as "the board is wrong": libGLcore for VPro is a
different library, and some of these may be its own behaviour.

| Suite | Result |
|---|---|
| pixtest (visual 0x43, RGBA8 double, z24 s8) | 179 pass (172 + 7 XPASS), 197 fail, 5 XFAIL, 108 skip |
| pixtest --ci 8 (visual 0x20) | 6 pass (1 + 5 XPASS), 35 fail, 8 XFAIL |
| pixtest --ci 12 (visual 0x2f) | 6 pass, 73 fail, 4 XFAIL |
| glstate (visual 0x43) | 408 pass (399 + 9 XPASS), 7 fail, 4 XFAIL, 14 skip |
| blendtest, 8,8,8,8 and 12,12,12,12 visuals (0x40-0x43, 0x49-0x4b) | 508 cells, 0 fail |
| blendtest, 4,4,4,4 / 5,5,5,1 / 10,10,10,2 visuals (0x38-0x3d, 0x45-0x48) | 5 of 5 groups fail on every visual |
| seltest | 0 failed |
| vissweep, default lists | 2 pass, 24 fail, 198 no such visual |
| vissweep, VPro lists | 2 pass, 28 fail, 50 no such visual |
| cicopy --bits 8 / 12 | 6 bad each (every check fails) |
| gltest --frames 30 | runs, exits 0 |
| texsweep.sh | prints only; 8 of 32 internal formats read back exactly |
| glprim scenes (quadrants, depth, stencil, blend, blendsmooth, alphatest, lit, fog, tex, texmod, twoside, clipplane, dlist) | all draw; checked by screenshot |

Runs are deterministic: pixtest twice and vissweep three times gave
identical counts. The whole set takes about a minute of wall time
(glstate 1 s, pixtest 7.5 s) because the board model does the work on the
host. `gepeek` and `toktest` are IMPACT command-processor tools and were
not run.

## Visuals

`glprim --listvisuals`: 44 visuals. The GL ones:

- Colour index: 8-bit (0x20 sb, 0x21 db, plus stereo 0x28/0x29) and 12-bit
  (0x2c-0x31), all z24, s0 or s8.
- RGBA 4,4,4,4 and 5,5,5,1: **double-buffered only**, z24 s0/s8, with
  16-bit accum (0x38-0x3f, stereo variants included).
- RGBA 8,8,8,8 and 10,10,10,2: single and double, z24 s0/s8, accum 16
  (0x40-0x43, 0x45-0x48).
- RGBA 12,12,12,12: single z0 (0x49), single z16 (0x4a), double z16 (0x4b).
  No stencil, no z24.
- Pbuffers exist (`vissweep` finds fbconfigs with max 1280x1024), although
  the `--listvisuals` header says "0 pbuffer fbconfigs".
- No 3,3,2, no 4,4,4 without alpha, no 8,8,8 without alpha.
- Several overlay-ish visuals (0x22, 0x25-0x27, 0x2a-0x2b, 0x32-0x37, 0x44)
  show "no GL".

Fallbacks the suites made: pixtest and blendtest pick 0x43 / 0x42 (8888
double). glstate also lands on 0x43. glprim with no `--visual` picks the
deepest RGB visual, so it ran on 0x45/0x46 (10,10,10,2) or 0x49
(12,12,12,12) in the scene runs, and those were the visuals the
screenshots show. vissweep needs VPro's lists:
`COLORS="4,4,4,4 5,5,5,1 8,8,8,8 10,10,10,2 12,12,12,12" ZS="none z24 s8z24 z16" sh vissweep.sh`.
`blendtest --visual 0x38` (and the others) runs the low-depth visuals.

## Passing

- **blendtest on 8,8,8,8 and 12,12,12,12**: all 508 cells, all four
  blend equations, blend colour, min/max and logic ops, single and double
  buffered. This is the one suite that fully passes.
- **seltest**: three hit records, names and z ranges right.
- **glstate**: 408 of 415 non-skipped cases. Capabilities look like a real
  VPro: MAX_TEXTURE_SIZE 2048, MAX_3D_TEXTURE_SIZE_EXT 16384, MAX_VIEWPORT
  3840x3840, point and line width 0.5..10, 8 lights, 6 clip planes, accum
  16 bit.
- **vissweep 12,12,12,12 sb, no depth**, window and pbuffer: PASS.
- **Scenes** (screenshots of glprim `--smooth`): quadrants, depth,
  stencil, blend, blendsmooth, alphatest, lit (smooth-shaded octagon),
  fog (ramp), tex and texmod (tiled quad and triangle, perspective
  checker), twoside, clipplane, dlist all draw something plausible.
  Stencil's blue triangle is clipped to the stencilled shape.
- pixtest passes: RGBA read at UNSIGNED_BYTE, FLOAT draw of every format,
  RGBA/UNSIGNED_SHORT, RGB and LUMINANCE UNSIGNED_BYTE draw, glBitmap (4 of
  6, the other 2 XPASS), glTexSubImage2D (7 of 8, the narrow case XPASS),
  glGetTexImage of RGBA, RED, GREEN and BLUE at UNSIGNED_BYTE.

## Failures, grouped by likely cause

None of these were root-caused; the "likely cause" is from the pattern.

### 1. Depth and stencil: zero in vissweep, depth test not effective (vissweep, pixtest, depth scene)

Every vissweep run with a depth buffer fails the same checks, on every
colour format and for window and pbuffer alike:

    glprim: check depth bl FAIL 00000000 (0.0000), want 1.0000
    glprim: check depth q.bl FAIL 00000000 (0.0000), want 0.2500
    ... (q.br 0.5, q.tl 0.75, q.tr 0.512)
    glprim: check stencil q.bl FAIL 0, want 1  (q.br 2, q.tl 3, q.tr 4)

Geometry is drawn and its colour is read correctly (8888 sb: front checks
pass), but Z and stencil read back as zero.
pixtest agrees from two directions: `depth/colour-draw/less`,
`less-nomask` and `colour-copy/less` fail with "colour f01020, should be
kept (nearer depth)" (the depth test does not reject the farther
fragment), and `stencil/draw-read`, `shift-offset-mask` and `copy` read 205
(0xcd, which is the suite's own prefill of the read buffer, so the read
wrote nothing) where it wants 17, 11, 199. In the `depth` scene the blue bar
paints over the red triangle on the side where the triangle is nearer, as
if no depth test ran. DrawPixels of depth and ReadPixels of it round-trip
up to 1 LSB (`depth/draw-read/FLOAT` got 0.6431361, want 0.6431373; also
#304-306), so a depth buffer exists. Likely cause: geometry does not write Z or
stencil (or stores it where the readback does not look), and the stencil
ReadPixels returns without writing the buffer. The depth-scene reading
(blue over red on the far side) is by eye, not measured.

### 2. Colour readback of 4,4,4,4 / 5,5,5,1 / 10,10,10,2 returns zero

vissweep: every `front` and `back` check on those formats reads
`00000000`, even the clear colour. 12-bit and 8-bit visuals read fine
single-buffered. blendtest on 0x38-0x3d and 0x45-0x48 fails `add` (99 of
100 cells), `subtract` (69-73 of 100), `reverse-subtract` (70-73),
`min` and `max` (4 of 4 cells); 8,8,8,8 and 12,12,12,12 pass all. Likely
cause: pixel readback or storage for the packed and 10-bit layouts is
missing; whatever draws, the read does not see it. Double-buffered 8888 and
12-bit window readbacks pass, so buffer swap itself is not the issue.

### 3. Pbuffers with double buffering lose the colour readback too

8,8,8,8 and 12,12,12,12 db pbuffer fail `front` and `back` (8888 db
window passes them). Single-buffered pbuffers pass colour. Likely cause: the
double-buffer pbuffer's buffers are not wired for glReadBuffer.

### 4. DrawPixels drops or mangles most type/format pairs (pixtest draw: 85 fail, tex: 49 fail, others)

The sentinel background in pixtest is 1a334d66. Pairs that pass: every
FLOAT format, RGBA/UNSIGNED_SHORT, RGB/UNSIGNED_BYTE and
LUMINANCE/UNSIGNED_BYTE, ABGR/UNSIGNED_SHORT and two packed ABGR types.
Failures sort into:

- **Nothing drawn** (read back is the sentinel, `got 1a334d66`): every
  BYTE, SHORT, INT and UNSIGNED_INT pair, plus RED/GREEN/BLUE/ALPHA/
  LUMINANCE(_ALPHA) at UNSIGNED_SHORT. 62 of the 85.
- **Alpha forced to 255**: RGBA and ABGR_EXT at UNSIGNED_BYTE and
  UNSIGNED_INT_8_8_8_8: `got 116cc7ff want 116cc722`. Colour is right.
- **Same wrong value everywhere**: RED/GREEN/BLUE/ALPHA at UNSIGNED_BYTE all
  read 467bb0ff. That is not the data (inferred, not checked: a
  leftover from an earlier case).
- **Wrong channels**: 4_4_4_4 (`c249f5ff` for 1166cc22), 5_5_5_1 (`704d3eff`
  for 106bc500) and 3_3_2 (`5379a1ff`).
- **Rows skewed**: RGB/UNSIGNED_SHORT at any size (`5a2e89ff` at (0,1)
  where `2e89e4ff` is wanted), also 93x7.
- **Pixel transfer ignored**: scale-bias, map16/map256 and every colour-index
  draw return the unmodified input (see 8).

The tex/ cases (49 fail) upload the same pairs as textures and read them
back; the failing pairs are the same ones, with the same wrong values
(`467bb0ff`, `c249f5ff`, alpha 255). They mostly mirror this group.

### 5. Pixel zoom and CopyPixels

All zoom cases fail (10), zoombig (12), and `copy/color` (9, every
offset, zoom and scale-bias): `pixel (100,85) got 1a334d66 want 116cc722`.
CopyPixels reads back the sentinel everywhere, so the copy source was
either not drawn (it is DrawPixels RGBA/UNSIGNED_BYTE, group 4) or the copy
did not run. These cases depend on group 4, so this group may fold into it.

### 6. Buffer selection (pixtest buffers: 3)

`geometry-front-back: blue missing in back`, `drawpixels-front-back:
front lost its green (+2 more)`, `copy-front-to-back: copy missing in
back`. Writing to GL_BACK or GL_FRONT_AND_BACK does not reach the back
buffer in these cases. Fits group 3.

### 7. Texture readback

glGetTexImage of RGB/LUMINANCE/LUMINANCE_ALPHA/ABGR at UNSIGNED_BYTE fails
(`gettex` 4 cases, `texel 0 comp 0 got 205 want 17`, 0xcd again);
copytex and copytexsub get 10 where 17 is wanted (`0ac85aff` for
`116cc722`, a different value than the source: the source was rendered by the
failing DrawPixels).

`texsweep.sh` (glprim `--texread`, which prefills its buffer with 0x11):

- exact (0 bytes differ): rgb8, rgb12, rgb16, rgba8, rgba12, rgba16.
- **buffer untouched** (`11111111`, 256 of 256 differ, no GL error):
  r3_g3_b2, rgb4, rgb5, rgb10, rgba2, rgba4, rgb5_a1, rgb10_a2. The read
  returns without writing, silently.
- alpha*, luminance*, intensity*: 176 of 256 differ, first texel `000000ff`.
  luminance_alpha*: 240 of 256, first texel `00000000`. Whether the suite's
  expectation matches libGLcore here is not checked; compare with the GR2
  XFAIL "glGetTexImage returns LUMINANCE as R+G+B".
- Wrap and border cases, sub-image (`texsub: (32,8) 32x8 glGetError 0x0`),
  texcopy full and sub, copycolor and copydepth: no GL errors; the images
  need a visual check, which was not done.

### 8. Colour index is dead

Both CI depths fail even `baseline/rects` and `baseline/clear`: a rectangle
drawn with glIndex reads back index 0 where 17 is wanted, glClearIndex 5
reads 0. cicopy: all six checks fail, "2048 bad" on each, at 8 and at 12
bits. So CI rendering, clearing and copy produce nothing. (The `XFAIL` and
`XPASS` entries in the CI runs are not informative while the baseline fails.)

### 9. glstate (7 fail, 4 XFAIL, 9 XPASS)

- `current/RASTER_COLOR`: got 0,0,0,0, want 0.5,0.25,1,0.75.
- `current/RASTER_DISTANCE`: got -0.5, want 0.5 (sign flipped; GR2 returns
  eye z here, so this is probably another libGLcore choice).
- `fog/INDEX`: got 0, want 7.
- `fb/INDEX_WRITEMASK`: got 0, want 28. `fb/INDEX_CLEAR_VALUE`: got 1, want 11.
  (Index state not retained; fits group 8.)
- `enable/PIXEL_TEXTURE_SGIS` on and off: `glGetBooleanv got 90 (0x5a)`
  with GL error 0x500 (GL_INVALID_ENUM), though SGIS_pixel_texture is in
  the extensions string. Same symptom as the POLYGON_OFFSET_LINE/POINT
  XFAILs (the query is refused); a candidate VPro XFAIL.
- XFAIL as expected on IMPACT: POLYGON_OFFSET_LINE and _POINT queries
  (INVALID_ENUM) on and off.
- XPASS (IMPACT's quirk absent here): `lightmodel/AMBIENT`,
  `texparam/WRAP_S/CLAMP_TO_EDGE`, `texparam/RESIDENT`,
  `texlevel/LUMINANCE_SIZE`, `texlevel/INTENSITY_SIZE`, `texenv/MODE/REPLACE`
  (IMPACT sends REPLACE as MODULATE; VPro does not), `texenv/COLOR`,
  `frag/BLEND_COLOR_EXT`, `eval/MAP1_VERTEX_3`.
- 14 skips: MULTISAMPLE_SGIS, SAMPLE_*_SGIS, TEXTURE_4D_SGIS,
  REFERENCE_PLANE_SGIX, SPRITE_SGIX are not accepted by VPro's libGLcore.

### 10. pixtest read cases (13 fail, 4 XFAIL, 4 XPASS)

RGB reads at UNSIGNED_BYTE (and /align1,2,8, 3x120) return 0xcd, the
suite's prefill of the read buffer (`got 205 want 17`): the read wrote
nothing. Same for ABGR_EXT/UNSIGNED_BYTE and RGBA/4_4_4_4 (0xcdcd).
RGBA/UNSIGNED_SHORT and RGB/SHORT, UNSIGNED_SHORT come back as an 8-bit
value repeated in both bytes (`raw 34181` = 0x8585, want 4369 = 0x1111;
whose pixel that byte is, not checked). The IMPACT XFAILs for packed 5551
and 1010102 (reversed layout) still apply. The 0xcd fill also shows in
groups 1 and 7: several ReadPixels and GetTexImage type/format pairs return
without storing anything.

## Gotchas

- **The csh and `iris-ci run` traps cost most of the time.** Root's shell is
  csh. A `2>&1 |` is "Ambiguous output redirect", use `|&` or `>&`. A
  multi-line `foreach` typed into `ci run` leaves the shell at `?` forever
  and every following command times out waiting for `IRIS-CI-RC=`: put
  a script and `sh` it. Writing `ci run` output to a file in the guest and
  fetching it with `iris-ci get` is faster than reading the console.
- **The screen blanks after 600 s idle** (`xset q`: timeout 600). A screenshot
  taken then is solid black with only the cursor, and every GL window
  comes out black. `xset s off; xset s noblank; xset s reset` first.
  Running two GL suites at once on one display is a bad idea too: overlapping
  windows corrupt each other's readbacks.
- **cc prints a licence nag** ("requires a license password") on every
  invocation, and still compiles. `sh irix.sh` builds everything in
  about a minute.
- IRIX `grep` has no `\|` and no `-A`. Use `egrep` with a plain pattern.
- The guest's /tmp is cleared on boot: stage the tests again after every
  restart (`iris-ci put` a tar of `test/gltest`).
- **Default visual**: glprim and gltest pick the deepest RGB visual, so
  without `--visual` a scene runs on a 10-bit or 12-bit visual, not 8888.
  Pass `--visual 8,8,8,8` when comparing with another board.
- Most `vissweep.sh` default rows are `none`: VPro has no 3,3,2, no
  single-buffered 4,4,4,4 or 5,5,5,1, no z32, and no stencil with z16.
- Scene windows open at a different origin each run (it moved 32,32 per
  run): read the `window origin` line from glprim's output before
  cropping a screenshot.

## What to do next

1. Z and stencil after geometry (group 1) and the 8888/12-bit double
   pbuffer readbacks (groups 3 and 6) are the cleanest defects. One
   command reproduces group 1: `glprim -p none --scene quadrants --visual
   8,8,8,8 --zs s8z24 --read front,depth,stencil --check --hold 0`.
2. Then DrawPixels by type (group 4): the failing list is mostly the
   non-FLOAT, non-unsigned-short types, and zoom and CopyPixels follow.
3. CI (group 8) and the packed visuals (group 2) next.
4. After a fix, add VPro's real XFAIL list to the suites (a `vpro_only`
   flag beside `gr2_only`) once libGLcore behaviour is checked, as was
   done for GR2.
