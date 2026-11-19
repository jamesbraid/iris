/*
 * C interface of the VPro (Odyssey) graphics engine: its constructor and the
 * entries that name its own types. Everything else (write, read, pump,
 * events, state, lent memory) is iris_engine.h, whose call contract applies.
 *
 * Kept in step with src/ffi.rs by tools/check-header.
 */

#ifndef IRIS_VPRO_H
#define IRIS_VPRO_H

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
uint64_t iris_vpro_memory_bytes(const IrisVproConfig *cfg);

/*
 * Start an engine. Free it with iris_engine_free. NULL on bad arguments or
 * failure (the reason goes to the log). The board memory may be lent
 * (lent_ptr, lent_len == iris_vpro_memory_bytes): the engine then draws in
 * it and reports what it wrote through iris_engine_drain_dirty.
 */
IrisEngine *iris_vpro_new(const IrisVproConfig *cfg,
                          const IrisEngineHost *host);

/*
 * Bring the pixels of a box (corners included, every plane under it) current
 * for a CPU reader. Waits for queued CPU work, then only for GPU work that
 * touches the box.
 */
int iris_vpro_observe(IrisEngine *h, int32_t x0, int32_t y0, int32_t x1,
                      int32_t y1);

/*
 * Copy the composed scan-out into buf (cap 32-bit words): 0x00RRGGBB a pixel,
 * row by row from the top. *w and *hgt are set to the frame's size: 0 by 0
 * while no video mode is set (OK, nothing copied), or the size needed when
 * cap is too small (ERROR, nothing copied). FAULT on a faulted engine. Without
 * a GPU unit nothing waits for the board, so a drawing in progress may show
 * for one frame; with one, the call drains the board and reads its memory back.
 */
int iris_vpro_scanout(IrisEngine *h, uint32_t *buf, size_t cap, uint32_t *w,
                      uint32_t *hgt);

#ifdef __cplusplus
}
#endif

#endif /* IRIS_VPRO_H */
