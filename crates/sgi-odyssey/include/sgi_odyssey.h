/*
 * C interface of the VPro (Odyssey) graphics engine: its constructor and the
 * entries that name its own types. Everything else (write, read, pump,
 * events, state, lent memory) is iris_engine.h, whose call contract applies.
 *
 * Kept in step with src/ffi.rs by tools/check-header.
 */

#ifndef SGI_ODYSSEY_H
#define SGI_ODYSSEY_H

#include <stddef.h>
#include <stdint.h>

#include "iris_engine.h"

#ifdef __cplusplus
extern "C" {
#endif

/* The VPro configuration: the shared part, then the board's. */
typedef struct IrisVproConfig {
    IrisEngineConfig common;
    /*
     * "v12", "v10", "v8" or "v6". A product overrides revision, memory_size,
     * memory_banks and backend_version. NULL or "": the properties below are
     * used as given and checked.
     */
    const char *product;
    const char *nic_record; /* the NIC's record text; NULL or "": the product's */
    uint64_t nic_serial;    /* the NIC's serial, 48 bits; 0: a synthetic one */
    uint32_t widget;        /* the widget number the board answers as */
    uint32_t revision;      /* 0 to 15 */
    uint32_t memory_size;   /* megabytes: 32, 64 or 128 */
    uint32_t memory_banks;  /* 2 or 4 */
    uint32_t cas_latency;   /* 2 or 3 */
    uint32_t backend_version;
    uint32_t pool_threads;  /* the fragment pool's workers; 0: a default */
    uint32_t pool_tile;     /* its tile size in pixels; 0: a default */
} IrisVproConfig;

/*
 * Bytes of board memory the configuration has: the lent_len to give in the
 * shared configuration. 0 for a configuration the board cannot have.
 */
uint64_t sgi_odyssey_memory_bytes(const IrisVproConfig *cfg);

/*
 * Start an engine. Free it with iris_engine_free. NULL on bad arguments or
 * failure (the reason goes to the log). The board memory may be lent
 * (lent_ptr, lent_len == sgi_odyssey_memory_bytes): the engine then draws in
 * it and reports what it wrote through iris_engine_drain_dirty.
 */
IrisEngine *sgi_odyssey_new(const IrisVproConfig *cfg,
                          const IrisEngineHost *host);

/*
 * Test hooks, in a build with the test-hold feature. sgi_odyssey_hold_next_slice:
 * the board's thread holds its next slice open for up to ms milliseconds or until
 * sgi_odyssey_release_hold, so a read that waits answers BUSY meanwhile (OK in
 * threaded mode, ERROR in inline mode). sgi_odyssey_slice_open: 1 while the
 * thread is inside a slice.
 */
int sgi_odyssey_hold_next_slice(IrisEngine *h, uint32_t ms);
int sgi_odyssey_release_hold(IrisEngine *h);
int sgi_odyssey_slice_open(IrisEngine *h);

/*
 * The cursor's position (signed, in pixels) and whether it is drawn, as the display
 * registers hold them.
 */
int sgi_odyssey_cursor(IrisEngine *h, int32_t *x, int32_t *y, uint32_t *shown);

/*
 * Wall time in ns the board's threads (the command FIFO's and the raster unit's) have spent running work since the
 * engine was built, not waiting for it, summed over them, so it can exceed wall time. The same number as
 * iris_engine_busy_ns.
 */
uint64_t sgi_odyssey_busy_ns(IrisEngine *h);

/* What the GPU unit has done. */
typedef struct IrisVproGpuCounters {
    uint64_t records;
    uint64_t batches;
    uint64_t copies;
    uint64_t readbacks;
    uint64_t rejections;
    uint64_t faults;
} IrisVproGpuCounters;

/*
 * Fill *out with the GPU unit's counters, so a shell can assert that a GPU
 * run executed GPU records. ERROR when the engine has no GPU unit; FAULT on a
 * faulted engine.
 */
int sgi_odyssey_gpu_counters(IrisEngine *h, IrisVproGpuCounters *out);

/*
 * One cell of IrisVproDmaStats: the descriptors the board completed and their
 * pixels (clipped ones included), and the part of them that went by row stores
 * (screen destinations only).
 */
typedef struct IrisVproDmaCell {
    uint64_t descriptors;
    uint64_t pixels;
    uint64_t fast_descriptors;
    uint64_t fast_pixels;
} IrisVproDmaCell;

/*
 * What the guest's DMA descriptors did since the engine was built. seen counts
 * descriptors when they start and the cells when they complete, so seen less
 * the cells and declined is what failed (a bus error, a header that disagrees
 * with itself); a read never shows seen below the rest. declined counts
 * descriptors the board does not model. cells[kind][bytes]: kind 0 screen,
 * 1 memory object, 2 draw-sentinel upload, 3 to host; bytes is the source
 * pixel's size. slow[why] counts the screen descriptors that did not go wholly
 * by row stores: 0 GPU unit, 1 not 4-byte pixels, 2 scale or bias, 3 source
 * code outside RGBA, 4 a row clipped or a field the row store refuses.
 */
typedef struct IrisVproDmaStats {
    uint64_t seen;
    uint64_t declined;
    uint64_t slow[5];
    IrisVproDmaCell cells[4][17];
} IrisVproDmaStats;

/*
 * Fill *out with the DMA counters. ERROR when out is NULL. Does not wait for
 * the board.
 */
int sgi_odyssey_dma_stats(IrisEngine *h, IrisVproDmaStats *out);

/*
 * Bring the pixels of a box (corners included, every plane under it) current
 * for a CPU reader. Waits for queued CPU work, then only for GPU work that
 * touches the box.
 */
int sgi_odyssey_observe(IrisEngine *h, int32_t x0, int32_t y0, int32_t x1,
                      int32_t y1);

/*
 * Copy the composed scan-out into buf (cap 32-bit words): 0x00RRGGBB a pixel,
 * row by row from the top. *w and *hgt are set to the frame's size: 0 by 0
 * while no video mode is set (OK, nothing copied), or the size needed when
 * cap is too small (ERROR, nothing copied). FAULT on a faulted engine. Without
 * a GPU unit nothing waits for the board, so a drawing in progress may show
 * for one frame; with one, the call drains the board and reads its memory back.
 */
int sgi_odyssey_scanout(IrisEngine *h, uint32_t *buf, size_t cap, uint32_t *w,
                      uint32_t *hgt);

#ifdef __cplusplus
}
#endif

#endif /* SGI_ODYSSEY_H */
