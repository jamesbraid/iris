/*
 * C interface of the GR2 (Express) graphics engine: its constructor and the entries that name its own
 * types. Everything else (write, read, pump, events, state) is iris_engine.h, whose call contract applies.
 *
 * The window is the board's 4 MB slot on the GIO bus, offsets from its base. 8, 16 and 32 bit accesses
 * carry the register's value whatever the lane; 64 bit stores are taken as two words. The HQ2_GEDMA port
 * (offset 0x6a068) is the port iris_engine_push_words fills and iris_engine_pull_words empties.
 *
 * Kept in step with src/ffi.rs by tools/check-header.
 */

#ifndef SGI_GR2_H
#define SGI_GR2_H

#include <stddef.h>
#include <stdint.h>

#include "iris_engine.h"

#ifdef __cplusplus
extern "C" {
#endif

/* The GR2 configuration: the shared part, then the board's. */
typedef struct IrisGr2Config {
    IrisEngineConfig common;
    uint32_t ges;           /* geometry engines fitted: 1, 2, 4 or 8 */
    uint32_t ports[4];      /* what the board configuration ports (+0x6c000 to +0x6c00c) read as */
    uint32_t hq_rev;        /* the HQ2 revision in the status word and in hq_gepc */
    uint32_t aux_window;    /* the window at +0x6c1c0 to +0x6c1fc: 0 not decoded (an Indigo), 1 iris's own word, 2 sixteen byte cells */
    int32_t jit;            /* RE3 span JIT: -1 default, 0 off, 1 on first use, 2 background */
} IrisGr2Config;

/*
 * Bit 0 of iris_engine_irq_levels (and of Pump.irq_levels) is the HQ2 FIFO level, which the shell wires to the board's
 * GIO interrupt line 0. It is up while the command FIFO's count is at or above the full threshold the guest programmed.
 */
#define IRIS_GR2_IRQ_FIFO 1       /* 1 << 0 */

/*
 * Start an engine. Free it with iris_engine_free. NULL on bad arguments or failure (the reason goes to
 * the log). The HQ2 and RE3 run on threads, so common.mode must be 1; the board has its own memory
 * (lent_len 0) and composes on the CPU (gpu IRIS_ENGINE_GPU_NONE).
 */
IrisEngine *sgi_gr2_new(const IrisGr2Config *cfg, const IrisEngineHost *host);

/* The hardware cursor as the VC1's registers hold it: the glyph's top-left pixel in screen pixels from the
 * top left in *x and *y, set whether or not it is shown. Returns 1 when it is shown, 0 when not or for a
 * NULL output. */
int sgi_gr2_cursor_position(IrisEngine *h, int32_t *x, int32_t *y);

/* Rows that may have changed since the last call: up to cap (first, end) pairs into runs, in row order, their
 * number in *n (0 for none); past cap the nearest runs are joined. FAULT on a faulted engine. */
int sgi_gr2_take_damage(IrisEngine *h, uint32_t *runs, size_t cap, size_t *n);

/* As iris_engine_scanout for rows first up to end only, written at their place in buf (row y from word y * *w),
 * other words untouched; cap must hold the whole picture (else ERROR, size set). Does not wait for drawing. */
int sgi_gr2_scanout_rows(IrisEngine *h, uint32_t first, uint32_t end, uint32_t *buf, size_t cap, uint32_t *w,
                         uint32_t *hgt);

#ifdef __cplusplus
}
#endif

#endif
