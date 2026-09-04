/*
 * The C consumer of spike #1052's staticlib — the non-App prong of D53's
 * fork. It links liborrery_unreal_direct.a, creates ONE handle (the existing
 * orrery_host from orrery_sim_host.h over Skirmish), owns the fixed-step
 * loop, and — this is the prong's content — owns everything the App prong
 * got from Bevy and lightyear: the D8 prediction ring, the input history,
 * the correction intake with its rollback-or-snap decision, the restore,
 * the replay with ring rewrite, and the residual the reconciliation monitor
 * would consume. All of it through orrery_host_snapshot / orrery_host_restore
 * / orrery_host_install_state / orrery_host_submit_command / orrery_host_step
 * and nothing else. The size of this file is the prong's cost.
 *
 * It measures the predicted-tick latency the way #920's observer measures
 * ipc_added (crates/orrery_ipc_transport/src/bench.rs) and the way spike
 * #1043's consumer measures inproc_added — same columns, same clock, same
 * pacing, same percentile formula — so the three sit on one graph, writing a
 * report in the orrery-ipc-harness/1 schema that scripts/ipc-report.py renders.
 *
 * Modes (argv[1]):
 *   bench      the measurement. Options:
 *                --entities N          (24)     craft installed; N=24 is #920's headline
 *                --ticks N             (36000)  sampled ticks; 10 min at 60 Hz
 *                --warmup N            (600)    ticks before sampling
 *                --correction-every K  (12)     ticks between authority corrections (5 Hz)
 *                --no-ring                      control: the host alone, no ring, no
 *                                               authority, no corrections — the exact
 *                                               shape of #1043's --no-app run
 *                --report PATH                  write the JSON report
 *                --note TEXT                    appended to the report's notes (repeatable)
 *   smoke      bench at 120 ticks, no warmup; prints one line
 *   rollback   the hash-for-hash proof of the driver, at depth 9 (D8's window):
 *              an identity correction replays to the same hashes; a divergent
 *              one changes them; the same divergent one again reproduces them
 *
 * What is measured per frame, in order, on the one thread that owns the loop:
 *   ring snapshot at the tick boundary       snapshot        (baseline column)
 *   honest orders for the remote craft       remote_inputs   (baseline column)
 *   t0  local input handed to the host   } hop_in    = t1 - t0   (the ABI call: decode + queue)
 *   t1  submit returned                  }
 *   t2  orrery_host_step returned          extract   = t2 - t1   (the tick, and its state hashes)
 *   t3  orrery_host_collect_states done    encode    = t3 - t2   (canonical bytes copied out)
 *   t4' one clock read later               hop_out   = t4' - t3  (there is no hop; this is its absence)
 *   td  every record decoded in C          decode_out = td - t4'
 *   ta  mirror actors written              phase     = ta - t4', phase_after_decode = ta - td
 *                                          ipc_added = td - t0   (here: inproc_added)
 *   drains                                 drains          (baseline column)
 *   the stand-in authority's own tick      authority_step  (baseline column; NOT the game thread's
 *                                                           cost in production — see the README)
 *   every K frames, one correction         rollback_depth_k (restore + install + k-tick replay
 *                                                           with ring rewrite), restore, replay
 *
 * ipc_added is defined exactly as #1043 defines inproc_added so the two
 * prongs compare like with like; the ring snapshot and the rollback are the
 * columns this prong ADDS and they are reported beside it, never folded in.
 * frame_total is everything the frame did, corrections included.
 */

#define _POSIX_C_SOURCE 200809L
#define _DEFAULT_SOURCE

#include "orrery_unreal_direct.h"

#include <dirent.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define TICK_HZ 60u
#define HITCH_NS 16700000ull
#define MAX_NOTES 32
#define MAX_THREAD_NAMES 64
#define COMM_LEN 32

/* D8/D16's rollback window: PredictConfig::rollback_ticks = 9
 * (crates/orrery_predict/src/config.rs:62). The ring holds the window plus
 * the boundary being stepped. */
#define WINDOW 9u
#define RING (WINDOW + 1u)
#define MAX_ENTITIES 256u

/* ---- clock and pacing, matching orrery_ipc_transport ---------------------- */

static uint64_t now_ns(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        fprintf(stderr, "clock_gettime(CLOCK_MONOTONIC) failed\n");
        exit(2);
    }
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

/* sleep_until_ns from crates/orrery_ipc_transport/src/lib.rs: sleep in ~1 ms
 * quanta, spin the last ~1.5 ms. Pacing never produces a timestamp. */
static void sleep_until_ns(uint64_t deadline) {
    for (;;) {
        uint64_t now = now_ns();
        if (now >= deadline) {
            return;
        }
        uint64_t remaining = deadline - now;
        if (remaining <= 1500000ull) {
            continue;
        }
        uint64_t ns = remaining - 1000000ull;
        struct timespec ts;
        ts.tv_sec = (time_t)(ns / 1000000000ull);
        ts.tv_nsec = (long)(ns % 1000000000ull);
        nanosleep(&ts, NULL);
    }
}

/* ---- byte helpers ------------------------------------------------------- */

static uint64_t read_u64(const uint8_t *at) {
    uint64_t value = 0;
    for (int i = 7; i >= 0; --i) {
        value = (value << 8) | at[i];
    }
    return value;
}

static uint32_t read_u32(const uint8_t *at) {
    uint32_t value = 0;
    for (int i = 3; i >= 0; --i) {
        value = (value << 8) | at[i];
    }
    return value;
}

static uint16_t read_u16(const uint8_t *at) {
    return (uint16_t)(at[0] | ((uint16_t)at[1] << 8));
}

static void write_u64(uint8_t *at, uint64_t value) {
    for (int i = 0; i < 8; ++i) {
        at[i] = (uint8_t)(value >> (8 * i));
    }
}

static void write_u32(uint8_t *at, uint32_t value) {
    for (int i = 0; i < 4; ++i) {
        at[i] = (uint8_t)(value >> (8 * i));
    }
}

/* ---- the C mirror of Craft::decode (orrery_games/src/skirmish/state.rs:104-127) */

typedef struct craft {
    uint8_t archetype;
    int64_t pos[3]; /* millimetres on the lattice (orrery_core/src/quantize.rs:21-26) */
    int64_t vel[3];
    int32_t yaw_urad;
    int32_t pitch_urad;
    int32_t hull;
    int32_t shield;
    uint16_t cooldown;
    uint32_t shots;
    uint64_t damage_dealt;
} craft;

static int decode_craft(const uint8_t *bytes, size_t len, craft *out) {
    if (len != ORRERY_SKIRMISH_CRAFT_BYTES) {
        return 0;
    }
    out->archetype = bytes[0];
    for (int axis = 0; axis < 3; ++axis) {
        out->pos[axis] = (int64_t)read_u64(bytes + 1 + 8 * axis);
        out->vel[axis] = (int64_t)read_u64(bytes + 25 + 8 * axis);
    }
    out->yaw_urad = (int32_t)read_u32(bytes + 49);
    out->pitch_urad = (int32_t)read_u32(bytes + 53);
    out->hull = (int32_t)read_u32(bytes + 57);
    out->shield = (int32_t)read_u32(bytes + 61);
    out->cooldown = read_u16(bytes + 65);
    out->shots = read_u32(bytes + 67);
    out->damage_dealt = read_u64(bytes + 71);
    return 1;
}

/* The mirror: what an actor's transform write would receive. */
typedef struct mirror_actor {
    uint64_t entity;
    int64_t pos[3];
    int32_t yaw_urad;
    int32_t pitch_urad;
    int32_t hull;
} mirror_actor;

/* The C mirror of Order::Thrust's encoding (skirmish/order.rs:117-126),
 * framed as the flat command orrery_host_submit_command takes. */
static size_t encode_thrust(uint8_t *out, uint64_t target, int32_t accel_mmss,
                            int32_t yaw_urad, int32_t pitch_urad) {
    write_u64(out, target);
    out[8] = 0;
    write_u32(out + 9, (uint32_t)accel_mmss);
    write_u32(out + 13, (uint32_t)yaw_urad);
    write_u32(out + 17, (uint32_t)pitch_urad);
    return 21;
}

/* ---- process observation ------------------------------------------------ */

typedef struct thread_name_count {
    char name[COMM_LEN];
    unsigned count;
} thread_name_count;

typedef struct thread_table {
    unsigned total;
    unsigned distinct;
    thread_name_count names[MAX_THREAD_NAMES];
} thread_table;

/* Read from the OS, never from the library's own claim of what it started. */
static void read_threads(thread_table *table) {
    memset(table, 0, sizeof *table);
    DIR *dir = opendir("/proc/self/task");
    if (dir == NULL) {
        return;
    }
    struct dirent *entry;
    while ((entry = readdir(dir)) != NULL) {
        if (entry->d_name[0] < '0' || entry->d_name[0] > '9') {
            continue;
        }
        char path[sizeof "/proc/self/task//comm" + sizeof entry->d_name];
        snprintf(path, sizeof path, "/proc/self/task/%s/comm", entry->d_name);
        FILE *comm = fopen(path, "r");
        if (comm == NULL) {
            continue;
        }
        char name[COMM_LEN] = {0};
        if (fgets(name, sizeof name, comm) != NULL) {
            name[strcspn(name, "\n")] = '\0';
        }
        fclose(comm);
        table->total += 1;
        unsigned i;
        for (i = 0; i < table->distinct; ++i) {
            if (strcmp(table->names[i].name, name) == 0) {
                table->names[i].count += 1;
                break;
            }
        }
        if (i == table->distinct && table->distinct < MAX_THREAD_NAMES) {
            snprintf(table->names[i].name, COMM_LEN, "%s", name);
            table->names[i].count = 1;
            table->distinct += 1;
        }
    }
    closedir(dir);
}

static void read_loadavg(double out[3]) {
    out[0] = out[1] = out[2] = 0.0;
    FILE *f = fopen("/proc/loadavg", "r");
    if (f == NULL) {
        return;
    }
    if (fscanf(f, "%lf %lf %lf", &out[0], &out[1], &out[2]) != 3) {
        out[0] = out[1] = out[2] = 0.0;
    }
    fclose(f);
}

/* ---- summaries, matching bench.rs Summary::of ---------------------------- */

typedef struct summary {
    size_t n;
    double mean_ns;
    uint64_t min_ns, p50_ns, p99_ns, p99_9_ns, max_ns;
} summary;

static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a;
    uint64_t y = *(const uint64_t *)b;
    return x < y ? -1 : x > y ? 1 : 0;
}

/* Nearest rank over the sorted samples: index = ceil(pct/100 * n), clamped
 * to [1, n], minus one. */
static uint64_t rank(const uint64_t *sorted, size_t n, double pct) {
    double index = (pct / 100.0) * (double)n;
    double ceiled = (double)(uint64_t)index;
    if (ceiled < index) {
        ceiled += 1.0;
    }
    if (ceiled < 1.0) {
        ceiled = 1.0;
    }
    size_t i = (size_t)ceiled - 1;
    return sorted[i < n - 1 ? i : n - 1];
}

static summary summarize(uint64_t *samples, size_t n) {
    summary s;
    memset(&s, 0, sizeof s);
    if (n == 0) {
        return s;
    }
    qsort(samples, n, sizeof *samples, cmp_u64);
    double total = 0.0;
    for (size_t i = 0; i < n; ++i) {
        total += (double)samples[i];
    }
    s.n = n;
    s.mean_ns = total / (double)n;
    s.min_ns = samples[0];
    s.p50_ns = rank(samples, n, 50.0);
    s.p99_ns = rank(samples, n, 99.0);
    s.p99_9_ns = rank(samples, n, 99.9);
    s.max_ns = samples[n - 1];
    return s;
}

static void json_summary(FILE *f, const char *name, uint64_t *samples, size_t n, int last) {
    summary s = summarize(samples, n);
    fprintf(f,
            "    \"%s\": {\"n\": %zu, \"mean_ns\": %.6f, \"min_ns\": %" PRIu64
            ", \"p50_ns\": %" PRIu64 ", \"p99_ns\": %" PRIu64 ", \"p99_9_ns\": %" PRIu64
            ", \"max_ns\": %" PRIu64 "}%s\n",
            name, s.n, s.mean_ns, s.min_ns, s.p50_ns, s.p99_ns, s.p99_9_ns, s.max_ns,
            last ? "" : ",");
}

static void json_string(FILE *f, const char *text) {
    fputc('"', f);
    for (const char *c = text; *c != '\0'; ++c) {
        if (*c == '"' || *c == '\\') {
            fputc('\\', f);
            fputc(*c, f);
        } else if (*c == '\n') {
            fputs("\\n", f);
        } else {
            fputc(*c, f);
        }
    }
    fputc('"', f);
}

static void json_threads(FILE *f, const char *name, const thread_table *t, int last) {
    fprintf(f, "    \"%s\": {\"total\": %u, \"by_name\": {", name, t->total);
    for (unsigned i = 0; i < t->distinct; ++i) {
        json_string(f, t->names[i].name);
        fprintf(f, ": %u%s", t->names[i].count, i + 1 < t->distinct ? ", " : "");
    }
    fprintf(f, "}}%s\n", last ? "" : ",");
}

/* ---- growable byte buffers ---------------------------------------------- */

typedef struct bytes {
    uint8_t *data;
    size_t len, cap;
} bytes;

static void bytes_reserve(bytes *b, size_t cap) {
    if (b->cap >= cap) {
        return;
    }
    size_t grown = b->cap ? b->cap : 4096;
    while (grown < cap) {
        grown *= 2;
    }
    uint8_t *data = realloc(b->data, grown);
    if (data == NULL) {
        fprintf(stderr, "out of memory\n");
        exit(2);
    }
    b->data = data;
    b->cap = grown;
}

static void bytes_append(bytes *b, const uint8_t *src, size_t len) {
    bytes_reserve(b, b->len + len);
    memcpy(b->data + b->len, src, len);
    b->len += len;
}

/* Call a (out, capacity, out_required) ABI function into a growable buffer,
 * retrying once on BUFFER_TOO_SMALL. */
#define CALL_INTO(b, expr)                                                          \
    do {                                                                            \
        size_t required_ = 0;                                                       \
        orrery_host_result r_ = (expr);                                             \
        if (r_ == ORRERY_HOST_BUFFER_TOO_SMALL) {                                   \
            bytes_reserve((b), required_);                                          \
            r_ = (expr);                                                            \
        }                                                                           \
        (b)->len = (r_ == ORRERY_HOST_OK) ? required_ : 0;                          \
        result = r_;                                                                \
    } while (0)

/* ---- the predictor: D8's ring and rollback, in the consumer -------------- */

/* One ring slot: the host as it stood at tick boundary T (next_tick == T),
 * before frame T's inputs were submitted, and every command frame T then
 * submitted, as [u32 len][command] records. Together they are exactly what
 * HostSnapshot::restore says it needs to reproduce the run
 * (crates/orrery_sim_host/src/lib.rs:711-720): the snapshot carries the
 * adapter-routed inputs already queued at the boundary (lib.rs:194-200), and
 * the log carries what the consumer submitted after it. */
typedef struct ring_slot {
    uint64_t tick;
    int valid;
    bytes snapshot;
    bytes inputs;
    /* The state hashes tick T produced, by entity index — kept so the
     * rollback proof can say "hash for hash" rather than "looks the same". */
    uint8_t hashes[MAX_ENTITIES][32];
    unsigned hash_count;
} ring_slot;

typedef struct predictor {
    orrery_host *host;
    uint32_t entities;
    ring_slot ring[RING];
    bytes scratch;
    orrery_host_state_hash *hash_records;
    /* counters */
    uint64_t input_dropped, snapshot_failed, restore_failed, replay_step_failed;
    uint64_t events_reemitted_by_replay;
} predictor;

static ring_slot *slot_for(predictor *p, uint64_t tick) {
    return &p->ring[tick % RING];
}

static int check(orrery_host_result result, const char *operation) {
    if (result == ORRERY_HOST_OK) {
        return 1;
    }
    fprintf(stderr, "%s failed with result %d\n", operation, (int)result);
    return 0;
}

/* The ring write: the host at boundary `tick`, before any of frame `tick`'s
 * inputs. Clears the slot's input log. Returns ns spent. */
static uint64_t ring_snapshot(predictor *p, uint64_t tick) {
    ring_slot *slot = slot_for(p, tick);
    uint64_t t0 = now_ns();
    orrery_host_result result;
    bytes *b = &slot->snapshot;
    CALL_INTO(b, orrery_host_snapshot(p->host, b->data, b->cap, &required_));
    if (result != ORRERY_HOST_OK) {
        p->snapshot_failed += 1;
        slot->valid = 0;
        return now_ns() - t0;
    }
    slot->tick = tick;
    slot->valid = 1;
    slot->inputs.len = 0;
    /* The slot's hashes are record_hashes' to reset: during a replay the
     * slot is re-snapshotted BEFORE its tick is re-stepped, and the hashes it
     * holds are what the replay is compared against. */
    return now_ns() - t0;
}

/* Submit one command for the frame at `tick` and log it in the ring: the
 * input history lightyear keeps as redundant input
 * (PredictConfig::redundant_input_ticks, config.rs:47), here kept only as
 * deep as the window because nothing retransmits it. */
static orrery_host_result submit_logged(predictor *p, uint64_t tick, const uint8_t *cmd,
                                        size_t len) {
    orrery_host_result r = orrery_host_submit_command(p->host, cmd, len);
    if (r != ORRERY_HOST_OK) {
        p->input_dropped += 1;
        return r;
    }
    uint8_t prefix[4];
    write_u32(prefix, (uint32_t)len);
    bytes_append(&slot_for(p, tick)->inputs, prefix, 4);
    bytes_append(&slot_for(p, tick)->inputs, cmd, len);
    return r;
}

/* Drain the hashes the last step produced into the slot for `tick`. */
static void record_hashes(predictor *p, uint64_t tick) {
    ring_slot *slot = slot_for(p, tick);
    size_t count = 0;
    orrery_host_result r =
        orrery_host_drain_state_hashes(p->host, p->hash_records, MAX_ENTITIES, &count);
    slot->hash_count = 0;
    if (r != ORRERY_HOST_OK) {
        return;
    }
    for (size_t i = 0; i < count && i < MAX_ENTITIES; ++i) {
        if (p->hash_records[i].tick != tick) {
            continue;
        }
        memcpy(slot->hashes[slot->hash_count], p->hash_records[i].hash, 32);
        slot->hash_count += 1;
    }
}

/* Compare the hashes the replay of `tick` produced against what the slot held
 * before; returns the number that differ, and overwrites the slot with the
 * replay's hashes (the ring now describes the corrected timeline). */
static unsigned rehash_and_compare(predictor *p, uint64_t tick) {
    ring_slot *slot = slot_for(p, tick);
    uint8_t before[MAX_ENTITIES][32];
    unsigned before_count = slot->hash_count;
    memcpy(before, slot->hashes, sizeof before);
    record_hashes(p, tick);
    unsigned differ = 0;
    unsigned n = slot->hash_count < before_count ? slot->hash_count : before_count;
    for (unsigned i = 0; i < n; ++i) {
        if (memcmp(before[i], slot->hashes[i], 32) != 0) {
            differ += 1;
        }
    }
    if (slot->hash_count != before_count) {
        differ += (slot->hash_count > before_count) ? slot->hash_count - before_count
                                                    : before_count - slot->hash_count;
    }
    return differ;
}

typedef enum correction_plan {
    PLAN_ROLLBACK = 0, /* AuthorityCorrectionPlan::Rollback (correction.rs:14-19) */
    PLAN_SNAP = 1      /* AuthorityCorrectionPlan::Snap     (correction.rs:20-24) */
} correction_plan;

typedef struct rollback_report {
    correction_plan plan;
    uint64_t depth;
    uint64_t restore_ns, install_ns, replay_ns, total_ns;
    unsigned hashes_changed; /* over the replayed ticks */
    int64_t residual_mm;     /* max-axis |pos_before - pos_after| of the corrected entity at now */
    int ok;
} rollback_report;

/* Read one entity's position out of the host. */
static int read_pos(orrery_host *host, uint64_t entity, int64_t pos[3]) {
    uint8_t state[ORRERY_SKIRMISH_CRAFT_BYTES];
    size_t required = 0;
    if (orrery_host_state(host, entity, state, sizeof state, &required) != ORRERY_HOST_OK) {
        return 0;
    }
    craft c;
    if (!decode_craft(state, required, &c)) {
        return 0;
    }
    pos[0] = c.pos[0];
    pos[1] = c.pos[1];
    pos[2] = c.pos[2];
    return 1;
}

/* The correction intake: authority says `entity` was `bytes` at boundary
 * `authoritative_tick`. This is authority_correction_plan
 * (crates/orrery_predict/src/correction.rs:71-85) followed by the driver D53
 * §5 says does not exist: restore the ring slot, install the authoritative
 * state at that tick, replay every logged frame since, rewriting the ring as
 * it goes, and read the residual off the corrected entity. */
static rollback_report apply_correction(predictor *p, uint64_t entity, uint64_t authoritative_tick,
                                        const uint8_t *state_bytes, size_t len, uint64_t now) {
    rollback_report r;
    memset(&r, 0, sizeof r);
    r.depth = now - authoritative_tick;
    int64_t before[3] = {0, 0, 0}, after[3] = {0, 0, 0};
    (void)read_pos(p->host, entity, before);
    uint64_t t0 = now_ns();

    ring_slot *slot = slot_for(p, authoritative_tick);
    if (r.depth > WINDOW || !slot->valid || slot->tick != authoritative_tick) {
        /* The tick has left the ring: snap simulation state, smooth
         * presentation (correction.rs:20-24). */
        r.plan = PLAN_SNAP;
        r.ok = orrery_host_install_state(p->host, entity, now, state_bytes, len) == ORRERY_HOST_OK;
        r.total_ns = now_ns() - t0;
        (void)read_pos(p->host, entity, after);
        for (int a = 0; a < 3; ++a) {
            int64_t d = after[a] - before[a];
            if (d < 0) {
                d = -d;
            }
            if (d > r.residual_mm) {
                r.residual_mm = d;
            }
        }
        return r;
    }

    r.plan = PLAN_ROLLBACK;
    if (orrery_host_restore(p->host, slot->snapshot.data, slot->snapshot.len) != ORRERY_HOST_OK) {
        p->restore_failed += 1;
        r.total_ns = now_ns() - t0;
        return r;
    }
    uint64_t t1 = now_ns();
    r.restore_ns = t1 - t0;
    if (orrery_host_install_state(p->host, entity, authoritative_tick, state_bytes, len) !=
        ORRERY_HOST_OK) {
        r.total_ns = now_ns() - t0;
        return r;
    }
    uint64_t t2 = now_ns();
    r.install_ns = t2 - t1;

    /* Replay: for each logged frame, re-snapshot the slot (the ring must
     * describe the corrected timeline, or the next correction restores the
     * abandoned one), resubmit its log, step once, re-hash. */
    r.ok = 1;
    for (uint64_t t = authoritative_tick; t < now; ++t) {
        ring_slot *s = slot_for(p, t);
        /* The input log must survive the re-snapshot: snapshot bytes and the
         * log are separate buffers in the slot, and ring_snapshot clears the
         * log — so keep it aside. */
        bytes log = s->inputs;
        memset(&s->inputs, 0, sizeof s->inputs);
        (void)ring_snapshot(p, t);
        s->inputs = log;
        size_t at = 0;
        while (at + 4 <= log.len) {
            uint32_t l = read_u32(log.data + at);
            at += 4;
            if (orrery_host_submit_command(p->host, log.data + at, l) != ORRERY_HOST_OK) {
                p->input_dropped += 1;
                r.ok = 0;
            }
            at += l;
        }
        uint64_t first = 0, next = 0;
        if (orrery_host_step(p->host, 1, &first, &next) != ORRERY_HOST_OK || first != t) {
            p->replay_step_failed += 1;
            r.ok = 0;
            break;
        }
        r.hashes_changed += rehash_and_compare(p, t);
    }
    /* The replay re-emits every event the abandoned timeline emitted; a
     * presentation layer that already played them must not play them twice.
     * Counted, because it is a real obligation the App prong's lightyear
     * rollback also has and this driver has to carry itself. */
    {
        orrery_host_result result;
        bytes *b = &p->scratch;
        CALL_INTO(b, orrery_host_drain_events(p->host, b->data, b->cap, &required_));
        if (result == ORRERY_HOST_OK) {
            size_t at = 0;
            while (at + 12 <= b->len) {
                uint32_t l = read_u32(b->data + at + 8);
                at += 12 + l;
                p->events_reemitted_by_replay += 1;
            }
        }
    }
    uint64_t t3 = now_ns();
    r.replay_ns = t3 - t2;
    r.total_ns = t3 - t0;

    (void)read_pos(p->host, entity, after);
    for (int a = 0; a < 3; ++a) {
        int64_t d = after[a] - before[a];
        if (d < 0) {
            d = -d;
        }
        if (d > r.residual_mm) {
            r.residual_mm = d;
        }
    }
    return r;
}

/* ---- hosts ---------------------------------------------------------------- */

static orrery_host *create_host(const uint8_t seed[32], uint32_t entities) {
    orrery_host *host = NULL;
    orrery_host_ruleset_identity id;
    if (orrery_host_abi_version() != ORRERY_SIM_HOST_ABI_VERSION) {
        fprintf(stderr, "host abi version mismatch\n");
        return NULL;
    }
    if (!check(orrery_skirmish_host_create(seed, 0, &host), "orrery_skirmish_host_create")) {
        return NULL;
    }
    /* decode_craft was written against Skirmish rules v3; refuse anything
     * else before decoding a byte (orrery_sim_host.h:22-23). */
    if (!check(orrery_host_ruleset_id(host, &id), "orrery_host_ruleset_id") || id.version != 3u) {
        fprintf(stderr, "ruleset id mismatch: version %" PRIu32 "\n", id.version);
        (void)orrery_host_destroy(host);
        return NULL;
    }
    uint8_t state[ORRERY_SKIRMISH_CRAFT_BYTES];
    for (uint32_t slot = 0; slot < entities; ++slot) {
        uint64_t entity = (uint64_t)slot + 1;
        size_t required = 0;
        if (!check(orrery_skirmish_spawn_state(entity, slot, state, sizeof state, &required),
                   "orrery_skirmish_spawn_state") ||
            required != sizeof state ||
            !check(orrery_host_install_state(host, entity, 0, state, required),
                   "orrery_host_install_state")) {
            (void)orrery_host_destroy(host);
            return NULL;
        }
    }
    return host;
}

static void predictor_init(predictor *p, orrery_host *host, uint32_t entities) {
    memset(p, 0, sizeof *p);
    p->host = host;
    p->entities = entities;
    p->hash_records = calloc(MAX_ENTITIES, sizeof *p->hash_records);
    if (p->hash_records == NULL) {
        fprintf(stderr, "out of memory\n");
        exit(2);
    }
    bytes_reserve(&p->scratch, 1 << 16);
    for (unsigned i = 0; i < RING; ++i) {
        bytes_reserve(&p->ring[i].snapshot, 1 << 14);
        bytes_reserve(&p->ring[i].inputs, 1 << 12);
    }
}

static void predictor_free(predictor *p) {
    for (unsigned i = 0; i < RING; ++i) {
        free(p->ring[i].snapshot.data);
        free(p->ring[i].inputs.data);
    }
    free(p->scratch.data);
    free(p->hash_records);
}

/* The stand-in authority: a second host with the same population, stepped on
 * the same remote inputs, that sees the local craft's input ONE TICK LATE —
 * the physical shape of a mispredict (the network delivered the input after
 * the authority's tick sealed). It exists only to manufacture authoritative
 * bytes for the corrections; in production it is a remote peer and costs
 * this thread nothing. Its per-tick state of the local craft is kept for the
 * window so a correction "at tick T" has bytes to carry. */
typedef struct authority {
    orrery_host *host;
    uint8_t local_state[RING][ORRERY_SKIRMISH_CRAFT_BYTES];
    uint64_t local_state_tick[RING];
    uint8_t pending_local[21];
    size_t pending_local_len;
    uint64_t input_dropped, step_failed;
} authority;

/* ---- the run ------------------------------------------------------------ */

typedef struct options {
    uint32_t entities;
    uint64_t ticks;
    uint64_t warmup;
    uint64_t correction_every;
    int with_ring;
    const char *report;
    const char *notes[MAX_NOTES];
    unsigned note_count;
} options;

typedef struct columns {
    uint64_t *hop_in, *extract, *encode, *hop_out, *decode_out, *phase, *phase_after_decode,
        *ipc_added, *snapshot, *remote_inputs, *drains, *authority_step, *frame_total;
    uint64_t *restore, *replay, *residual_mm;
    uint64_t *rollback_by_depth[WINDOW + 1];
    size_t rollback_count[WINDOW + 1];
} columns;

static uint64_t *column(uint64_t n) {
    uint64_t *c = calloc((size_t)n + 1, sizeof *c);
    if (c == NULL) {
        fprintf(stderr, "out of memory\n");
        exit(2);
    }
    return c;
}

static int run_bench(const options *opt, int smoke) {
    static const uint8_t seed[32] = {0x52, 0x10};
    const uint64_t period = 1000000000ull / TICK_HZ;
    const uint64_t total = opt->warmup + opt->ticks;
    const uint32_t n = opt->entities;

    double loadavg_start[3], loadavg_end[3];
    read_loadavg(loadavg_start);

    thread_table threads_before, threads_after_create, threads_end;
    read_threads(&threads_before);

    orrery_host *host = create_host(seed, n);
    if (host == NULL) {
        return 1;
    }
    read_threads(&threads_after_create);

    predictor pred;
    predictor_init(&pred, host, n);

    authority auth;
    memset(&auth, 0, sizeof auth);
    if (opt->with_ring) {
        auth.host = create_host(seed, n);
        if (auth.host == NULL) {
            return 1;
        }
    }

    /* Buffers, sized once. */
    const size_t record_bytes = 8 + 4 + ORRERY_SKIRMISH_CRAFT_BYTES;
    size_t states_cap = record_bytes * n;
    uint8_t *states = malloc(states_cap);
    craft *crafts = calloc(n, sizeof *crafts);
    mirror_actor *actors = calloc(n, sizeof *actors);
    size_t events_cap = 1 << 16;
    uint8_t *events = malloc(events_cap);
    size_t commands_cap = 1 << 12;
    uint8_t *commands = malloc(commands_cap);
    uint64_t *peers = calloc(n, sizeof *peers);
    if (states == NULL || crafts == NULL || actors == NULL || events == NULL || commands == NULL ||
        peers == NULL) {
        fprintf(stderr, "out of memory\n");
        return 2;
    }

    columns c;
    memset(&c, 0, sizeof c);
    c.hop_in = column(opt->ticks);
    c.extract = column(opt->ticks);
    c.encode = column(opt->ticks);
    c.hop_out = column(opt->ticks);
    c.decode_out = column(opt->ticks);
    c.phase = column(opt->ticks);
    c.phase_after_decode = column(opt->ticks);
    c.ipc_added = column(opt->ticks);
    c.snapshot = column(opt->ticks);
    c.remote_inputs = column(opt->ticks);
    c.drains = column(opt->ticks);
    c.authority_step = column(opt->ticks);
    c.frame_total = column(opt->ticks);
    uint64_t max_corrections = opt->correction_every ? total / opt->correction_every + 1 : 1;
    c.restore = column(max_corrections);
    c.replay = column(max_corrections);
    c.residual_mm = column(max_corrections);
    for (unsigned d = 0; d <= WINDOW; ++d) {
        c.rollback_by_depth[d] = column(max_corrections);
    }

    size_t samples = 0;
    uint64_t step_failed = 0, tick_overruns = 0, decode_failures = 0;
    uint64_t hitch_host_path = 0, hitch_frame = 0, hitch_rollback = 0;
    uint64_t events_total = 0, events_max_bytes = 0;
    uint64_t corrections = 0, corrections_rollback = 0, corrections_snap = 0, rollback_failed = 0;
    uint64_t hashes_changed_total = 0, replay_ticks_total = 0;
    uint64_t snapshot_bytes_max = 0;
    uint64_t checksum = 0;
    uint64_t host_tick = 0;
    if (!check(orrery_host_next_tick(host, &host_tick), "orrery_host_next_tick")) {
        return 1;
    }

    const uint64_t start = now_ns() + period;
    for (uint64_t tick = 0; tick < total; ++tick) {
        sleep_until_ns(start + tick * period);
        const int sampling = tick >= opt->warmup;
        const uint64_t t_frame = now_ns();

        /* 1. The ring write at the boundary. The prong's first added cost. */
        uint64_t snapshot_ns = 0;
        if (opt->with_ring) {
            snapshot_ns = ring_snapshot(&pred, host_tick);
            size_t sz = slot_for(&pred, host_tick)->snapshot.len;
            if (sz > snapshot_bytes_max) {
                snapshot_bytes_max = sz;
            }
        }

        /* 2. What the remote population asks for this tick: the honest pilot
         *    for every craft but the local one, through the generic submit,
         *    logged in the ring. The authority gets the same bytes. */
        uint64_t tr0 = now_ns();
        for (uint32_t slot = 1; slot < n; ++slot) {
            uint64_t entity = (uint64_t)slot + 1;
            size_t peer_count = 0;
            for (uint32_t other = 0; other < n; ++other) {
                if (other != slot) {
                    peers[peer_count++] = (uint64_t)other + 1;
                }
            }
            size_t required = 0;
            orrery_host_result r = orrery_skirmish_honest_commands(
                seed, entity, slot, host_tick, peers, peer_count, commands, commands_cap,
                &required);
            if (r != ORRERY_HOST_OK) {
                pred.input_dropped += 1;
                continue;
            }
            size_t at = 0;
            while (at + 4 <= required) {
                uint32_t len = read_u32(commands + at);
                at += 4;
                if (opt->with_ring) {
                    (void)submit_logged(&pred, host_tick, commands + at, len);
                    if (orrery_host_submit_command(auth.host, commands + at, len) !=
                        ORRERY_HOST_OK) {
                        auth.input_dropped += 1;
                    }
                } else if (orrery_host_submit_command(host, commands + at, len) !=
                           ORRERY_HOST_OK) {
                    pred.input_dropped += 1;
                }
                at += len;
            }
        }
        uint64_t remote_inputs_ns = now_ns() - tr0;

        /* 3. The local player's input: one sample per tick, as #920's input
         *    batch carries one. t0 is the hand-over. The log write is outside
         *    hop_in so the column is the ABI call and nothing else. */
        uint8_t thrust[21];
        size_t thrust_len = encode_thrust(thrust, 1, 3000, 5000, (tick % 7 == 0) ? 200 : -200);
        const uint64_t t0 = now_ns();
        orrery_host_result submit = orrery_host_submit_command(host, thrust, thrust_len);
        const uint64_t t1 = now_ns();
        if (submit != ORRERY_HOST_OK) {
            pred.input_dropped += 1;
        } else if (opt->with_ring) {
            uint8_t prefix[4];
            write_u32(prefix, (uint32_t)thrust_len);
            bytes_append(&slot_for(&pred, host_tick)->inputs, prefix, 4);
            bytes_append(&slot_for(&pred, host_tick)->inputs, thrust, thrust_len);
        }

        /* 4. The tick. */
        uint64_t first = 0, next = 0;
        orrery_host_result stepped = orrery_host_step(host, 1, &first, &next);
        const uint64_t t2 = now_ns();
        if (stepped != ORRERY_HOST_OK) {
            step_failed += 1;
            fprintf(stderr, "orrery_host_step returned %d at tick %" PRIu64 "\n", (int)stepped,
                    tick);
            if (stepped == ORRERY_HOST_POISONED || stepped == ORRERY_HOST_PANIC) {
                break;
            }
        }
        const uint64_t executed_tick = host_tick;
        host_tick = next;

        /* 5. Canonical bytes out. */
        size_t states_len = 0;
        orrery_host_result collected =
            orrery_host_collect_states(host, states, states_cap, &states_len);
        const uint64_t t3 = now_ns();
        const uint64_t t_arrive = now_ns();
        int frame_ok = collected == ORRERY_HOST_OK;

        /* 6. Decode every record in C, then apply to the mirror. */
        uint32_t decoded = 0;
        if (frame_ok) {
            size_t at = 0;
            while (at + 12 <= states_len && decoded < n) {
                uint64_t entity = read_u64(states + at);
                uint32_t len = read_u32(states + at + 8);
                at += 12;
                if (!decode_craft(states + at, len, &crafts[decoded])) {
                    decode_failures += 1;
                    frame_ok = 0;
                    break;
                }
                actors[decoded].entity = entity;
                at += len;
                decoded += 1;
            }
        }
        const uint64_t t_decode = now_ns();
        if (frame_ok) {
            for (uint32_t i = 0; i < decoded; ++i) {
                actors[i].pos[0] = crafts[i].pos[0];
                actors[i].pos[1] = crafts[i].pos[1];
                actors[i].pos[2] = crafts[i].pos[2];
                actors[i].yaw_urad = crafts[i].yaw_urad;
                actors[i].pitch_urad = crafts[i].pitch_urad;
                actors[i].hull = crafts[i].hull;
                checksum = checksum * 0x100000001B3ull + actors[i].entity +
                           (uint64_t)actors[i].pos[0] + (uint64_t)crafts[i].shots;
            }
        }
        const uint64_t t_apply = now_ns();

        /* 7. Drains: hashes into the ring slot (or discarded), events so the
         *    adapter's routing is exercised and counted. */
        if (opt->with_ring) {
            record_hashes(&pred, executed_tick);
        } else {
            size_t hash_count = 0;
            (void)orrery_host_drain_state_hashes(host, pred.hash_records, MAX_ENTITIES,
                                                 &hash_count);
        }
        size_t event_bytes = 0;
        orrery_host_result drained = orrery_host_drain_events(host, events, events_cap, &event_bytes);
        if (drained == ORRERY_HOST_OK) {
            size_t at = 0;
            while (at + 12 <= event_bytes) {
                uint32_t len = read_u32(events + at + 8);
                at += 12 + len;
                events_total += 1;
            }
            if (event_bytes > events_max_bytes) {
                events_max_bytes = event_bytes;
            }
        }
        const uint64_t t_drained = now_ns();

        /* 8. The stand-in authority's tick: the local input it sees is the
         *    one from the previous frame. Attributed to its own column. */
        uint64_t authority_ns = 0;
        if (opt->with_ring) {
            uint64_t ta0 = now_ns();
            if (auth.pending_local_len > 0 &&
                orrery_host_submit_command(auth.host, auth.pending_local, auth.pending_local_len) !=
                    ORRERY_HOST_OK) {
                auth.input_dropped += 1;
            }
            memcpy(auth.pending_local, thrust, thrust_len);
            auth.pending_local_len = thrust_len;
            uint64_t af = 0, an = 0;
            if (orrery_host_step(auth.host, 1, &af, &an) != ORRERY_HOST_OK) {
                auth.step_failed += 1;
            }
            size_t hc = 0;
            (void)orrery_host_drain_state_hashes(auth.host, pred.hash_records, MAX_ENTITIES, &hc);
            size_t eb = 0;
            (void)orrery_host_drain_events(auth.host, events, events_cap, &eb);
            size_t required = 0;
            unsigned ring_i = (unsigned)(an % RING);
            if (orrery_host_state(auth.host, 1, auth.local_state[ring_i],
                                  ORRERY_SKIRMISH_CRAFT_BYTES, &required) == ORRERY_HOST_OK) {
                auth.local_state_tick[ring_i] = an;
            }
            authority_ns = now_ns() - ta0;
        }

        /* 9. Every K frames, a correction arrives for the local craft at a
         *    depth that cycles 1..WINDOW, carrying the authority's bytes at
         *    that boundary. */
        if (opt->with_ring && opt->correction_every > 0 && tick >= WINDOW + 1 &&
            tick % opt->correction_every == 0) {
            uint64_t depth = 1 + (corrections % WINDOW);
            uint64_t at_tick = host_tick - depth;
            unsigned ring_i = (unsigned)(at_tick % RING);
            if (auth.local_state_tick[ring_i] == at_tick) {
                rollback_report rr = apply_correction(&pred, 1, at_tick, auth.local_state[ring_i],
                                                      ORRERY_SKIRMISH_CRAFT_BYTES, host_tick);
                if (rr.plan == PLAN_ROLLBACK) {
                    corrections_rollback += 1;
                    if (sampling) {
                        size_t k = c.rollback_count[depth]++;
                        c.rollback_by_depth[depth][k] = rr.total_ns;
                        c.restore[corrections] = rr.restore_ns;
                        c.replay[corrections] = rr.replay_ns;
                        c.residual_mm[corrections] = (uint64_t)rr.residual_mm;
                    }
                    hashes_changed_total += rr.hashes_changed;
                    replay_ticks_total += depth;
                } else {
                    corrections_snap += 1;
                }
                if (!rr.ok) {
                    rollback_failed += 1;
                }
                corrections += 1;
                if (rr.total_ns > HITCH_NS) {
                    hitch_rollback += 1;
                }
            }
        }
        const uint64_t t_end = now_ns();

        if (t_apply - tr0 > HITCH_NS) {
            hitch_host_path += 1;
        }
        if (t_end - t_frame > HITCH_NS) {
            hitch_frame += 1;
        }

        if (sampling && frame_ok && stepped == ORRERY_HOST_OK && submit == ORRERY_HOST_OK) {
            c.hop_in[samples] = t1 - t0;
            c.extract[samples] = t2 - t1;
            c.encode[samples] = t3 - t2;
            c.hop_out[samples] = t_arrive - t3;
            c.decode_out[samples] = t_decode - t_arrive;
            c.phase[samples] = t_apply - t_arrive;
            c.phase_after_decode[samples] = t_apply - t_decode;
            c.ipc_added[samples] = t_decode - t0;
            c.snapshot[samples] = snapshot_ns;
            c.remote_inputs[samples] = remote_inputs_ns;
            c.drains[samples] = t_drained - t_apply;
            c.authority_step[samples] = authority_ns;
            c.frame_total[samples] = t_end - t_frame;
            samples += 1;
        }
        if (now_ns() > start + (tick + 1) * period) {
            tick_overruns += 1;
        }
    }
    const uint64_t finished = now_ns();
    const double duration_s = (double)(finished - (start + opt->warmup * period)) / 1e9;

    read_threads(&threads_end);
    read_loadavg(loadavg_end);

    uint64_t host_next = 0;
    (void)orrery_host_next_tick(host, &host_next);
    uint64_t auth_next = 0;
    if (auth.host != NULL) {
        (void)orrery_host_next_tick(auth.host, &auth_next);
    }
    /* Compressed per-correction columns for the summary line. */
    size_t sampled_corrections = 0;
    for (unsigned d = 1; d <= WINDOW; ++d) {
        sampled_corrections += c.rollback_count[d];
    }

    if (smoke) {
        printf("ticks=%" PRIu64 " host_next_tick=%" PRIu64 " authority_next_tick=%" PRIu64
               " events=%" PRIu64 " samples=%zu corrections=%" PRIu64 " rollbacks=%" PRIu64
               " snaps=%" PRIu64 " rollback_failed=%" PRIu64 " hashes_changed=%" PRIu64
               " replay_ticks=%" PRIu64 " events_reemitted=%" PRIu64 " input_dropped=%" PRIu64
               " step_failed=%" PRIu64 " snapshot_failed=%" PRIu64 " restore_failed=%" PRIu64
               " decode_failures=%" PRIu64 " threads_before=%u threads_after_create=%u "
               "threads_end=%u snapshot_bytes_max=%" PRIu64 " checksum=%" PRIx64 "\n",
               total, host_next, auth_next, events_total, samples, corrections,
               corrections_rollback, corrections_snap, rollback_failed, hashes_changed_total,
               replay_ticks_total, pred.events_reemitted_by_replay, pred.input_dropped,
               step_failed, pred.snapshot_failed, pred.restore_failed, decode_failures,
               threads_before.total,
               threads_after_create.total, threads_end.total, snapshot_bytes_max, checksum);
    } else {
        /* summarize sorts in place; the JSON writer below re-sorts, which is
         * idempotent. */
        summary ipc = summarize(c.ipc_added, samples);
        summary snap = summarize(c.snapshot, samples);
        summary rb9 = summarize(c.rollback_by_depth[WINDOW], c.rollback_count[WINDOW]);
        printf("inproc_added p50 %.1f us, p99 %.1f us, p99.9 %.1f us, max %.1f us over %zu samples "
               "(N=%u); snapshot p50 %.1f us p99 %.1f us; rollback depth 9 p50 %.1f us p99 %.1f us "
               "max %.1f us over %zu; corrections %" PRIu64 " (%" PRIu64 " rollback, %" PRIu64
               " snap); threads %u -> %u -> %u; overruns %" PRIu64 "\n",
               (double)ipc.p50_ns / 1000.0, (double)ipc.p99_ns / 1000.0,
               (double)ipc.p99_9_ns / 1000.0, (double)ipc.max_ns / 1000.0, samples, n,
               (double)snap.p50_ns / 1000.0, (double)snap.p99_ns / 1000.0,
               (double)rb9.p50_ns / 1000.0, (double)rb9.p99_ns / 1000.0,
               (double)rb9.max_ns / 1000.0, c.rollback_count[WINDOW], corrections,
               corrections_rollback, corrections_snap, threads_before.total,
               threads_after_create.total, threads_end.total, tick_overruns);
    }

    if (opt->report != NULL) {
        FILE *f = fopen(opt->report, "w");
        if (f == NULL) {
            fprintf(stderr, "cannot write %s\n", opt->report);
            return 1;
        }
        fprintf(f, "{\n");
        fprintf(f, "  \"schema\": \"orrery-ipc-harness/1\",\n");
        fprintf(f, "  \"role\": \"observer\",\n");
        fprintf(f, "  \"platform\": \"linux\",\n");
        fprintf(f, "  \"arch\": \"x86_64\",\n");
        fprintf(f, "  \"clock\": \"CLOCK_MONOTONIC\",\n");
        fprintf(f, "  \"transport\": \"%s\",\n",
                opt->with_ring ? "inproc-staticlib-direct+ring" : "inproc-staticlib-direct-no-ring");
        fprintf(f, "  \"measured_quantity\": \"inproc_added\",\n");
        /* Schema fields the renderer prints. There is no socket, so nothing
         * was set; and this is Linux, so no timer resolution was raised. */
        fprintf(f, "  \"tcp_nodelay\": false,\n");
        fprintf(f, "  \"time_begin_period\": false,\n");
        fprintf(f, "  \"entities\": %u,\n", n);
        fprintf(f, "  \"tick_hz\": %u,\n", TICK_HZ);
        fprintf(f, "  \"warmup_ticks\": %" PRIu64 ",\n", opt->warmup);
        fprintf(f, "  \"ticks\": %" PRIu64 ",\n", opt->ticks);
        fprintf(f, "  \"samples\": %zu,\n", samples);
        fprintf(f, "  \"duration_s\": %.9f,\n", duration_s);
        fprintf(f, "  \"loadavg_start\": [%.2f, %.2f, %.2f],\n", loadavg_start[0],
                loadavg_start[1], loadavg_start[2]);
        fprintf(f, "  \"loadavg_end\": [%.2f, %.2f, %.2f],\n", loadavg_end[0], loadavg_end[1],
                loadavg_end[2]);
        fprintf(f, "  \"phases_ns\": {\n");
        json_summary(f, "hop_in", c.hop_in, samples, 0);
        json_summary(f, "extract", c.extract, samples, 0);
        json_summary(f, "encode", c.encode, samples, 0);
        json_summary(f, "hop_out", c.hop_out, samples, 0);
        json_summary(f, "decode_out", c.decode_out, samples, 0);
        json_summary(f, "phase", c.phase, samples, 0);
        json_summary(f, "phase_after_decode", c.phase_after_decode, samples, 0);
        json_summary(f, "ipc_added", c.ipc_added, samples, 1);
        fprintf(f, "  },\n");
        fprintf(f, "  \"baselines_ns\": {\n");
        json_summary(f, "snapshot", c.snapshot, samples, 0);
        json_summary(f, "remote_inputs", c.remote_inputs, samples, 0);
        json_summary(f, "drains", c.drains, samples, 0);
        json_summary(f, "authority_step", c.authority_step, samples, 0);
        json_summary(f, "frame_total", c.frame_total, samples, 1);
        fprintf(f, "  },\n");
        fprintf(f, "  \"rollback_ns\": {\n");
        fprintf(f, "    \"window_ticks\": %u,\n", WINDOW);
        fprintf(f, "    \"correction_every_ticks\": %" PRIu64 ",\n", opt->correction_every);
        fprintf(f, "    \"corrections\": %" PRIu64 ",\n", corrections);
        fprintf(f, "    \"plan_rollback\": %" PRIu64 ",\n", corrections_rollback);
        fprintf(f, "    \"plan_snap\": %" PRIu64 ",\n", corrections_snap);
        fprintf(f, "    \"rollback_failed\": %" PRIu64 ",\n", rollback_failed);
        fprintf(f, "    \"replay_ticks_total\": %" PRIu64 ",\n", replay_ticks_total);
        fprintf(f, "    \"hashes_changed_total\": %" PRIu64 ",\n", hashes_changed_total);
        fprintf(f, "    \"events_reemitted_by_replay\": %" PRIu64 ",\n",
                pred.events_reemitted_by_replay);
        fprintf(f, "    \"snapshot_bytes_max\": %" PRIu64 ",\n", snapshot_bytes_max);
        json_summary(f, "restore", c.restore, sampled_corrections, 0);
        json_summary(f, "replay", c.replay, sampled_corrections, 0);
        json_summary(f, "residual_mm", c.residual_mm, sampled_corrections, 0);
        for (unsigned d = 1; d <= WINDOW; ++d) {
            char name[32];
            snprintf(name, sizeof name, "depth_%u", d);
            json_summary(f, name, c.rollback_by_depth[d], c.rollback_count[d], d == WINDOW);
        }
        fprintf(f, "  },\n");
        fprintf(f, "  \"drops\": {\n");
        fprintf(f, "    \"input_dropped\": %" PRIu64 ",\n", pred.input_dropped);
        fprintf(f, "    \"step_failed\": %" PRIu64 ",\n", step_failed);
        fprintf(f, "    \"snapshot_failed\": %" PRIu64 ",\n", pred.snapshot_failed);
        fprintf(f, "    \"restore_failed\": %" PRIu64 ",\n", pred.restore_failed);
        fprintf(f, "    \"replay_step_failed\": %" PRIu64 ",\n", pred.replay_step_failed);
        fprintf(f, "    \"authority_input_dropped\": %" PRIu64 ",\n", auth.input_dropped);
        fprintf(f, "    \"authority_step_failed\": %" PRIu64 ",\n", auth.step_failed);
        fprintf(f, "    \"decode_failures\": %" PRIu64 ",\n", decode_failures);
        fprintf(f, "    \"tick_overruns\": %" PRIu64 "\n", tick_overruns);
        fprintf(f, "  },\n");
        fprintf(f, "  \"coexistence\": {\n");
        fprintf(f, "    \"app_present\": false,\n");
        fprintf(f, "    \"ring_present\": %s,\n", opt->with_ring ? "true" : "false");
        fprintf(f, "    \"cores_online\": %ld,\n", sysconf(_SC_NPROCESSORS_ONLN));
        json_threads(f, "threads_before_create", &threads_before, 0);
        json_threads(f, "threads_after_host_create", &threads_after_create, 0);
        json_threads(f, "threads_at_end", &threads_end, 0);
        fprintf(f, "    \"hitches_over_16_7ms\": {\"host_path\": %" PRIu64 ", \"rollback\": %" PRIu64
                   ", \"frame\": %" PRIu64 "},\n",
                hitch_host_path, hitch_rollback, hitch_frame);
        fprintf(f, "    \"events_routed\": %" PRIu64 ",\n", events_total);
        fprintf(f, "    \"events_max_bytes_per_tick\": %" PRIu64 ",\n", events_max_bytes);
        fprintf(f, "    \"mirror_checksum\": \"%016" PRIx64 "\"\n", checksum);
        fprintf(f, "  },\n");
        fprintf(f, "  \"timeline\": {\n");
        fprintf(f, "    \"ticks_issued_by_c\": %" PRIu64 ",\n", total);
        fprintf(f, "    \"host_next_tick\": %" PRIu64 ",\n", host_next);
        fprintf(f, "    \"authority_next_tick\": %" PRIu64 ",\n", auth_next);
        fprintf(f, "    \"drift_ticks_host_minus_issued\": %" PRId64 "\n",
                (int64_t)host_next - (int64_t)total);
        fprintf(f, "  },\n");
        fprintf(f, "  \"notes\": [\n");
        const char *fixed_notes[] = {
            "ipc_added in this report is inproc_added, defined as #1043 defines it: (t_decode - "
            "t0) with no hop; hop_in is the submit call (command decode + queue), extract is "
            "orrery_host_step, encode is orrery_host_collect_states, hop_out is one clock read, "
            "decode_out is the C-side decode of every record. The ring snapshot and the "
            "rollback are this prong's added columns and are reported beside it, not inside it",
            "phase is the apply cost, not a tick wait: in-process the mirror is applied in the "
            "frame that produced it, so #920's wait-for-next-tick is zero by construction",
            "Linux staticlib plus C consumer on clang, informational: not the in-process Unreal "
            "number G10.2 turns on, and it settles neither G10.2 nor D52/D53 (Proposed)",
            "no Bevy App, schedule runner, task pool, lightyear, iroh or tokio is in this "
            "process: the prediction ring, correction intake, rollback and replay are the C "
            "consumer's (#1052), driven through orrery_host_snapshot/restore/step only",
            "authority_step is a stand-in authority stepped on this thread to manufacture "
            "correction bytes; in production the authority is a remote peer and this column "
            "does not exist on the game thread",
            "there is one clock: the host never reads one "
            "(crates/orrery_sim_host/src/lib.rs:6-9) and no second accumulator exists to drift "
            "against it; drift_ticks_host_minus_issued is the whole tick bridge",
        };
        size_t fixed_count = sizeof fixed_notes / sizeof fixed_notes[0];
        for (size_t i = 0; i < fixed_count; ++i) {
            fprintf(f, "    ");
            json_string(f, fixed_notes[i]);
            fprintf(f, "%s\n", (i + 1 < fixed_count || opt->note_count > 0) ? "," : "");
        }
        for (unsigned i = 0; i < opt->note_count; ++i) {
            fprintf(f, "    ");
            json_string(f, opt->notes[i]);
            fprintf(f, "%s\n", i + 1 < opt->note_count ? "," : "");
        }
        fprintf(f, "  ]\n");
        fprintf(f, "}\n");
        fclose(f);
    }

    int rc = 0;
    if (!check(orrery_host_destroy(host), "orrery_host_destroy")) {
        rc = 1;
    }
    if (auth.host != NULL && !check(orrery_host_destroy(auth.host), "orrery_host_destroy")) {
        rc = 1;
    }
    predictor_free(&pred);
    free(states);
    free(crafts);
    free(actors);
    free(events);
    free(commands);
    free(peers);
    free(c.hop_in);
    free(c.extract);
    free(c.encode);
    free(c.hop_out);
    free(c.decode_out);
    free(c.phase);
    free(c.phase_after_decode);
    free(c.ipc_added);
    free(c.snapshot);
    free(c.remote_inputs);
    free(c.drains);
    free(c.authority_step);
    free(c.frame_total);
    free(c.restore);
    free(c.replay);
    free(c.residual_mm);
    for (unsigned d = 0; d <= WINDOW; ++d) {
        free(c.rollback_by_depth[d]);
    }
    return rc;
}

/* ---- the rollback proof ---------------------------------------------------
 *
 * The falsifier #1052 names: "the prediction/rollback path cannot be driven
 * at all through the existing ABI". Three corrections at depth 9 on a 24-craft
 * population that has been fighting for 60 ticks (damage routed, cooldowns
 * running):
 *   identity   the predictor's own bytes for entity 1 at T: every replayed
 *              tick must reproduce its original state hash, for every entity
 *   divergent  the authority's bytes at T: some hash must change
 *   repeat     the same divergent bytes again: no hash may change — the
 *              corrected timeline is what the ring now holds
 */
static int run_rollback_proof(void) {
    static const uint8_t seed[32] = {0x52, 0x10};
    const uint32_t n = 24;
    const uint64_t ticks = 60;
    orrery_host *host = create_host(seed, n);
    if (host == NULL) {
        return 1;
    }
    predictor pred;
    predictor_init(&pred, host, n);
    authority auth;
    memset(&auth, 0, sizeof auth);
    auth.host = create_host(seed, n);
    if (auth.host == NULL) {
        return 1;
    }
    /* The predictor's own local-craft bytes per boundary, for the identity
     * correction. */
    uint8_t own_state[RING][ORRERY_SKIRMISH_CRAFT_BYTES];
    uint64_t own_state_tick[RING];
    memset(own_state_tick, 0xff, sizeof own_state_tick);

    uint8_t commands[1 << 12];
    uint64_t peers[MAX_ENTITIES];
    uint8_t events[1 << 16];
    uint64_t host_tick = 0;
    for (uint64_t tick = 0; tick < ticks; ++tick) {
        (void)ring_snapshot(&pred, host_tick);
        for (uint32_t slot = 1; slot < n; ++slot) {
            uint64_t entity = (uint64_t)slot + 1;
            size_t peer_count = 0;
            for (uint32_t other = 0; other < n; ++other) {
                if (other != slot) {
                    peers[peer_count++] = (uint64_t)other + 1;
                }
            }
            size_t required = 0;
            if (orrery_skirmish_honest_commands(seed, entity, slot, host_tick, peers, peer_count,
                                                commands, sizeof commands,
                                                &required) != ORRERY_HOST_OK) {
                continue;
            }
            size_t at = 0;
            while (at + 4 <= required) {
                uint32_t len = read_u32(commands + at);
                at += 4;
                (void)submit_logged(&pred, host_tick, commands + at, len);
                (void)orrery_host_submit_command(auth.host, commands + at, len);
                at += len;
            }
        }
        uint8_t thrust[21];
        size_t thrust_len = encode_thrust(thrust, 1, 3000, 5000, (tick % 7 == 0) ? 200 : -200);
        (void)submit_logged(&pred, host_tick, thrust, thrust_len);
        uint64_t first = 0, next = 0;
        if (!check(orrery_host_step(host, 1, &first, &next), "orrery_host_step")) {
            return 1;
        }
        record_hashes(&pred, host_tick);
        size_t eb = 0;
        (void)orrery_host_drain_events(host, events, sizeof events, &eb);
        host_tick = next;
        size_t required = 0;
        if (orrery_host_state(host, 1, own_state[host_tick % RING], ORRERY_SKIRMISH_CRAFT_BYTES,
                              &required) == ORRERY_HOST_OK) {
            own_state_tick[host_tick % RING] = host_tick;
        }

        if (auth.pending_local_len > 0) {
            (void)orrery_host_submit_command(auth.host, auth.pending_local, auth.pending_local_len);
        }
        memcpy(auth.pending_local, thrust, thrust_len);
        auth.pending_local_len = thrust_len;
        uint64_t af = 0, an = 0;
        if (!check(orrery_host_step(auth.host, 1, &af, &an), "orrery_host_step(authority)")) {
            return 1;
        }
        size_t hc = 0;
        (void)orrery_host_drain_state_hashes(auth.host, pred.hash_records, MAX_ENTITIES, &hc);
        (void)orrery_host_drain_events(auth.host, events, sizeof events, &eb);
        if (orrery_host_state(auth.host, 1, auth.local_state[an % RING], ORRERY_SKIRMISH_CRAFT_BYTES,
                              &required) == ORRERY_HOST_OK) {
            auth.local_state_tick[an % RING] = an;
        }
    }

    const uint64_t at_tick = host_tick - WINDOW;
    if (own_state_tick[at_tick % RING] != at_tick || auth.local_state_tick[at_tick % RING] != at_tick) {
        fprintf(stderr, "no state at tick %" PRIu64 "\n", at_tick);
        return 1;
    }
    rollback_report identity = apply_correction(&pred, 1, at_tick, own_state[at_tick % RING],
                                                ORRERY_SKIRMISH_CRAFT_BYTES, host_tick);
    rollback_report divergent = apply_correction(&pred, 1, at_tick, auth.local_state[at_tick % RING],
                                                 ORRERY_SKIRMISH_CRAFT_BYTES, host_tick);
    rollback_report repeat = apply_correction(&pred, 1, at_tick, auth.local_state[at_tick % RING],
                                              ORRERY_SKIRMISH_CRAFT_BYTES, host_tick);
    uint64_t host_next = 0;
    (void)orrery_host_next_tick(host, &host_next);
    printf("rollback depth=%" PRIu64 " host_next_tick=%" PRIu64
           " identity_ok=%d identity_hashes_changed=%u identity_residual_mm=%" PRId64
           " divergent_ok=%d divergent_hashes_changed=%u divergent_residual_mm=%" PRId64
           " repeat_ok=%d repeat_hashes_changed=%u repeat_residual_mm=%" PRId64
           " restore_failed=%" PRIu64 " replay_step_failed=%" PRIu64
           " events_reemitted=%" PRIu64 " identity_ns=%" PRIu64 " divergent_ns=%" PRIu64 "\n",
           identity.depth, host_next, identity.ok, identity.hashes_changed, identity.residual_mm,
           divergent.ok, divergent.hashes_changed, divergent.residual_mm, repeat.ok,
           repeat.hashes_changed, repeat.residual_mm, pred.restore_failed,
           pred.replay_step_failed, pred.events_reemitted_by_replay, identity.total_ns,
           divergent.total_ns);
    int rc = 0;
    if (!check(orrery_host_destroy(host), "orrery_host_destroy") ||
        !check(orrery_host_destroy(auth.host), "orrery_host_destroy(authority)")) {
        rc = 1;
    }
    predictor_free(&pred);
    return rc;
}

/* ---- main ---------------------------------------------------------------- */

static int parse_options(int argc, char **argv, options *opt) {
    for (int i = 2; i < argc; ++i) {
        const char *arg = argv[i];
        const char *value = (i + 1 < argc) ? argv[i + 1] : NULL;
        if (strcmp(arg, "--no-ring") == 0) {
            opt->with_ring = 0;
            continue;
        }
        if (value == NULL) {
            fprintf(stderr, "%s needs a value\n", arg);
            return 0;
        }
        if (strcmp(arg, "--entities") == 0) {
            opt->entities = (uint32_t)strtoul(value, NULL, 10);
        } else if (strcmp(arg, "--ticks") == 0) {
            opt->ticks = strtoull(value, NULL, 10);
        } else if (strcmp(arg, "--warmup") == 0) {
            opt->warmup = strtoull(value, NULL, 10);
        } else if (strcmp(arg, "--correction-every") == 0) {
            opt->correction_every = strtoull(value, NULL, 10);
        } else if (strcmp(arg, "--report") == 0) {
            opt->report = value;
        } else if (strcmp(arg, "--note") == 0) {
            if (opt->note_count < MAX_NOTES) {
                opt->notes[opt->note_count++] = value;
            }
        } else {
            fprintf(stderr, "unknown option %s\n", arg);
            return 0;
        }
        i += 1;
    }
    if (opt->entities < 2 || opt->entities > MAX_ENTITIES) {
        fprintf(stderr, "--entities must be in [2, %u]\n", MAX_ENTITIES);
        return 0;
    }
    return 1;
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: direct_consumer bench|smoke|rollback [options]\n");
        return 2;
    }
    options opt;
    memset(&opt, 0, sizeof opt);
    opt.entities = 24;
    opt.ticks = 36000;
    opt.warmup = 600;
    opt.correction_every = 12;
    opt.with_ring = 1;

    if (strcmp(argv[1], "bench") == 0) {
        return parse_options(argc, argv, &opt) ? run_bench(&opt, 0) : 2;
    }
    if (strcmp(argv[1], "smoke") == 0) {
        opt.ticks = 120;
        opt.warmup = 0;
        return parse_options(argc, argv, &opt) ? run_bench(&opt, 1) : 2;
    }
    if (strcmp(argv[1], "rollback") == 0) {
        return run_rollback_proof();
    }
    fprintf(stderr, "unknown mode %s\n", argv[1]);
    return 2;
}
