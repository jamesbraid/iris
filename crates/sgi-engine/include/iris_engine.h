/*
 * C interface shared by the SGI board engines (MGRAS, VPro, KONA, ...).
 *
 * Kept in step with src/ffi.rs by tools/check-header. A board's own header
 * includes this one and adds its configuration, its constructor and the few
 * entries that name its own types.
 *
 * CALLS. Every call is made with the BQL held, one at a time; different
 * calls may come from different threads in turn. A call that comes in while
 * another is running on the same handle (from a callback, or another
 * thread) is refused without touching the engine: it answers as a fault
 * does (IRIS_ENGINE_FAULT, or 0 or NULL) and the device is not faulted by
 * it. Callbacks must not call any iris_engine_ or board function. Guard the
 * device's own MMIO region against DMA with MemReentrancyGuard.
 *
 * NEVER WAITS ON THE SHELL. No call waits for an input only the shell can
 * give. An engine whose stream holds for a frame or a time says so
 * (IRIS_ENGINE_STALLED_FRAME, _TIME in advance and wait_idle; MIDWAY from
 * quiesce). The shell's loop is: wait_idle; on STALLED_FRAME call
 * iris_engine_frame; on STALLED_TIME call iris_engine_advance at that time;
 * repeat. A read that answers BUSY on a bus that cannot retry runs that loop
 * without a bound, then reads again.
 *
 * FAULTS. After a device fault, write, read, observe_lent, observe_all,
 * save, load, advance and wait_idle answer IRIS_ENGINE_FAULT (advance and
 * wait_idle still fill their result: state FAULTED), push_words and run
 * return 0, access_waits returns 0, and quiesce answers FAULT but still
 * parks. drain_events, fault, last_error, irq_levels, state_version, resume,
 * reset and free keep working. A panic in the engine is a device fault too.
 * Containment needs a build that unwinds: iris_engine_contains_panics() says
 * whether this one does; a build with panic=abort aborts on a panic.
 *
 * QUIESCE LOOP. quiesce(budget) runs the queued work and answers OK, or
 * MIDWAY: the engine is stalled on the shell, or still busy at the budget,
 * and keeps running. On MIDWAY call iris_engine_wait_idle(now, 0); if it says
 * STALLED_FRAME call iris_engine_frame, if STALLED_TIME call
 * iris_engine_advance at that time, then call quiesce again. A VM-stop
 * handler and migration give up after a bounded number of tries and say so
 * (iris_engine_last_error has the reason): a guest that loops the engine must
 * cost a bounded wait, not a dead monitor. A faulted engine answers FAULT and
 * is parked.
 *
 * CLOCK. now_ns is one monotonic clock for iris_engine_frame, _advance and
 * _wait_idle: the VM's virtual time, not the host's. It must not go backwards
 * across calls, so a shell that ran the quiesce loop must not pass an older
 * time afterwards. The loop gives a stall early: a held swap or retrace is
 * released at the moment of a blocked read, not at the guest's next retrace.
 *
 * REGISTER VALUES. val in write and read is the register's value as a number
 * of bits wide (8, 16, 32 or 64), after the bus's byte order has been
 * applied: not a byte buffer, and not swapped again by the engine.
 *
 * VM STOP. Quiesce when the VM stops and resume when it runs again: a worker
 * still draining keeps writing guest RAM and lent memory, and migration's
 * final RAM pass runs before devices save. Read drain_dirty after the
 * quiesce.
 *
 * COMPLETION WORDS. A scalar the board publishes for the guest (a fence
 * token, a query's completion words) is one ram_write of its whole size, 2, 4
 * or 8 bytes, aligned: refuse it or store it atomically, and a word that
 * reaches no RAM is not half written. Block moves split as they like.
 *
 * RAM CALLBACKS. ram_read and ram_write are called from engine threads and
 * must be lock-free and reach RAM only. In QEMU: address_space_translate
 * under RCU, memory_region_is_ram, a copy, and the dirty marking; return
 * non-zero for MMIO and unmapped space. Never address_space_read or _write:
 * on an MMIO target they take the BQL, which the thread waiting for the
 * engine holds. addr is a bus address; the callback translates it, from
 * state that is safe to read without the BQL. bus_read and bus_write may reach MMIO and
 * take locks; they are called only on the calling thread, inside a call.
 * They may be NULL, and the RAM callbacks serve.
 *
 * NOTIFY. Called from any thread, possibly with engine locks held and on the
 * BQL thread: it may only schedule the pump (a bottom half). It is never
 * called after iris_engine_free returns, so the shell can delete the bottom
 * half then. In inline mode nothing runs the queue by itself: the bottom
 * half calls iris_engine_run.
 *
 * STRUCTURES that the caller builds carry the ABI version and their size
 * first, so a field added later reads as zero for an older caller.
 */

#ifndef IRIS_ENGINE_H
#define IRIS_ENGINE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define IRIS_ENGINE_ABI_VERSION 1

/* Return codes. */
#define IRIS_ENGINE_OK            0
#define IRIS_ENGINE_BUSY          1
#define IRIS_ENGINE_FAULT         2   /* the device faulted, or a call re-entered */
#define IRIS_ENGINE_ERROR         3   /* a bad argument, or a state callback failed */
#define IRIS_ENGINE_NOT_QUIESCED  4
#define IRIS_ENGINE_MIDWAY        5   /* stalled on the shell, or still busy: ask again */
#define IRIS_ENGINE_MISMATCH      6   /* the state does not fit this engine */

/* IrisEngineIdle.state */
#define IRIS_ENGINE_IDLE            0
#define IRIS_ENGINE_WORKING         1
#define IRIS_ENGINE_STALLED_FRAME   2
#define IRIS_ENGINE_STALLED_TIME    3
#define IRIS_ENGINE_FAULTED         4
#define IRIS_ENGINE_STOPPED         5   /* the command stream stopped for good; not a fault */

/* IrisEngineEvent.kind */
#define IRIS_ENGINE_EVENT_IRQ           1
#define IRIS_ENGINE_EVENT_DAMAGE        2
#define IRIS_ENGINE_EVENT_NOTE          3
#define IRIS_ENGINE_EVENT_BUS_FAULT     4
#define IRIS_ENGINE_EVENT_DEVICE_FAULT  5
#define IRIS_ENGINE_EVENT_GPU_LOST      6
#define IRIS_ENGINE_EVENT_INTERRUPT     7
#define IRIS_ENGINE_EVENT_CREDITS       8

/* IrisEngineConfig.gpu */
#define IRIS_ENGINE_GPU_NONE      0
#define IRIS_ENGINE_GPU_AUTO      1
#define IRIS_ENGINE_GPU_SOFTWARE  2

typedef struct IrisEngine IrisEngine;

/*
 * The shell's side. swap in the memory callbacks is 0 for none, 1, 2 or 3
 * to swap the bytes within each 16, 32 or 64 bits. They return 0, or
 * non-zero for an address that is not RAM (ram_*) or not reachable (bus_*).
 * user is passed to every callback.
 */
typedef struct IrisEngineHost {
    uint32_t abi_version;   /* IRIS_ENGINE_ABI_VERSION */
    uint32_t struct_size;   /* sizeof(IrisEngineHost) */
    void *user;
    int (*ram_read)(void *user, uint64_t addr, uint8_t *buf, size_t len,
                    uint32_t swap);
    int (*ram_write)(void *user, uint64_t addr, const uint8_t *buf,
                     size_t len, uint32_t swap);
    int (*bus_read)(void *user, uint64_t addr, uint8_t *buf, size_t len,
                    uint32_t swap);
    int (*bus_write)(void *user, uint64_t addr, const uint8_t *buf,
                     size_t len, uint32_t swap);
    void (*notify)(void *user);
} IrisEngineHost;

/*
 * The configuration every board shares. A board's own configuration starts
 * with this and adds its fields after it.
 */
typedef struct IrisEngineConfig {
    uint32_t abi_version;   /* IRIS_ENGINE_ABI_VERSION */
    uint32_t struct_size;   /* sizeof the board's whole configuration */
    uint32_t mode;          /* 0 inline, 1 threaded */
    uint32_t queue_ops;     /* writes queued before BUSY; 0: default */
    uint32_t gpu;           /* IRIS_ENGINE_GPU_*: open a GPU unit */
    const char *gpu_backend; /* auto, vulkan, metal, dx12 or gl; NULL: auto */
    uint8_t *lent_ptr;      /* board memory lent to the engine: 8-aligned, */
    uint64_t lent_len;      /* whole words, valid until free; NULL, 0: none */
    uint64_t reserved[4];   /* the shared part grows here */
} IrisEngineConfig;

typedef struct IrisEngineIdle {
    uint32_t state;         /* IRIS_ENGINE_IDLE .. IRIS_ENGINE_FAULTED */
    uint64_t stall_time;    /* STALLED_TIME: advance to this, in ns */
} IrisEngineIdle;

typedef struct IrisEnginePump {
    IrisEngineIdle idle;
    uint32_t events;        /* events wait */
    uint32_t has_deadline;  /* next_deadline is valid */
    uint64_t next_deadline; /* when to call iris_engine_advance next, in ns */
    uint64_t irq_levels;    /* iris_engine_irq_levels() as of this call */
} IrisEnginePump;

/* A run of lent memory, in bytes from its start. */
typedef struct IrisEngineRun {
    uint64_t offset;
    uint64_t len;
} IrisEngineRun;

typedef struct IrisEngineDamage {
    uint64_t store;
    uint64_t planes;
    int32_t x0;             /* the box, corners included */
    int32_t y0;
    int32_t x1;
    int32_t y1;
} IrisEngineDamage;

typedef struct IrisEngineBusFault {
    uint64_t addr;
    uint32_t write;         /* non-zero for a write */
} IrisEngineBusFault;

typedef struct IrisEnginePacket {
    uint64_t addr;
    uint32_t sender;        /* the widget the packet comes from */
    uint32_t widget;
    uint32_t vector;
    uint32_t level;
} IrisEnginePacket;

typedef struct IrisEngineCredits {
    uint32_t source;
    uint32_t retired;
} IrisEngineCredits;

typedef union IrisEngineEventData {
    IrisEngineDamage damage;
    IrisEngineBusFault bus_fault;
    IrisEnginePacket packet;
    IrisEngineCredits credits;
    char text[96];          /* NUL-terminated, cut to fit */
} IrisEngineEventData;

/*
 * kind says which member of data is valid: DAMAGE damage, BUS_FAULT
 * bus_fault, INTERRUPT packet, CREDITS credits, NOTE, DEVICE_FAULT and
 * GPU_LOST text; IRQ none.
 */
typedef struct IrisEngineEvent {
    uint32_t kind;
    IrisEngineEventData data;
} IrisEngineEvent;

/* A piece of saved state. */
typedef struct IrisEngineChunk {
    char name[32];          /* NUL-terminated */
    uint64_t index;
    const uint8_t *bytes;   /* valid until the next callback */
    size_t len;
} IrisEngineChunk;

/*
 * Stop the engine and free the handle. The notify hook is not called after
 * it returns.
 */
void iris_engine_free(IrisEngine *h);

/* 1 if a panic in the engine becomes a device fault; 0 if this build aborts. */
int iris_engine_contains_panics(void);

/*
 * A register or FIFO write. BUSY: the queue is full and nothing was taken.
 * Charged stores earn credits (iris_engine_set_source).
 */
int iris_engine_write(IrisEngine *h, uint32_t off, uint32_t bits,
                      uint64_t val);

/* iris_engine_write with the shell's time at the store, in its advance time base. */
int iris_engine_write_at(IrisEngine *h, uint64_t now_ns, uint32_t off,
                         uint32_t bits, uint64_t val);

/* The number of scan-out heads iris_engine_scanout takes; 0 for a board with none. */
uint32_t iris_engine_heads(IrisEngine *h);

/*
 * Copy head's composed picture, one 0x00RRGGBB word a pixel, rows from the top,
 * into buf (cap words). *w and *hgt are the frame's size: 0 by 0 while the head
 * has no mode (OK, nothing copied), and the size needed when cap is too small
 * (ERROR, nothing copied). FAULT on a faulted engine.
 */
int iris_engine_scanout(IrisEngine *h, uint32_t head, uint32_t *buf,
                        size_t cap, uint32_t *w, uint32_t *hgt);

/* A head's scan-out timing: a frame is frame_clocks clocks of clock_hz in fields fields, so retrace n falls at
 * epoch + n * frame_clocks / (fields * clock_hz); lines lines in all. epoch_ns is when retrace 0 fell in the
 * iris_engine_advance time base, or -1 where the shell's clock starts the phase. generation changes whenever
 * the timing or the phase restarts. */
typedef struct IrisEngineHeadTiming {
    uint64_t frame_clocks;
    int64_t epoch_ns;
    uint64_t generation;
    uint32_t clock_hz;
    uint32_t fields;
    uint32_t lines;
    uint32_t reserved;
} IrisEngineHeadTiming;

/* The timing of head's scan-out: 1 with *t set, 0 for none (the shell keeps its default). */
int iris_engine_head_timing(IrisEngine *h, uint32_t head, IrisEngineHeadTiming *t);

/* As iris_engine_scanout, x2r10g10b10 a pixel (2 unused bits, then red, green, blue of 10): the top 8 bits of
 * each colour are the 8-bit scan-out's. A board with 8-bit colour maps repeats their top bits. */
int iris_engine_scanout10(IrisEngine *h, uint32_t head, uint32_t *buf,
                          size_t cap, uint32_t *w, uint32_t *hgt);

/* When head next needs iris_engine_advance, in ns: 1 with *when set, 0 for none. */
int iris_engine_head_deadline(IrisEngine *h, uint32_t head, uint64_t *when);

/* A batch of FIFO words into a port; returns the number taken. */
size_t iris_engine_push_words(IrisEngine *h, uint32_t port,
                              const uint32_t *words, size_t n);

/* A register read. BUSY while queued work could change the answer. */
int iris_engine_read(IrisEngine *h, uint32_t off, uint32_t bits,
                     uint64_t *val);

/*
 * Where the stores that follow come from, for flow control. id is the shell's
 * own number (a Hub, a CPU), which the engine echoes in CREDITS events and
 * never reads; id 0 with charged 0 is a store with no sender. A charged store
 * earns one credit once the board has retired it, returned in a CREDITS event
 * for that source: retired is how many credits to give back to source. A
 * source's credits coalesce into one event until drained, and reset drops the
 * credits still owed, so the shell zeroes its own counts when it resets.
 */
int iris_engine_set_source(IrisEngine *h, uint32_t id, uint32_t charged);

/* Whether a write (write non-zero) or a read at off would answer BUSY now:
 * 1 if it would, 0 if not. A faulted engine answers 0: the call answers FAULT. */
int iris_engine_access_waits(IrisEngine *h, uint32_t off, uint32_t bits,
                             uint32_t write);

/* Inline mode: run queued work, at most max_ops operations, stopping at a
 * stall; returns how many ran. In threaded mode, 0. */
size_t iris_engine_run(IrisEngine *h, size_t max_ops);

/* A frame boundary of the shell's scan-out timing, at now_ns. */
int iris_engine_frame(IrisEngine *h, uint64_t now_ns);

/*
 * The pump, at the shell's time now_ns. Hands queued work over, collects
 * what is due, never runs work and never waits. FAULT, with *pump filled, if
 * the engine stopped.
 */
int iris_engine_advance(IrisEngine *h, uint64_t now_ns, IrisEnginePump *pump);

/*
 * Wait for the engine's CPU work for at most budget_ns of host time (inline
 * mode runs the queue instead). A stalled engine answers at once with what
 * it needs. FAULT, with *idle filled, if the engine stopped.
 */
int iris_engine_wait_idle(IrisEngine *h, uint64_t now_ns, uint64_t budget_ns,
                          IrisEngineIdle *idle);

/*
 * The interrupt output lines, one bit per line, from atomics: never waits.
 * A level is state, not an edge: read it again after an IRQ event. A refused
 * call (a re-entering one) answers 0, which reads as every line low, so do not
 * call it from a callback. A board that signals by packets answers 0.
 */
uint64_t iris_engine_irq_levels(IrisEngine *h);

/*
 * Move up to cap events into out, oldest first; returns how many. Works on a
 * faulted engine. IRQ: levels may have changed, so read iris_engine_irq_levels;
 * coalesced, and posted by load and reset too. INTERRUPT: one packet for the
 * shell to deliver, which sets (level non-zero) or clears one pending bit
 * of the receiver, named by sender, widget, addr and vector. The engine keeps
 * the newest packet for each such line: a packet replaces the queued one, an
 * identical one is dropped, and after a drain the next is queued again. So
 * deliver every packet of one drain before any vCPU runs, in order: only then
 * is the collapse of a set and a clear to the clear invisible. Distinct lines
 * must reach distinct receiver bits (a receiver that decodes the vector alone
 * must not see two lines with one vector); a receiver that counts packets
 * cannot take this interface. After reset the receiver's pending
 * bits for the engine's lines are the shell's to lower. CREDITS: see
 * iris_engine_set_source. The text of NOTE, DEVICE_FAULT and GPU_LOST is cut
 * to 95 bytes; iris_engine_fault has a fault's whole text, and a GPU_LOST text
 * names the adapter.
 */
size_t iris_engine_drain_events(IrisEngine *h, IrisEngineEvent *out,
                                size_t cap);

/*
 * Lent memory. drain_dirty moves the runs the engine wrote since the last
 * call into out, up to cap, and keeps the rest for the next call: call it
 * until it returns less than cap (after quiesce too). Runs are whole
 * 16 KiB blocks but for the last, so there are at most (lent_len / 16 KiB + 1)
 * / 2 of them at once. trapped lists the ranges the shell must not map into
 * the guest: a CPU read calls observe_lent first, a CPU write calls
 * cpu_wrote after. The trapped ranges are fixed for the engine's life and
 * few; trapped returns at most cap of them.
 */
size_t iris_engine_drain_dirty(IrisEngine *h, IrisEngineRun *out, size_t cap);
size_t iris_engine_trapped(IrisEngine *h, IrisEngineRun *out, size_t cap);
int iris_engine_observe_lent(IrisEngine *h, uint64_t off, uint64_t len);
int iris_engine_cpu_wrote(IrisEngine *h, uint64_t off, uint64_t len);

/* Bring all memory current. Needs a quiesced engine. */
int iris_engine_observe_all(IrisEngine *h);

/*
 * Run everything queued and park the workers, for at most budget_ns of host
 * time (UINT64_MAX: no bound). MIDWAY: the engine is stalled on the shell or
 * still busy, and keeps running. Call before save, load, reset and
 * observe_all, and when the VM stops; call resume to carry on.
 */
int iris_engine_quiesce(IrisEngine *h, uint64_t budget_ns);
int iris_engine_resume(IrisEngine *h);

/* Power-on state; clears a fault. Needs a quiesced engine. */
int iris_engine_reset(IrisEngine *h);

/*
 * Stream the state to put, a chunk a call (meta first); put returns 0, or
 * non-zero to stop (then ERROR). Needs a quiesced engine.
 */
int iris_engine_save(IrisEngine *h,
                     int (*put)(void *user, const char *name, uint64_t index,
                                const uint8_t *bytes, size_t len),
                     void *user);

/*
 * Take a state from next, which fills a chunk and returns 1, or 0 at the end,
 * or negative to stop (then ERROR). Chunks are applied as they arrive: after
 * any failure call iris_engine_reset. Needs a quiesced engine. MISMATCH: the
 * state is another board's, version or size.
 */
int iris_engine_load(IrisEngine *h,
                     int (*next)(void *user, IrisEngineChunk *chunk),
                     void *user);

/* The version of the state save writes, for the shell's migration version. */
uint32_t iris_engine_state_version(IrisEngine *h);

/* The device-fault message, or NULL. Valid until the next call. */
const char *iris_engine_fault(IrisEngine *h);

/*
 * The reason for the last refusal (NOT_QUIESCED, MIDWAY, MISMATCH, a refusing
 * FAULT or a failed observe), for the shell's error message, or NULL. Valid
 * until the next refusal.
 */
const char *iris_engine_last_error(IrisEngine *h);

/*
 * Send the engine's log lines to cb(user, level, text): level 1 error, 2
 * warn, 3 info, 4 debug, 5 trace. max_level is the most verbose to pass (0:
 * none). cb may be called from any thread and must not call into the engine.
 * ERROR if the process already has another logger.
 */
int iris_engine_log_set(void (*cb)(void *user, int level, const char *text),
                        void *user, int max_level);

#ifdef __cplusplus
}
#endif

#endif /* IRIS_ENGINE_H */
