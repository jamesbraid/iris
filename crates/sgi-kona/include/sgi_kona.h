/*
 * C interface of the KONA (InfiniteReality) graphics engine: its constructor and the
 * entries that name its own types. Everything else (write, read, pump, events, state)
 * is iris_engine.h, whose call contract applies.
 *
 * Kept in step with src/ffi.rs by tools/check-header.
 */

#ifndef SGI_KONA_H
#define SGI_KONA_H

#include <stddef.h>
#include <stdint.h>

#include "iris_engine.h"

#ifdef __cplusplus
extern "C" {
#endif

/* The KONA configuration: the shared part, then the pipe's. */
typedef struct IrisKonaConfig {
    IrisEngineConfig common;
    const char *product;        /* "ir2", "ir2e" or "ir3"; NULL is refused */
    uint32_t rm_count;          /* raster managers, 1 or 2; 4 behind a host board */
    uint32_t identity_index;    /* the pipe slot, 0 to 3 */
    uint32_t host_widget;       /* the widget DMA goes to out of reset, 0 to 15 */
    uint32_t widget;            /* the widget the pipe sits at, 0 to 15 */
    uint32_t probe;             /* the value the probe register reads */
    uint32_t managed_width;     /* the managed area in pixels; 0: 1280 by 1024 */
    uint32_t managed_height;
    uint32_t pool_threads;      /* tile workers; 0: draw on the caller's thread */
    uint32_t pool_tile;         /* tile size in pixels */
    /* The boards. NULL or "" takes the product's own; the display generator's own is the
     * DG5-8, or the DG5-2 behind a host board. */
    const char *geometry;       /* "ge14-4", "ge14-2" or "ge16-4", as the product takes them */
    const char *rm_type;        /* "rm7" or "rm9": the product's, or refused */
    const char *display;        /* "dg5-8" or "dg5-2" */
    const char *pixel_depth;    /* "small" or "medium"; NULL is small */
    uint32_t texture_mb;        /* 0 or the product's texture per raster manager, in MB */
    uint32_t channels;          /* 0 or the display generator's channel count */
    uint32_t host_board;        /* 0 none (Onyx2 deskside), 1 Ktown (rack), 2 Ktown2 (Onyx 3400) */
} IrisKonaConfig;

/* A box of pixels in one word position of the store. */
typedef struct IrisKonaRegion {
    uint32_t word;
    int32_t x0;
    int32_t y0;
    int32_t x1;
    int32_t y1;
} IrisKonaRegion;

/*
 * Start an engine. Free it with iris_engine_free. NULL on bad arguments or
 * failure (the reason goes to the log). KONA owns its pixel memory: lent_len
 * must be 0. mode is ignored: the pipe draws on the caller's thread, or on
 * its tile workers when pool_threads is not 0.
 */
IrisEngine *sgi_kona_new(const IrisKonaConfig *cfg,
                          const IrisEngineHost *host);

/* Bring a box of pixels current for a CPU reader. */
int sgi_kona_observe(IrisEngine *h, const IrisKonaRegion *r);

#ifdef __cplusplus
}
#endif

#endif /* SGI_KONA_H */
