/*
 * C interface of the MGRAS (IMPACT) graphics engine.
 *
 * Kept in step with src/ffi.rs by tools/check-header.
 *
 * Calls come from one thread at a time. The engine takes no lock of the
 * caller's and calls back only into the host memory callbacks and the
 * notify callback. A panic inside the engine stops it with a device fault:
 * the call answers IRIS_MGRAS_FAULT, and so does every later call until
 * iris_mgras_reset().
 */

#ifndef IRIS_MGRAS_H
#define IRIS_MGRAS_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define IRIS_MGRAS_ABI_VERSION 1

/* Return codes. */
#define IRIS_MGRAS_OK      0
#define IRIS_MGRAS_BUSY    1
#define IRIS_MGRAS_FAULT   2
#define IRIS_MGRAS_ERROR   3

/* IrisMgrasEvent.kind */
#define IRIS_MGRAS_EVENT_FLAGS         1
#define IRIS_MGRAS_EVENT_DAMAGE        2
#define IRIS_MGRAS_EVENT_NOTE          3
#define IRIS_MGRAS_EVENT_BUS_FAULT     4
#define IRIS_MGRAS_EVENT_DEVICE_FAULT  5

typedef struct IrisMgras IrisMgras;

/*
 * Host memory. swap is 0 for none, 1, 2 or 3 to swap the bytes within each
 * 16, 32 or 64 bits. Callbacks return 0, or non-zero for an address that
 * is not RAM (ram_*) or not reachable (bus_*).
 *
 * ram_read and ram_write are called from the engine's worker thread and
 * must be lock-free and reach RAM only. bus_read and bus_write may reach
 * MMIO and take locks; they are called only inside a call into the engine.
 * They may be NULL, and the RAM callbacks serve.
 */
typedef struct IrisMgrasMemOps {
    int (*ram_read)(void *user, uint64_t addr, uint8_t *buf, size_t len,
                    uint32_t swap);
    int (*ram_write)(void *user, uint64_t addr, const uint8_t *buf,
                     size_t len, uint32_t swap);
    int (*bus_read)(void *user, uint64_t addr, uint8_t *buf, size_t len,
                    uint32_t swap);
    int (*bus_write)(void *user, uint64_t addr, const uint8_t *buf,
                     size_t len, uint32_t swap);
} IrisMgrasMemOps;

typedef struct IrisMgrasConfig {
    uint32_t abi_version;   /* IRIS_MGRAS_ABI_VERSION */
    uint32_t board;         /* 0 Solid IMPACT, 1 High, 2 Maximum */
    uint32_t flags;         /* bit 0: the Octane's HQ4 front */
    uint32_t mode;          /* 0 inline, 1 threaded */
    uint32_t queue_ops;     /* writes queued before BUSY; 0: default */
    int32_t jit;            /* -1 default, 0 off, 1 on first use, 2 background */
} IrisMgrasConfig;

typedef struct IrisMgrasPump {
    uint32_t idle;          /* nothing is queued and nothing runs */
    uint32_t events;        /* events wait */
    uint32_t has_deadline;  /* next_deadline is valid */
    uint64_t next_deadline; /* when to call iris_mgras_advance next, in ns */
} IrisMgrasPump;

typedef struct IrisMgrasEvent {
    uint32_t kind;
    uint32_t write;         /* BUS_FAULT: non-zero for a write */
    uint64_t store;         /* DAMAGE: store key; BUS_FAULT: address */
    uint64_t planes;        /* DAMAGE: planes that changed */
    int32_t x0;             /* DAMAGE: the box, corners included */
    int32_t y0;
    int32_t x1;
    int32_t y1;
    char text[96];          /* NOTE, DEVICE_FAULT: NUL-terminated, cut to fit */
} IrisMgrasEvent;

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
 * Start an engine. notify may be NULL; it is called from any thread when work
 * waits or an event is posted, and must only schedule the caller's pump. NULL
 * on bad arguments or failure.
 */
IrisMgras *iris_mgras_new(const IrisMgrasConfig *cfg,
                          const IrisMgrasMemOps *mem, void *mem_user,
                          void (*notify)(void *), void *notify_user);

/* Stop the engine and free the handle. */
void iris_mgras_free(IrisMgras *h);

/*
 * A register or FIFO write at a host-window offset, 32 or 64 bits wide.
 * IRIS_MGRAS_BUSY: the queue is full and nothing was taken; run or wait,
 * then retry.
 */
int iris_mgras_write(IrisMgras *h, uint32_t off, uint32_t bits,
                     uint64_t val);

/* A batch of FIFO words into a port (0 user, 1 privileged); returns the
 * number taken. */
size_t iris_mgras_push_words(IrisMgras *h, uint32_t port,
                             const uint32_t *words, size_t n);

/*
 * A register read. Status registers and flags never wait. Others answer
 * IRIS_MGRAS_BUSY while queued work could change the answer.
 */
int iris_mgras_read(IrisMgras *h, uint32_t off, uint32_t bits,
                    uint64_t *val);

/* Whether a write (write non-zero) or a read at off would answer BUSY now:
 * 1 if it would, 0 if not, IRIS_MGRAS_FAULT on a device fault. */
int iris_mgras_access_waits(IrisMgras *h, uint32_t off, uint32_t bits,
                            uint32_t write);

/* Inline mode: run queued work, at most max_ops operations; returns how many
 * ran. In threaded mode, 0. */
size_t iris_mgras_run(IrisMgras *h, size_t max_ops);

/* The pump, at the caller's time now_ns. Hands queued work over; never waits
 * and never runs work. */
int iris_mgras_advance(IrisMgras *h, uint64_t now_ns, IrisMgrasPump *pump);

/*
 * Wait for the engine's CPU work for at most budget_ns of host time (inline
 * mode runs the queue instead). Returns 1 if idle, 0 if not,
 * IRIS_MGRAS_FAULT on a device fault. Never waits for the GPU.
 */
int iris_mgras_wait_idle(IrisMgras *h, uint64_t now_ns, uint64_t budget_ns);

/* Move up to cap events into out, oldest first; returns how many. */
size_t iris_mgras_drain_events(IrisMgras *h, IrisMgrasEvent *out, size_t cap);

/* Bring a box of pixels current for a CPU reader. Waits for queued CPU work,
 * then only for GPU work that touches the box. */
int iris_mgras_observe(IrisMgras *h, const IrisMgrasRegion *r);

/* The pixel at (x, y) of the buffer r names, as stored. */
int iris_mgras_pixel(IrisMgras *h, const IrisMgrasRegion *r, uint32_t x,
                     uint32_t y, uint64_t *val);

/* Bring all memory current. Needs a quiesced engine. */
int iris_mgras_observe_all(IrisMgras *h);

/*
 * Run everything queued and park the worker. Call before save, load, reset
 * and observe_all; call resume to carry on.
 */
int iris_mgras_quiesce(IrisMgras *h);
int iris_mgras_resume(IrisMgras *h);

/* Power-on state; clears a fault. Needs a quiesced engine. */
int iris_mgras_reset(IrisMgras *h);

/*
 * Save the state into the handle (needs a quiesced engine), then read it
 * with state_len and state_copy. A state loaded back is checked whole: one
 * that fails leaves the engine as it was.
 */
int iris_mgras_save(IrisMgras *h);
size_t iris_mgras_state_len(IrisMgras *h);
size_t iris_mgras_state_copy(IrisMgras *h, uint8_t *buf, size_t cap);
int iris_mgras_load(IrisMgras *h, const uint8_t *buf, size_t len);

/* The device-fault message, or NULL. Valid until the next call. */
const char *iris_mgras_fault(IrisMgras *h);

/* The HQ flags as the board holds them. */
uint32_t iris_mgras_flags(IrisMgras *h);

#ifdef __cplusplus
}
#endif

#endif /* IRIS_MGRAS_H */
