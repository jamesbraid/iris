/*
 * C interface of the MGRAS (IMPACT) graphics engine: its constructor and the
 * entries that name its own types. Everything else (write, read, pump,
 * events, state) is iris_engine.h, whose call contract applies.
 *
 * Host DMA addresses (the host's ram_* and bus_* callbacks) are fabric addresses: the XIO widget of the
 * transfer's page table, bits 19:16 of its control word, in bits 51:48 above the node's 40-bit physical address.
 * Inline (mode 0) the transfers go through bus_*, with a worker thread through ram_* alone.
 *
 * Kept in step with src/ffi.rs by tools/check-header.
 */

#ifndef SGI_MGRAS_H
#define SGI_MGRAS_H

#include <stddef.h>
#include <stdint.h>

#include "iris_engine.h"

#ifdef __cplusplus
extern "C" {
#endif

/* The MGRAS configuration: the shared part, then the board's. */
typedef struct IrisMgrasConfig {
    IrisEngineConfig common;
    uint32_t board;         /* 0 Solid IMPACT, 1 High, 2 Maximum */
    uint32_t flags;         /* bit 0: the Octane's HQ4 front; bit 1: 8-byte DMA page-table entries;
                               bit 2: bdvers, cmap_rev and ge_revision are given (else the
                               board's own bus bytes and no GE revision); bit 3: pp1_revision is given */
    int32_t jit;            /* -1 default, 0 off, 1 on first use, 2 background */
    uint32_t bdvers[2];     /* the display bus's two board-version bytes */
    uint32_t cmap_rev[2];   /* the two colour maps' revision registers */
    uint32_t ge_revision;   /* read at 0x028004 before the first GE query: 2 GE11 rev B, 3 GE12 rev A */
    uint32_t pp1_revision;  /* the XMAP's pixel processor revision, bits 2:0, read when flags bit 3 is set;
                               else the board's own: A on the GE11 boards, E on a one-GE GE12 board,
                               H on a two-GE one */
} IrisMgrasConfig;

/*
 * The store of an IRIS_ENGINE_EVENT_DAMAGE mark the display bus posts when a store changes the cursor alone: its box
 * is in display pixels from the top left, y0 to y1 the rows the glyph covered or covers. A mark for anything else the
 * bus changes has the store UINT64_MAX (everything). A drawing mark's store is the buffer's page pointer, plus
 * 1 << 16 for the overlay.
 */
#define IRIS_MGRAS_DAMAGE_DISPLAY 131072       /* 1 << 17 */

/* A box of pixels in one buffer. */
typedef struct IrisMgrasRegion {
    uint32_t page;          /* the buffer's page pointer */
    uint32_t overlay;       /* non-zero: the 9-bit overlay buffer */
    uint32_t drbsize;       /* the raster DRBSIZE the buffer was laid out with */
    int32_t x0;
    int32_t y0;
    int32_t x1;
    int32_t y1;
} IrisMgrasRegion;

/*
 * Start an engine. Free it with iris_engine_free. NULL on bad arguments or
 * failure (the reason goes to the log). MGRAS owns its pixel memory: lent_len
 * must be 0.
 */
IrisEngine *sgi_mgras_new(const IrisMgrasConfig *cfg,
                           const IrisEngineHost *host);

/* Bring a box of pixels current for a CPU reader. Waits for queued CPU work,
 * then only for GPU work that touches the box. */
int sgi_mgras_observe(IrisEngine *h, const IrisMgrasRegion *r);

/* The pixel at (x, y) of the buffer r names, as stored. */
int sgi_mgras_pixel(IrisEngine *h, const IrisMgrasRegion *r, uint32_t x,
                     uint32_t y, uint64_t *val);

/* Pixels x0 up to x1 (exclusive) of row y of the buffer r names, current after one observe: the
   low 32 bits of each wide pixel, 9 bits of an overlay one. out holds x1 - x0 words. A range
   outside the planes answers IRIS_ENGINE_ERROR. */
int sgi_mgras_read_row(IrisEngine *h, const IrisMgrasRegion *r, uint32_t y, uint32_t x0,
                       uint32_t x1, uint32_t *out);

/*
 * The shell's device on the display control bus, for an option board such as the Digital
 * Video board: the engine calls read for a load and write for a store on a slot it has no
 * device for (not the colour maps, DAC, XMAP, VC3 or the board-version byte), register block
 * crs of slot dev, width bytes. read returns 1 with *data set when it answers, 0 to leave the
 * access to the engine's scratch register; write returns 1 when it took the store. Both run
 * with the engine's display lock held and must not call the engine. NULL for both detaches.
 * It survives a reset and a load.
 */
int sgi_mgras_set_display_bus(IrisEngine *h, void *user,
                              int (*read)(void *user, uint32_t dev, uint32_t crs, uint32_t width, uint32_t *data),
                              int (*write)(void *user, uint32_t dev, uint32_t crs, uint32_t width, uint32_t data));

/* The hardware cursor as the VC3's registers hold it: the glyph's top-left pixel in screen pixels from the top
 * left (the position registers less 31, signed) in *x and *y, set whether or not it is shown. Returns 1 when
 * it is shown (cursor control bits 0 and 1 both set), 0 when not or for a NULL output. */
int sgi_mgras_cursor_position(IrisEngine *h, int32_t *x, int32_t *y);

/* The VC3's display control register (0x1e) as the guest last wrote it: bit 0 enables the retrace
 * interrupt, bit 2 the display; 0 after a reset. */
uint32_t sgi_mgras_vc3_control(IrisEngine *h);

/*
 * The CPU picture, for composing off the shell's big lock. sgi_mgras_scan_begin is an ordinary call (one
 * caller at a time) and gives NULL for an engine with a GPU unit, whose picture iris_engine_scanout gives.
 * sgi_mgras_scan_rows may then be called from any thread, also while the handle is in a call: display rows
 * first up to end (clamped to the picture) as 0x00RRGGBB words into buf (cap words) at y * width, the other
 * words untouched; *w and *hgt are the picture's size, and with a cap too small for it the answer is
 * IRIS_ENGINE_ERROR and nothing was composed. It holds the engine's lock for the plane reads and their decode and
 * the display lock for a copy of the display bus; only the colour map, gamma and cursor pass runs with neither.
 * End the source with sgi_mgras_scan_end once no rows call is running.
 */
typedef struct IrisMgrasScan IrisMgrasScan;
IrisMgrasScan *sgi_mgras_scan_begin(IrisEngine *h);
int sgi_mgras_scan_rows(IrisMgrasScan *s, uint32_t first, uint32_t end, uint32_t *buf, size_t cap,
                        uint32_t *w, uint32_t *hgt);
void sgi_mgras_scan_end(IrisMgrasScan *s);

/* The HQ flags as the board holds them (a debug accessor: the interrupt
 * line is iris_engine_irq_levels). */
uint32_t sgi_mgras_flags(IrisEngine *h);

#ifdef __cplusplus
}
#endif

#endif /* SGI_MGRAS_H */
