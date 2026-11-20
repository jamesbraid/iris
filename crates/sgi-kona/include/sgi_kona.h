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
    uint32_t pool_threads;      /* tile workers, at most 256; 0: draw on the caller's thread */
    uint32_t pool_tile;         /* tile size in pixels */
    /* The boards. NULL or "" takes the product's own; the display generator's own is the
     * DG5-8, or the DG5-2 behind a host board. */
    const char *geometry;       /* "ge14-4", "ge14-2" or "ge16-4", as the product takes them */
    const char *rm_type;        /* "rm7" or "rm9": the product's, or refused */
    const char *display;        /* "dg5-8" or "dg5-2" */
    const char *pixel_depth;    /* "small" or "medium"; NULL is small */
    uint32_t texture_mb;        /* 0 or the product's texture per raster manager, in MB */
    uint32_t channels;          /* 0 or the display generator's channel count */
    uint32_t ktown2;            /* the Onyx 3400's Ktown2 is fitted: it allows four raster managers */
    /* The one-wire parts fitted besides the geometry board's, by part number: "030-1067-001"
     * is the Ktown of a rack, whose record joins the chain. Any other part is refused. */
    const char *const *parts;
    uint32_t num_parts;
    uint32_t shared_identity;   /* the slot whose Ktown serial the record follows, plus one (1 to 4); 0: own */
    uint64_t geometry_serial;   /* the geometry board part's 48-bit ROM serial; 0: the synthetic one */
    void *user;                 /* the argument of the callbacks */
    const uint8_t *flash_image; /* the flash's contents, flash_len bytes; NULL: a new flash */
    uint32_t flash_len;         /* 0, or exactly 0x80000 */
    /* Every range the pipe programs, and the whole image after a snapshot loads. A NULL callback
     * keeps the flash in memory; an image needs it. */
    void (*flash_write)(void *user, uint32_t offset, const uint8_t *data, size_t len);
    /* The widget the pipe's DMA goes to (0 to 15), before the engine returns from the call that
     * created it or changed it: the shell's bus callbacks name it. */
    void (*dma_widget)(void *user, uint32_t widget);
    /* Bit 0: the shell models the display option board and will give sgi_kona_set_display_bus its
     * device, which lets display name "dg5-2-gvo". Zero for a caller that predates the field. */
    uint32_t flags;
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

/*
 * A Crossbow link reset reached the widget: its widget control, interrupt destination and
 * interrupt vector registers go back to their reset values; the rest of the pipe, the map RAM
 * included, stays. Not a power-on (iris_engine_reset): use it for the link reset.
 */
int sgi_kona_link_reset(IrisEngine *h);

/* Bring a box of pixels current for a CPU reader. */
int sgi_kona_observe(IrisEngine *h, const IrisKonaRegion *r);

/*
 * Copy the composed scan-out into buf (cap 32-bit words): 0x00RRGGBB a pixel,
 * row by row from the top. *w and *hgt are set to the frame's size also when
 * cap is too small (ERROR, nothing copied). FAULT on a faulted engine. The call
 * waits for the drawing queued behind the frame.
 */
int sgi_kona_scanout(IrisEngine *h, uint32_t *buf, size_t cap, uint32_t *w,
                     uint32_t *hgt);

/*
 * The shell's device for the display option board (a GVO), for a pipe configured with "dg5-2-gvo":
 * the engine calls read for a read of display-bus word addr and write for a store, as the display
 * generator's firmware does for the board when the X server probes it (a write of id + 0x3e8 to the
 * video output chips' register 0x31, at 0xc831 and 0xc931, read back), loads its logic (0x8900 clear, bytes
 * to 0x8a02 polled for bit 7) and sets its data format (0x8400, 0xc006, 0xc214 to 0xc216), and for the
 * diagnostic requests that read and write a display-bus word. read returns 1 with *data set when the
 * board has the register, 0 when it does not; write returns 1 when the board took the store. Both are
 * called from the engine's calls, on the thread making them, and must not call the engine. NULL for both
 * detaches. It survives a reset and a load.
 */
int sgi_kona_set_display_bus(IrisEngine *h, void *user,
                             int (*read)(void *user, uint32_t addr, uint32_t *data),
                             int (*write)(void *user, uint32_t addr, uint32_t data));

/*
 * Channel channel's output window: the rectangle of the managed area its output shows, set to *x,
 * *y (from the top), *w and *hgt. ERROR for a channel that does not exist or does not run (nothing set).
 * The display channels are 0 and 1; the option board's, once the firmware has found it, is the next, and
 * the option board's own VOC id (0xc6) names it too.
 */
int sgi_kona_channel_window(IrisEngine *h, uint32_t channel, uint32_t *x, uint32_t *y,
                            uint32_t *w, uint32_t *hgt);

/*
 * The pixels of that rectangle into buf (cap 32-bit words): 0x00RRGGBB a pixel, row by row from the
 * top. *w and *hgt are set to its size, also when cap is too small (ERROR, nothing copied); ERROR
 * with them untouched for a channel that does not exist or does not run. FAULT on a faulted engine.
 * The call waits for the drawing queued behind the frame.
 */
int sgi_kona_channel_scanout(IrisEngine *h, uint32_t channel, uint32_t *buf, size_t cap, uint32_t *w,
                             uint32_t *hgt);

#ifdef __cplusplus
}
#endif

#endif /* SGI_KONA_H */
