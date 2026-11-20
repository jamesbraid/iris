/*
 * C interface of the MGRAS (IMPACT) graphics engine: its constructor and the
 * entries that name its own types. Everything else (write, read, pump,
 * events, state) is iris_engine.h, whose call contract applies.
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
                               bit 2: bdvers and cmap_rev are given (else the board's own) */
    int32_t jit;            /* -1 default, 0 off, 1 on first use, 2 background */
    uint32_t bdvers[2];     /* the display bus's two board-version bytes */
    uint32_t cmap_rev[2];   /* the two colour maps' revision registers */
} IrisMgrasConfig;

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

/* The HQ flags as the board holds them (a debug accessor: the interrupt
 * line is iris_engine_irq_levels). */
uint32_t sgi_mgras_flags(IrisEngine *h);

#ifdef __cplusplus
}
#endif

#endif /* SGI_MGRAS_H */
