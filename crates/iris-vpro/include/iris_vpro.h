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
    const char *product;    /* "v12", "v10", "v8" or "v6"; NULL is refused */
    uint32_t widget;        /* the widget number the board answers as */
    uint32_t pool_threads;  /* the fragment pool's workers; 0: a default */
    uint32_t pool_tile;     /* its tile size in pixels; 0: a default */
} IrisVproConfig;

/*
 * Bytes of board memory the product carries: the lent_len to give in the
 * shared configuration. 0 for a product that does not exist.
 */
uint64_t iris_vpro_memory_bytes(const char *product);

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

#ifdef __cplusplus
}
#endif

#endif /* IRIS_VPRO_H */
