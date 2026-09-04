/*
 * Spike #1045 — what the C consumer and the Unreal plugin share, so the two
 * processes drive the same rules with the same inputs through the same entry
 * points and a hash chain from one can be checked against the other.
 *
 *   1. The C mirror of the Body / Intent codecs (rules.rs).
 *   2. The scripted scenes (#1045 "What to build" items 3 and 4): where every
 *      command comes from, and where every frame change is, by tick.
 *   3. Spike #1052's rollback driver (crates/orrery_unreal_direct/examples/c/
 *      direct_consumer.c:397-669), lifted with its shape intact: the D8 ring
 *      of host snapshots plus the input log, restore, install, replay with
 *      ring rewrite, hash-for-hash comparison — generalised only in that a
 *      correction may carry several entities' bytes.
 *   4. A stand-in authority: a second host on the same script, so a
 *      correction has authoritative bytes and authoritative hashes to be
 *      checked against.
 *
 * Header-only, in the C subset C++20 also compiles (explicit casts, no
 * designated initialisers, no VLAs): the Unreal module is C++.
 */

#ifndef INTERIORS_SHARED_H
#define INTERIORS_SHARED_H

#include "orrery_unreal_interiors.h"

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ---- constants ------------------------------------------------------------ */

#define INTERIORS_TICK_HZ 60u
/* D8/D16's rollback window: PredictConfig::rollback_ticks = 9
 * (crates/orrery_predict/src/config.rs:62). */
#define INTERIORS_WINDOW 9u
#define INTERIORS_RING (INTERIORS_WINDOW + 1u)
#define INTERIORS_MAX_ENTITIES 8u
#define INTERIORS_SCENE_ENTITIES 4u
/* The stand-in authority keeps this many ticks of state and hashes. */
#define INTERIORS_AUTHORITY_RING 64u
#define INTERIORS_TAU_URAD 6283185
#define INTERIORS_QUARTER_URAD 1570796
#define INTERIORS_HALF_URAD 3141593

/* Speeds chosen so a tick's displacement is an exact lattice step: 2400 mm/s
 * is 40 mm per tick; 1200 mm/s is 20. (2 m/s would be 33.33 mm and the
 * tick-boundary snap would lose the third.) */
#define INTERIORS_WALK_MMPS 2400
#define INTERIORS_MECH_MMPS 1200
#define INTERIORS_SHIP_STRAIGHT_MMPS 50000
#define INTERIORS_SHIP_FAST_MMPS 500000
/* D5's worked number: 500 m/s "with a slow roll". 1454 urad per tick is
 * 87,240 urad/s, 4.998 deg/s, a full roll every 72 s. */
#define INTERIORS_SHIP_ROLL_URAD_TICK 1454

/* ---- byte helpers ---------------------------------------------------------- */

static uint64_t interiors_read_u64(const uint8_t *at) {
    uint64_t value = 0;
    int i;
    for (i = 7; i >= 0; --i) {
        value = (value << 8) | at[i];
    }
    return value;
}

static uint32_t interiors_read_u32(const uint8_t *at) {
    uint32_t value = 0;
    int i;
    for (i = 3; i >= 0; --i) {
        value = (value << 8) | at[i];
    }
    return value;
}

static void interiors_write_u64(uint8_t *at, uint64_t value) {
    int i;
    for (i = 0; i < 8; ++i) {
        at[i] = (uint8_t)(value >> (8 * i));
    }
}

static void interiors_write_u32(uint8_t *at, uint32_t value) {
    int i;
    for (i = 0; i < 4; ++i) {
        at[i] = (uint8_t)(value >> (8 * i));
    }
}

/* ---- 1. the codec mirror (rules.rs) ---------------------------------------- */

typedef enum interiors_kind {
    INTERIORS_KIND_STATION = 0,
    INTERIORS_KIND_SHIP = 1,
    INTERIORS_KIND_MECH = 2,
    INTERIORS_KIND_AVATAR = 3
} interiors_kind;

typedef struct interiors_body {
    uint8_t kind;
    uint64_t frame;
    int64_t pos[3]; /* millimetres in the frame's lattice */
    int64_t vel[3]; /* mm/s */
    int32_t yaw_urad;
    int32_t roll_urad;
    int32_t yaw_rate_urad_tick;
    int32_t roll_rate_urad_tick;
    uint32_t frame_changes;
} interiors_body;

static int interiors_decode_body(const uint8_t *bytes, size_t len, interiors_body *out) {
    int axis;
    if (len != ORRERY_INTERIORS_BODY_BYTES) {
        return 0;
    }
    out->kind = bytes[0];
    out->frame = interiors_read_u64(bytes + 1);
    for (axis = 0; axis < 3; ++axis) {
        out->pos[axis] = (int64_t)interiors_read_u64(bytes + 9 + 8 * axis);
        out->vel[axis] = (int64_t)interiors_read_u64(bytes + 33 + 8 * axis);
    }
    out->yaw_urad = (int32_t)interiors_read_u32(bytes + 57);
    out->roll_urad = (int32_t)interiors_read_u32(bytes + 61);
    out->yaw_rate_urad_tick = (int32_t)interiors_read_u32(bytes + 65);
    out->roll_rate_urad_tick = (int32_t)interiors_read_u32(bytes + 69);
    out->frame_changes = interiors_read_u32(bytes + 73);
    return 1;
}

/* Flat commands, [target u64][Intent canonical bytes], as
 * orrery_host_submit_command takes them. */
typedef struct interiors_cmd {
    uint8_t bytes[48];
    size_t len;
} interiors_cmd;

static void interiors_cmd_move(interiors_cmd *c, uint64_t target, int64_t vx, int64_t vy,
                               int64_t vz, int32_t yaw_urad) {
    interiors_write_u64(c->bytes, target);
    c->bytes[8] = 0;
    interiors_write_u64(c->bytes + 9, (uint64_t)vx);
    interiors_write_u64(c->bytes + 17, (uint64_t)vy);
    interiors_write_u64(c->bytes + 25, (uint64_t)vz);
    interiors_write_u32(c->bytes + 33, (uint32_t)yaw_urad);
    c->len = 37;
}

static void interiors_cmd_enter(interiors_cmd *c, uint64_t target, uint64_t frame) {
    interiors_write_u64(c->bytes, target);
    c->bytes[8] = 1;
    interiors_write_u64(c->bytes + 9, frame);
    c->len = 17;
}

static void interiors_cmd_leave(interiors_cmd *c, uint64_t target) {
    interiors_write_u64(c->bytes, target);
    c->bytes[8] = 2;
    c->len = 9;
}

static void interiors_cmd_cruise(interiors_cmd *c, uint64_t target, int64_t vx, int64_t vy,
                                 int64_t vz, int32_t yaw_rate, int32_t roll_rate) {
    interiors_write_u64(c->bytes, target);
    c->bytes[8] = 3;
    interiors_write_u64(c->bytes + 9, (uint64_t)vx);
    interiors_write_u64(c->bytes + 17, (uint64_t)vy);
    interiors_write_u64(c->bytes + 25, (uint64_t)vz);
    interiors_write_u32(c->bytes + 33, (uint32_t)yaw_rate);
    interiors_write_u32(c->bytes + 37, (uint32_t)roll_rate);
    c->len = 41;
}

/* ---- 2. the scripted scenes ------------------------------------------------ */

typedef enum interiors_scene {
    /* #1045 item 3: ship at rest (docked, then undocked and still). */
    INTERIORS_SCENE_REST = 0,
    /* ship at 50 m/s straight */
    INTERIORS_SCENE_STRAIGHT = 1,
    /* ship at 500 m/s with a slow roll — the headline */
    INTERIORS_SCENE_ROLL = 2,
    /* 500 m/s + roll, and the avatar mounts a walking, turning mech: the
     * second nesting level (avatar in mech in ship) */
    INTERIORS_SCENE_MECH = 3,
    /* #1045 item 4: board / undock / EVA / board under way / dock /
     * disembark, cycled; 600 ticks per cycle */
    INTERIORS_SCENE_TRANSITIONS = 4
} interiors_scene;

static const char *interiors_scene_name(interiors_scene s) {
    switch (s) {
    case INTERIORS_SCENE_REST:
        return "rest";
    case INTERIORS_SCENE_STRAIGHT:
        return "straight";
    case INTERIORS_SCENE_ROLL:
        return "roll";
    case INTERIORS_SCENE_MECH:
        return "mech";
    case INTERIORS_SCENE_TRANSITIONS:
        return "transitions";
    }
    return "?";
}

static int interiors_scene_parse(const char *name, interiors_scene *out) {
    interiors_scene s;
    for (s = INTERIORS_SCENE_REST; s <= INTERIORS_SCENE_TRANSITIONS;
         s = (interiors_scene)(s + 1)) {
        if (strcmp(interiors_scene_name(s), name) == 0) {
            *out = s;
            return 1;
        }
    }
    return 0;
}

#define INTERIORS_CYCLE_TICKS 600u
#define INTERIORS_BOARD_TICK 250u
#define INTERIORS_UNDOCK_TICK 300u
#define INTERIORS_MOUNT_TICK 1550u

typedef enum interiors_transition_kind {
    INTERIORS_TR_BOARD_DOCKED = 0, /* station -> docked ship: teleport-class */
    INTERIORS_TR_UNDOCK = 1,       /* ship: station -> universe (at rest) */
    INTERIORS_TR_EVA = 2,          /* avatar: ship under way -> universe, velocity kept */
    INTERIORS_TR_BOARD_UNDERWAY = 3, /* avatar: universe -> ship under way, continuous */
    INTERIORS_TR_DOCK = 4,         /* ship: universe -> station */
    INTERIORS_TR_DISEMBARK = 5,    /* avatar: docked ship -> station */
    INTERIORS_TR_MOUNT = 6,        /* avatar: ship -> mech (second level) */
    INTERIORS_TR_DISMOUNT = 7,     /* avatar: mech -> ship */
    INTERIORS_TR_KINDS = 8
} interiors_transition_kind;

static const char *interiors_transition_name(interiors_transition_kind k) {
    static const char *names[INTERIORS_TR_KINDS] = {
        "board_docked", "undock", "eva", "board_underway", "dock", "disembark", "mount",
        "dismount"};
    return k < INTERIORS_TR_KINDS ? names[k] : "?";
}

typedef struct interiors_transition {
    uint64_t tick;
    interiors_transition_kind kind;
    uint64_t entity;
} interiors_transition;

typedef struct interiors_cmds {
    interiors_cmd cmd[4];
    unsigned count;
} interiors_cmds;

/* A rectangular loop walked leg by leg. Returns the direction for `t` ticks
 * into the loop, as unit axis (dx, dy) and heading. */
static void interiors_loop_dir(uint64_t t, uint64_t long_leg, uint64_t short_leg, int *dx,
                               int *dy, int32_t *yaw) {
    uint64_t period = 2 * (long_leg + short_leg);
    uint64_t u = t % period;
    if (u < long_leg) {
        *dx = 0;
        *dy = 1;
        *yaw = INTERIORS_QUARTER_URAD;
    } else if (u < long_leg + short_leg) {
        *dx = 1;
        *dy = 0;
        *yaw = 0;
    } else if (u < 2 * long_leg + short_leg) {
        *dx = 0;
        *dy = -1;
        *yaw = 3 * INTERIORS_QUARTER_URAD;
    } else {
        *dx = -1;
        *dy = 0;
        *yaw = INTERIORS_HALF_URAD;
    }
}

static void interiors_push_move(interiors_cmds *out, uint64_t target, int64_t speed, int dx,
                                int dy, int32_t yaw) {
    if (out->count < 4) {
        interiors_cmd_move(&out->cmd[out->count++], target, speed * dx, speed * dy, 0, yaw);
    }
}

/* The commands the script issues at `tick`, for a scene of `total` ticks.
 * The avatar is the local (predicted) entity and gets one Move every tick,
 * like #920's and #1052's one local input per tick; the ship and mech are
 * commanded only when something changes. */
static void interiors_script_at(interiors_scene scene, uint64_t tick, uint64_t total,
                                interiors_cmds *out) {
    const uint64_t AV = ORRERY_INTERIORS_AVATAR, SHIP = ORRERY_INTERIORS_SHIP,
                   MECH = ORRERY_INTERIORS_MECH, STATION = ORRERY_INTERIORS_STATION;
    out->count = 0;

    if (scene == INTERIORS_SCENE_TRANSITIONS) {
        uint64_t c = tick % INTERIORS_CYCLE_TICKS;
        /* 0..249: walk +y up the bay (station frame). 250: board. 251..:
         * walk +y inside. 300: undock; 301: cruise 50 m/s. 400: EVA.
         * 401..459: drift alongside (universe frame, ship velocity kept by
         * the migration; the Move re-states it). 460: board under way.
         * 500: cruise 0. 501: dock. 520: disembark. 521..599: walk -y. */
        if (c == INTERIORS_BOARD_TICK) {
            interiors_cmd_enter(&out->cmd[out->count++], AV, SHIP);
        } else if (c == 400) {
            interiors_cmd_leave(&out->cmd[out->count++], AV);
        } else if (c == 460) {
            interiors_cmd_enter(&out->cmd[out->count++], AV, SHIP);
        } else if (c == 520) {
            interiors_cmd_leave(&out->cmd[out->count++], AV);
        }
        if (c == INTERIORS_UNDOCK_TICK) {
            interiors_cmd_leave(&out->cmd[out->count++], SHIP);
        } else if (c == INTERIORS_UNDOCK_TICK + 1) {
            interiors_cmd_cruise(&out->cmd[out->count++], SHIP, INTERIORS_SHIP_STRAIGHT_MMPS, 0,
                                 0, 0, 0);
        } else if (c == 500) {
            interiors_cmd_cruise(&out->cmd[out->count++], SHIP, 0, 0, 0, 0, 0);
        } else if (c == 501) {
            interiors_cmd_enter(&out->cmd[out->count++], SHIP, STATION);
        }
        if (c > 400 && c < 460) {
            /* In the universe frame: keep pace with the ship, keep walking. */
            interiors_cmd_move(&out->cmd[out->count++], AV, INTERIORS_SHIP_STRAIGHT_MMPS,
                               INTERIORS_WALK_MMPS, 0, INTERIORS_QUARTER_URAD);
        } else if (c > 520) {
            interiors_push_move(out, AV, INTERIORS_WALK_MMPS, 0, -1, 3 * INTERIORS_QUARTER_URAD);
        } else if (c != 400 && c != 460 && c != 520 && c != INTERIORS_BOARD_TICK) {
            interiors_push_move(out, AV, INTERIORS_WALK_MMPS, 0, 1, INTERIORS_QUARTER_URAD);
        }
        return;
    }

    /* Scenes 0-3 share a prologue: 0..249 walk +y up the bay; 250 board
     * (teleport-class; the ship is docked and both frames are at rest);
     * 300 undock; 301 cruise per scene. From 250 the avatar walks the
     * corridor loop, 20 m x 3 m at 2.4 m/s, in ship-local coordinates. */
    if (tick == INTERIORS_BOARD_TICK) {
        interiors_cmd_enter(&out->cmd[out->count++], AV, SHIP);
    }
    if (tick == INTERIORS_UNDOCK_TICK) {
        interiors_cmd_leave(&out->cmd[out->count++], SHIP);
    } else if (tick == INTERIORS_UNDOCK_TICK + 1) {
        switch (scene) {
        case INTERIORS_SCENE_STRAIGHT:
            interiors_cmd_cruise(&out->cmd[out->count++], SHIP, INTERIORS_SHIP_STRAIGHT_MMPS, 0,
                                 0, 0, 0);
            break;
        case INTERIORS_SCENE_ROLL:
        case INTERIORS_SCENE_MECH:
            interiors_cmd_cruise(&out->cmd[out->count++], SHIP, INTERIORS_SHIP_FAST_MMPS, 0, 0,
                                 0, INTERIORS_SHIP_ROLL_URAD_TICK);
            break;
        default:
            break;
        }
    }

    if (scene == INTERIORS_SCENE_MECH && tick >= 1400) {
        /* 1400: the avatar is back at ship-local (0,0) (one loop period of
         * 1150 ticks after 250). Walk +x 6 m to the mech (150 ticks), mount
         * at 1550, walk a 1 m square inside the cockpit; the mech itself
         * walks a 10 m x 2 m loop, turning at each corner. Dismount 600
         * ticks before the end. */
        uint64_t dismount = total > 700 ? total - 600 : total;
        if (tick < INTERIORS_MOUNT_TICK) {
            interiors_push_move(out, AV, INTERIORS_WALK_MMPS, 1, 0, 0);
            return;
        }
        if (tick == INTERIORS_MOUNT_TICK) {
            interiors_cmd_enter(&out->cmd[out->count++], AV, MECH);
            return;
        }
        if (tick == dismount) {
            interiors_cmd_leave(&out->cmd[out->count++], AV);
            return;
        }
        if (tick > dismount) {
            interiors_push_move(out, AV, INTERIORS_WALK_MMPS, 0, 1, INTERIORS_QUARTER_URAD);
            return;
        }
        {
            int dx, dy;
            int32_t yaw;
            interiors_loop_dir(tick - INTERIORS_MOUNT_TICK, 50, 50, &dx, &dy, &yaw);
            interiors_push_move(out, AV, INTERIORS_MECH_MMPS, dx, dy, yaw);
            /* The mech: command at leg boundaries only. */
            {
                uint64_t m = tick - (INTERIORS_MOUNT_TICK + 1);
                uint64_t period = 2 * (500 + 100);
                uint64_t u = m % period;
                if (u == 0 || u == 500 || u == 600 || u == 1100) {
                    interiors_loop_dir(m, 500, 100, &dx, &dy, &yaw);
                    interiors_push_move(out, MECH, INTERIORS_MECH_MMPS, dx, dy, yaw);
                }
            }
        }
        return;
    }

    if (tick < INTERIORS_BOARD_TICK) {
        interiors_push_move(out, AV, INTERIORS_WALK_MMPS, 0, 1, INTERIORS_QUARTER_URAD);
    } else {
        int dx, dy;
        int32_t yaw;
        interiors_loop_dir(tick - INTERIORS_BOARD_TICK, 500, 75, &dx, &dy, &yaw);
        interiors_push_move(out, AV, INTERIORS_WALK_MMPS, dx, dy, yaw);
    }
}

/* Every frame change the script issues, by tick, in order. */
static unsigned interiors_script_transitions(interiors_scene scene, uint64_t total,
                                             interiors_transition *out, unsigned cap) {
    unsigned n = 0;
#define INTERIORS_PUSH_TR(T, K, E)                                                            \
    do {                                                                                     \
        if (n < cap && (T) < total) {                                                        \
            out[n].tick = (T);                                                               \
            out[n].kind = (K);                                                               \
            out[n].entity = (E);                                                             \
            n += 1;                                                                          \
        }                                                                                    \
    } while (0)
    if (scene == INTERIORS_SCENE_TRANSITIONS) {
        uint64_t base;
        for (base = 0; base < total; base += INTERIORS_CYCLE_TICKS) {
            INTERIORS_PUSH_TR(base + INTERIORS_BOARD_TICK, INTERIORS_TR_BOARD_DOCKED,
                              ORRERY_INTERIORS_AVATAR);
            INTERIORS_PUSH_TR(base + INTERIORS_UNDOCK_TICK, INTERIORS_TR_UNDOCK,
                              ORRERY_INTERIORS_SHIP);
            INTERIORS_PUSH_TR(base + 400, INTERIORS_TR_EVA, ORRERY_INTERIORS_AVATAR);
            INTERIORS_PUSH_TR(base + 460, INTERIORS_TR_BOARD_UNDERWAY, ORRERY_INTERIORS_AVATAR);
            INTERIORS_PUSH_TR(base + 501, INTERIORS_TR_DOCK, ORRERY_INTERIORS_SHIP);
            INTERIORS_PUSH_TR(base + 520, INTERIORS_TR_DISEMBARK, ORRERY_INTERIORS_AVATAR);
        }
        return n;
    }
    INTERIORS_PUSH_TR(INTERIORS_BOARD_TICK, INTERIORS_TR_BOARD_DOCKED, ORRERY_INTERIORS_AVATAR);
    INTERIORS_PUSH_TR(INTERIORS_UNDOCK_TICK, INTERIORS_TR_UNDOCK, ORRERY_INTERIORS_SHIP);
    if (scene == INTERIORS_SCENE_MECH) {
        INTERIORS_PUSH_TR(INTERIORS_MOUNT_TICK, INTERIORS_TR_MOUNT, ORRERY_INTERIORS_AVATAR);
        if (total > 700) {
            INTERIORS_PUSH_TR(total - 600, INTERIORS_TR_DISMOUNT, ORRERY_INTERIORS_AVATAR);
        }
    }
#undef INTERIORS_PUSH_TR
    return n;
}

/* ---- growable byte buffers (direct_consumer.c) ------------------------------ */

typedef struct interiors_bytes {
    uint8_t *data;
    size_t len, cap;
} interiors_bytes;

static void interiors_bytes_reserve(interiors_bytes *b, size_t cap) {
    size_t grown;
    uint8_t *data;
    if (b->cap >= cap) {
        return;
    }
    grown = b->cap ? b->cap : 4096;
    while (grown < cap) {
        grown *= 2;
    }
    data = (uint8_t *)realloc(b->data, grown);
    if (data == NULL) {
        fprintf(stderr, "out of memory\n");
        exit(2);
    }
    b->data = data;
    b->cap = grown;
}

static void interiors_bytes_append(interiors_bytes *b, const uint8_t *src, size_t len) {
    interiors_bytes_reserve(b, b->len + len);
    memcpy(b->data + b->len, src, len);
    b->len += len;
}

/* Call a (out, capacity, out_required) ABI function into a growable buffer,
 * retrying once on BUFFER_TOO_SMALL. */
#define INTERIORS_CALL_INTO(b, expr)                                                       \
    do {                                                                                   \
        size_t required_ = 0;                                                              \
        orrery_host_result r_ = (expr);                                                    \
        if (r_ == ORRERY_HOST_BUFFER_TOO_SMALL) {                                          \
            interiors_bytes_reserve((b), required_);                                       \
            r_ = (expr);                                                                   \
        }                                                                                  \
        (b)->len = (r_ == ORRERY_HOST_OK) ? required_ : 0;                                 \
        result = r_;                                                                       \
    } while (0)

/* ---- the host over the scene ------------------------------------------------ */

static orrery_host *interiors_create_host(const uint8_t seed[32]) {
    orrery_host *host = NULL;
    orrery_host_ruleset_identity id;
    uint64_t entity, n;
    if (orrery_host_abi_version() != ORRERY_SIM_HOST_ABI_VERSION) {
        fprintf(stderr, "host abi version mismatch\n");
        return NULL;
    }
    if (orrery_interiors_host_create(seed, 0, &host) != ORRERY_HOST_OK) {
        fprintf(stderr, "orrery_interiors_host_create failed\n");
        return NULL;
    }
    if (orrery_host_ruleset_id(host, &id) != ORRERY_HOST_OK ||
        id.version != ORRERY_INTERIORS_RULESET_VERSION) {
        fprintf(stderr, "ruleset id mismatch: version %u\n", (unsigned)id.version);
        (void)orrery_host_destroy(host);
        return NULL;
    }
    n = orrery_interiors_scene_len();
    for (entity = 1; entity <= n; ++entity) {
        uint8_t state[ORRERY_INTERIORS_BODY_BYTES];
        size_t required = 0;
        if (orrery_interiors_scene_state(entity, state, sizeof state, &required) !=
                ORRERY_HOST_OK ||
            required != sizeof state ||
            orrery_host_install_state(host, entity, 0, state, required) != ORRERY_HOST_OK) {
            fprintf(stderr, "installing scene body %u failed\n", (unsigned)entity);
            (void)orrery_host_destroy(host);
            return NULL;
        }
    }
    return host;
}

/* Read one body out of the host. */
static int interiors_read_body(orrery_host *host, uint64_t entity, interiors_body *out) {
    uint8_t state[ORRERY_INTERIORS_BODY_BYTES];
    size_t required = 0;
    if (orrery_host_state(host, entity, state, sizeof state, &required) != ORRERY_HOST_OK) {
        return 0;
    }
    return interiors_decode_body(state, required, out);
}

/* ---- 3. the predictor: D8's ring and rollback (direct_consumer.c:397-669) --- */

/* One ring slot: the host as it stood at tick boundary T (next_tick == T),
 * before frame T's inputs were submitted, and every command frame T then
 * submitted, as [u32 len][command] records. Together they are exactly what
 * HostSnapshot::restore says it needs to reproduce the run
 * (crates/orrery_sim_host/src/lib.rs:711-720). */
typedef struct interiors_ring_slot {
    uint64_t tick;
    int valid;
    interiors_bytes snapshot;
    interiors_bytes inputs;
    /* The state hashes tick T produced, by entity index — kept so the
     * rollback proof can say "hash for hash" rather than "looks the same". */
    uint8_t hashes[INTERIORS_MAX_ENTITIES][32];
    uint64_t hash_entity[INTERIORS_MAX_ENTITIES];
    unsigned hash_count;
} interiors_ring_slot;

typedef struct interiors_predictor {
    orrery_host *host;
    interiors_ring_slot ring[INTERIORS_RING];
    interiors_bytes scratch;
    orrery_host_state_hash *hash_records;
    /* counters */
    uint64_t input_dropped, snapshot_failed, restore_failed, replay_step_failed;
    uint64_t events_reemitted_by_replay;
    /* A running digest over every state hash in execution order, so two
     * processes (C and Unreal) running the same script can be compared with
     * one number. FNV-1a 64; equality is all it is for. */
    uint64_t chain;
} interiors_predictor;

static uint64_t interiors_fnv(uint64_t h, const uint8_t *bytes, size_t len) {
    size_t i;
    for (i = 0; i < len; ++i) {
        h ^= bytes[i];
        h *= 1099511628211ull;
    }
    return h;
}

static interiors_ring_slot *interiors_slot_for(interiors_predictor *p, uint64_t tick) {
    return &p->ring[tick % INTERIORS_RING];
}

static void interiors_predictor_init(interiors_predictor *p, orrery_host *host) {
    unsigned i;
    memset(p, 0, sizeof *p);
    p->host = host;
    p->chain = 1469598103934665603ull;
    p->hash_records = (orrery_host_state_hash *)calloc(INTERIORS_MAX_ENTITIES * INTERIORS_RING,
                                                       sizeof *p->hash_records);
    if (p->hash_records == NULL) {
        fprintf(stderr, "out of memory\n");
        exit(2);
    }
    interiors_bytes_reserve(&p->scratch, 1 << 14);
    for (i = 0; i < INTERIORS_RING; ++i) {
        interiors_bytes_reserve(&p->ring[i].snapshot, 1 << 10);
        interiors_bytes_reserve(&p->ring[i].inputs, 1 << 10);
    }
}

static void interiors_predictor_free(interiors_predictor *p) {
    unsigned i;
    for (i = 0; i < INTERIORS_RING; ++i) {
        free(p->ring[i].snapshot.data);
        free(p->ring[i].inputs.data);
    }
    free(p->scratch.data);
    free(p->hash_records);
}

/* The ring write: the host at boundary `tick`, before any of frame `tick`'s
 * inputs. Clears the slot's input log. */
static int interiors_ring_snapshot(interiors_predictor *p, uint64_t tick) {
    interiors_ring_slot *slot = interiors_slot_for(p, tick);
    orrery_host_result result;
    interiors_bytes *b = &slot->snapshot;
    INTERIORS_CALL_INTO(b, orrery_host_snapshot(p->host, b->data, b->cap, &required_));
    if (result != ORRERY_HOST_OK) {
        p->snapshot_failed += 1;
        slot->valid = 0;
        return 0;
    }
    slot->tick = tick;
    slot->valid = 1;
    slot->inputs.len = 0;
    return 1;
}

/* Submit one command for the frame at `tick` and log it in the ring: the
 * input history lightyear keeps as redundant input, here kept only as deep
 * as the window because nothing retransmits it. */
static orrery_host_result interiors_submit_logged(interiors_predictor *p, uint64_t tick,
                                                  const uint8_t *cmd, size_t len) {
    orrery_host_result r = orrery_host_submit_command(p->host, cmd, len);
    uint8_t prefix[4];
    if (r != ORRERY_HOST_OK) {
        p->input_dropped += 1;
        return r;
    }
    interiors_write_u32(prefix, (uint32_t)len);
    interiors_bytes_append(&interiors_slot_for(p, tick)->inputs, prefix, 4);
    interiors_bytes_append(&interiors_slot_for(p, tick)->inputs, cmd, len);
    return r;
}

/* Drain the hashes the last step produced into the slot for `tick`, and
 * fold them into the chain. */
static void interiors_record_hashes(interiors_predictor *p, uint64_t tick, int fold) {
    interiors_ring_slot *slot = interiors_slot_for(p, tick);
    size_t count = 0, i;
    orrery_host_result r = orrery_host_drain_state_hashes(
        p->host, p->hash_records, INTERIORS_MAX_ENTITIES * INTERIORS_RING, &count);
    slot->hash_count = 0;
    if (r != ORRERY_HOST_OK) {
        return;
    }
    for (i = 0; i < count; ++i) {
        if (p->hash_records[i].tick != tick || slot->hash_count >= INTERIORS_MAX_ENTITIES) {
            continue;
        }
        memcpy(slot->hashes[slot->hash_count], p->hash_records[i].hash, 32);
        slot->hash_entity[slot->hash_count] = p->hash_records[i].entity;
        slot->hash_count += 1;
        if (fold) {
            p->chain = interiors_fnv(p->chain, p->hash_records[i].hash, 32);
        }
    }
}

/* Compare the hashes the replay of `tick` produced against what the slot
 * held before; returns the number that differ, and overwrites the slot with
 * the replay's hashes (the ring now describes the corrected timeline). */
static unsigned interiors_rehash_and_compare(interiors_predictor *p, uint64_t tick) {
    interiors_ring_slot *slot = interiors_slot_for(p, tick);
    uint8_t before[INTERIORS_MAX_ENTITIES][32];
    unsigned before_count = slot->hash_count;
    unsigned differ = 0, n, i;
    memcpy(before, slot->hashes, sizeof before);
    interiors_record_hashes(p, tick, 0);
    n = slot->hash_count < before_count ? slot->hash_count : before_count;
    for (i = 0; i < n; ++i) {
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

/* One tick of the predicted loop: ring write at the boundary, the script's
 * commands submitted and logged, one step, hashes recorded. Returns the tick
 * stepped, or UINT64_MAX on failure. */
static uint64_t interiors_predict_tick(interiors_predictor *p, interiors_scene scene,
                                       uint64_t total) {
    uint64_t tick = 0, first = 0, next = 0;
    interiors_cmds cmds;
    unsigned i;
    if (orrery_host_next_tick(p->host, &tick) != ORRERY_HOST_OK) {
        return UINT64_MAX;
    }
    (void)interiors_ring_snapshot(p, tick);
    interiors_script_at(scene, tick, total, &cmds);
    for (i = 0; i < cmds.count; ++i) {
        (void)interiors_submit_logged(p, tick, cmds.cmd[i].bytes, cmds.cmd[i].len);
    }
    if (orrery_host_step(p->host, 1, &first, &next) != ORRERY_HOST_OK || first != tick) {
        return UINT64_MAX;
    }
    interiors_record_hashes(p, tick, 1);
    return tick;
}

typedef enum interiors_plan {
    INTERIORS_PLAN_ROLLBACK = 0, /* AuthorityCorrectionPlan::Rollback (correction.rs:14-19) */
    INTERIORS_PLAN_SNAP = 1      /* AuthorityCorrectionPlan::Snap     (correction.rs:20-24) */
} interiors_plan;

/* One entity's authoritative bytes at the correction tick. */
typedef struct interiors_correction_entity {
    uint64_t entity;
    uint8_t bytes[ORRERY_INTERIORS_BODY_BYTES];
} interiors_correction_entity;

typedef struct interiors_rollback_report {
    interiors_plan plan;
    uint64_t depth;
    uint64_t restore_ns, install_ns, replay_ns, total_ns;
    unsigned hashes_changed; /* over the replayed ticks, against the abandoned timeline */
    int64_t residual_mm;     /* max-axis |pos_before - pos_after| of the avatar at now */
    uint64_t frame_before, frame_after;
    int ok;
} interiors_rollback_report;

/* The correction intake: the authority says these entities were `bytes` at
 * boundary `authoritative_tick`. authority_correction_plan
 * (crates/orrery_predict/src/correction.rs:71-85) followed by #1052's driver:
 * restore the ring slot, install the authoritative state at that tick,
 * replay every logged frame since, rewriting the ring as it goes, and read
 * the residual off the avatar. */
static interiors_rollback_report interiors_apply_correction(
    interiors_predictor *p, const interiors_correction_entity *entities, unsigned entity_count,
    uint64_t authoritative_tick, uint64_t now, uint64_t (*clock_ns)(void)) {
    interiors_rollback_report r;
    interiors_body before, after;
    interiors_ring_slot *slot;
    uint64_t t0, t1, t2, t3, t;
    unsigned i;
    int a;
    memset(&r, 0, sizeof r);
    memset(&before, 0, sizeof before);
    memset(&after, 0, sizeof after);
    r.depth = now - authoritative_tick;
    (void)interiors_read_body(p->host, ORRERY_INTERIORS_AVATAR, &before);
    t0 = clock_ns();

    slot = interiors_slot_for(p, authoritative_tick);
    if (r.depth > INTERIORS_WINDOW || !slot->valid || slot->tick != authoritative_tick) {
        r.plan = INTERIORS_PLAN_SNAP;
        r.ok = 1;
        for (i = 0; i < entity_count; ++i) {
            if (orrery_host_install_state(p->host, entities[i].entity, now, entities[i].bytes,
                                          ORRERY_INTERIORS_BODY_BYTES) != ORRERY_HOST_OK) {
                r.ok = 0;
            }
        }
        r.total_ns = clock_ns() - t0;
        goto residual;
    }

    r.plan = INTERIORS_PLAN_ROLLBACK;
    if (orrery_host_restore(p->host, slot->snapshot.data, slot->snapshot.len) != ORRERY_HOST_OK) {
        p->restore_failed += 1;
        r.total_ns = clock_ns() - t0;
        return r;
    }
    t1 = clock_ns();
    r.restore_ns = t1 - t0;
    for (i = 0; i < entity_count; ++i) {
        if (orrery_host_install_state(p->host, entities[i].entity, authoritative_tick,
                                      entities[i].bytes,
                                      ORRERY_INTERIORS_BODY_BYTES) != ORRERY_HOST_OK) {
            r.total_ns = clock_ns() - t0;
            return r;
        }
    }
    t2 = clock_ns();
    r.install_ns = t2 - t1;

    /* Replay: for each logged frame, re-snapshot the slot (the ring must
     * describe the corrected timeline, or the next correction restores the
     * abandoned one), resubmit its log, step once, re-hash. */
    r.ok = 1;
    for (t = authoritative_tick; t < now; ++t) {
        interiors_ring_slot *s = interiors_slot_for(p, t);
        interiors_bytes log = s->inputs;
        size_t at = 0;
        uint64_t first = 0, next = 0;
        memset(&s->inputs, 0, sizeof s->inputs);
        (void)interiors_ring_snapshot(p, t);
        s->inputs = log;
        while (at + 4 <= log.len) {
            uint32_t l = interiors_read_u32(log.data + at);
            at += 4;
            if (orrery_host_submit_command(p->host, log.data + at, l) != ORRERY_HOST_OK) {
                p->input_dropped += 1;
                r.ok = 0;
            }
            at += l;
        }
        if (orrery_host_step(p->host, 1, &first, &next) != ORRERY_HOST_OK || first != t) {
            p->replay_step_failed += 1;
            r.ok = 0;
            break;
        }
        r.hashes_changed += interiors_rehash_and_compare(p, t);
    }
    /* The replay re-emits every event the abandoned timeline emitted — here
     * that includes the FrameChanged a presentation layer already acted on
     * (it re-parented an actor). Counted: the de-duplication obligation is
     * the consumer's (#1052 README, "events_reemitted_by_replay"). */
    {
        orrery_host_result result;
        interiors_bytes *b = &p->scratch;
        INTERIORS_CALL_INTO(b, orrery_host_drain_events(p->host, b->data, b->cap, &required_));
        if (result == ORRERY_HOST_OK) {
            size_t at = 0;
            while (at + 12 <= b->len) {
                uint32_t l = interiors_read_u32(b->data + at + 8);
                at += 12 + l;
                p->events_reemitted_by_replay += 1;
            }
        }
    }
    t3 = clock_ns();
    r.replay_ns = t3 - t2;
    r.total_ns = t3 - t0;

residual:
    (void)interiors_read_body(p->host, ORRERY_INTERIORS_AVATAR, &after);
    r.frame_before = before.frame;
    r.frame_after = after.frame;
    /* A residual across frames is not one number; report it only when the
     * corrected avatar is in the frame the abandoned one was. */
    if (before.frame == after.frame) {
        for (a = 0; a < 3; ++a) {
            int64_t d = after.pos[a] - before.pos[a];
            if (d < 0) {
                d = -d;
            }
            if (d > r.residual_mm) {
                r.residual_mm = d;
            }
        }
    } else {
        r.residual_mm = -1;
    }
    return r;
}

/* ---- 4. the stand-in authority ---------------------------------------------- */

/* A second host on the same script. It exists only to manufacture
 * authoritative bytes and authoritative hashes; in production it is a remote
 * peer. Its state and hashes for every entity are kept for
 * INTERIORS_AUTHORITY_RING ticks so a correction "at tick T" has bytes to
 * carry and the corrected client has hashes to be checked against. */
typedef struct interiors_authority {
    orrery_host *host;
    /* by tick % ring: the boundary state (before the tick's inputs) and the
     * hashes the tick produced */
    uint64_t tick[INTERIORS_AUTHORITY_RING];
    uint8_t state[INTERIORS_AUTHORITY_RING][INTERIORS_SCENE_ENTITIES][ORRERY_INTERIORS_BODY_BYTES];
    uint8_t hashes[INTERIORS_AUTHORITY_RING][INTERIORS_MAX_ENTITIES][32];
    uint64_t hash_entity[INTERIORS_AUTHORITY_RING][INTERIORS_MAX_ENTITIES];
    unsigned hash_count[INTERIORS_AUTHORITY_RING];
    orrery_host_state_hash records[INTERIORS_MAX_ENTITIES * 4];
    /* the divergence: an extra command the client does not know about */
    interiors_cmd extra;
    uint64_t extra_tick;
    int has_extra;
    uint64_t step_failed;
} interiors_authority;

static void interiors_authority_init(interiors_authority *a, orrery_host *host) {
    memset(a, 0, sizeof *a);
    a->host = host;
}

/* One authority tick: capture the boundary state, submit the script (plus
 * the divergence if this is its tick), step, capture the hashes. */
static uint64_t interiors_authority_tick(interiors_authority *a, interiors_scene scene,
                                         uint64_t total) {
    uint64_t tick = 0, first = 0, next = 0, e;
    unsigned slot, i;
    size_t count = 0, k;
    interiors_cmds cmds;
    if (orrery_host_next_tick(a->host, &tick) != ORRERY_HOST_OK) {
        return UINT64_MAX;
    }
    slot = (unsigned)(tick % INTERIORS_AUTHORITY_RING);
    a->tick[slot] = tick;
    for (e = 1; e <= INTERIORS_SCENE_ENTITIES; ++e) {
        size_t required = 0;
        (void)orrery_host_state(a->host, e, a->state[slot][e - 1], ORRERY_INTERIORS_BODY_BYTES,
                                &required);
    }
    interiors_script_at(scene, tick, total, &cmds);
    for (i = 0; i < cmds.count; ++i) {
        (void)orrery_host_submit_command(a->host, cmds.cmd[i].bytes, cmds.cmd[i].len);
    }
    if (a->has_extra && a->extra_tick == tick) {
        (void)orrery_host_submit_command(a->host, a->extra.bytes, a->extra.len);
    }
    if (orrery_host_step(a->host, 1, &first, &next) != ORRERY_HOST_OK || first != tick) {
        a->step_failed += 1;
        return UINT64_MAX;
    }
    a->hash_count[slot] = 0;
    if (orrery_host_drain_state_hashes(a->host, a->records, sizeof a->records / sizeof a->records[0],
                                       &count) == ORRERY_HOST_OK) {
        for (k = 0; k < count; ++k) {
            if (a->records[k].tick != tick || a->hash_count[slot] >= INTERIORS_MAX_ENTITIES) {
                continue;
            }
            memcpy(a->hashes[slot][a->hash_count[slot]], a->records[k].hash, 32);
            a->hash_entity[slot][a->hash_count[slot]] = a->records[k].entity;
            a->hash_count[slot] += 1;
        }
    }
    {
        /* Events are not part of this; keep the drain buffer from growing. */
        uint8_t sink[1024];
        size_t required = 0;
        (void)orrery_host_drain_events(a->host, sink, sizeof sink, &required);
    }
    return tick;
}

/* Compare the client's ring hashes for `tick` against the authority's.
 * Returns the number of (entity, tick) hashes that differ, counting a
 * missing entity on either side as a difference. */
static unsigned interiors_compare_tick(const interiors_predictor *p, const interiors_authority *a,
                                       uint64_t tick) {
    const interiors_ring_slot *s = &p->ring[tick % INTERIORS_RING];
    unsigned slot = (unsigned)(tick % INTERIORS_AUTHORITY_RING);
    unsigned differ = 0, i, j;
    if (s->tick != tick || a->tick[slot] != tick) {
        return INTERIORS_MAX_ENTITIES;
    }
    for (i = 0; i < a->hash_count[slot]; ++i) {
        int found = 0;
        for (j = 0; j < s->hash_count; ++j) {
            if (s->hash_entity[j] == a->hash_entity[slot][i]) {
                found = 1;
                if (memcmp(s->hashes[j], a->hashes[slot][i], 32) != 0) {
                    differ += 1;
                }
                break;
            }
        }
        if (!found) {
            differ += 1;
        }
    }
    if (s->hash_count > a->hash_count[slot]) {
        differ += s->hash_count - a->hash_count[slot];
    }
    return differ;
}

#ifdef __cplusplus
}
#endif

#endif /* INTERIORS_SHARED_H */
