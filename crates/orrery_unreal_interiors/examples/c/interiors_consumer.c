/*
 * The C consumer of spike #1045's staticlib. It links
 * liborrery_unreal_interiors.a, creates the one handle over the nested-frame
 * scene, owns the fixed-step loop and the D8 ring (interiors_shared.h, lifted
 * from spike #1052), and answers the part of #1045 that needs no engine:
 *
 *   rollback across the frame change — does a correction whose window spans
 *   the tick an entity changed frames replay to the authority's hashes?
 *
 * Modes (argv[1]):
 *   smoke                          400 ticks of the roll scene; prints the bodies and the chain
 *   trace SCENE [TICKS] [CSV]      the scene alone on the predicted host: per-tick step cost,
 *                                  the hash chain (compare with the Unreal run's), optional
 *                                  CSV of every body's frame-local pose per tick
 *   rollback SCENE [TICKS] [--report PATH] [--control-every K] [--shape identity|ship|avatar]
 *                                  client + stand-in authority in lockstep on the scene's script.
 *                                  At every frame change Tc the script issues, one correction
 *                                  is arranged so that its window [Ta, now) spans Tc:
 *                                    j     = i mod 9            (i = the transition's index)
 *                                    Ta    = Tc - j             (the authoritative tick)
 *                                    now   = Ta + 9             (D8's full window)
 *                                    shape = (i div 9) mod 3:
 *                                      identity  no divergence; the correction carries the
 *                                                client's own bytes
 *                                      ship      the authority applied a Cruise on the ship at
 *                                                Ta - 1 the client never saw: the frame the
 *                                                avatar crosses into is not the one it predicted
 *                                      avatar    the authority applied a different Move on the
 *                                                avatar at Ta - 1: the crossing entity's own
 *                                                pose differs
 *                                  The correction carries the authority's bytes for all four
 *                                  bodies at Ta. After restore + install + replay, the client's
 *                                  hashes for every (entity, tick) in [Ta, now) are compared
 *                                  with the authority's, and again for up to 30 ticks after
 *                                  (stopping before the next arranged divergence). Every
 *                                  correction is tagged with its arrangement; nothing else
 *                                  corrects the client, so a mismatch is the injected one's.
 *                                  Control corrections (shape ship, depth 5) every K ticks
 *                                  away from any transition give the non-spanning baseline.
 *
 * SCENE is one of rest, straight, roll, mech, transitions.
 */

#define _POSIX_C_SOURCE 200809L

#include "interiors_shared.h"

#include <inttypes.h>
#include <time.h>

static uint64_t now_ns(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        fprintf(stderr, "clock_gettime(CLOCK_MONOTONIC) failed\n");
        exit(2);
    }
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a;
    uint64_t y = *(const uint64_t *)b;
    return x < y ? -1 : x > y ? 1 : 0;
}

static uint64_t rank(uint64_t *samples, size_t n, double pct) {
    double index, ceiled;
    size_t i;
    if (n == 0) {
        return 0;
    }
    qsort(samples, n, sizeof *samples, cmp_u64);
    index = (pct / 100.0) * (double)n;
    ceiled = (double)(uint64_t)index;
    if (ceiled < index) {
        ceiled += 1.0;
    }
    if (ceiled < 1.0) {
        ceiled = 1.0;
    }
    i = (size_t)ceiled - 1;
    return samples[i < n - 1 ? i : n - 1];
}

static void read_loadavg(double out[3]) {
    FILE *f = fopen("/proc/loadavg", "r");
    out[0] = out[1] = out[2] = 0.0;
    if (f == NULL) {
        return;
    }
    if (fscanf(f, "%lf %lf %lf", &out[0], &out[1], &out[2]) != 3) {
        out[0] = out[1] = out[2] = 0.0;
    }
    fclose(f);
}

static const uint8_t SEED[32] = {0x10, 0x45, 0x10, 0x45, 0x10, 0x45, 0x10, 0x45, 0x10, 0x45, 0x10,
                                 0x45, 0x10, 0x45, 0x10, 0x45, 0x10, 0x45, 0x10, 0x45, 0x10, 0x45,
                                 0x10, 0x45, 0x10, 0x45, 0x10, 0x45, 0x10, 0x45, 0x10, 0x45};

static uint64_t default_ticks(interiors_scene scene) {
    return scene == INTERIORS_SCENE_TRANSITIONS ? 24 * INTERIORS_CYCLE_TICKS : 36000;
}

static void print_body(const char *name, const interiors_body *b) {
    printf("%s kind=%u frame=%" PRIu64 " pos=(%" PRId64 ",%" PRId64 ",%" PRId64 ") vel=(%" PRId64
           ",%" PRId64 ",%" PRId64 ") yaw=%" PRId32 " roll=%" PRId32 " changes=%" PRIu32 "\n",
           name, b->kind, b->frame, b->pos[0], b->pos[1], b->pos[2], b->vel[0], b->vel[1],
           b->vel[2], b->yaw_urad, b->roll_urad, b->frame_changes);
}

/* ---- trace ------------------------------------------------------------------- */

static int run_trace(interiors_scene scene, uint64_t total, const char *csv_path, int quiet) {
    orrery_host *host = interiors_create_host(SEED);
    interiors_predictor p;
    uint64_t *step_ns;
    uint64_t tick;
    FILE *csv = NULL;
    if (host == NULL) {
        return 2;
    }
    interiors_predictor_init(&p, host);
    step_ns = (uint64_t *)calloc(total, sizeof *step_ns);
    if (csv_path != NULL) {
        csv = fopen(csv_path, "w");
        if (csv == NULL) {
            fprintf(stderr, "cannot write %s\n", csv_path);
            return 2;
        }
        fprintf(csv, "tick,ship_frame,ship_x,ship_y,ship_z,ship_yaw,ship_roll,mech_frame,mech_x,"
                     "mech_y,mech_z,mech_yaw,av_frame,av_x,av_y,av_z,av_yaw\n");
    }
    for (tick = 0; tick < total; ++tick) {
        uint64_t t0 = now_ns();
        if (interiors_predict_tick(&p, scene, total) != tick) {
            fprintf(stderr, "step failed at tick %" PRIu64 "\n", tick);
            return 2;
        }
        step_ns[tick] = now_ns() - t0;
        if (csv != NULL) {
            interiors_body ship, mech, av;
            (void)interiors_read_body(host, ORRERY_INTERIORS_SHIP, &ship);
            (void)interiors_read_body(host, ORRERY_INTERIORS_MECH, &mech);
            (void)interiors_read_body(host, ORRERY_INTERIORS_AVATAR, &av);
            fprintf(csv,
                    "%" PRIu64 ",%" PRIu64 ",%" PRId64 ",%" PRId64 ",%" PRId64 ",%" PRId32
                    ",%" PRId32 ",%" PRIu64 ",%" PRId64 ",%" PRId64 ",%" PRId64 ",%" PRId32
                    ",%" PRIu64 ",%" PRId64 ",%" PRId64 ",%" PRId64 ",%" PRId32 "\n",
                    tick, ship.frame, ship.pos[0], ship.pos[1], ship.pos[2], ship.yaw_urad,
                    ship.roll_urad, mech.frame, mech.pos[0], mech.pos[1], mech.pos[2],
                    mech.yaw_urad, av.frame, av.pos[0], av.pos[1], av.pos[2], av.yaw_urad);
        }
        {
            uint8_t sink[4096];
            size_t required = 0;
            (void)orrery_host_drain_events(host, sink, sizeof sink, &required);
        }
    }
    if (csv != NULL) {
        fclose(csv);
    }
    {
        interiors_body ship, mech, av;
        (void)interiors_read_body(host, ORRERY_INTERIORS_SHIP, &ship);
        (void)interiors_read_body(host, ORRERY_INTERIORS_MECH, &mech);
        (void)interiors_read_body(host, ORRERY_INTERIORS_AVATAR, &av);
        if (!quiet) {
            print_body("ship", &ship);
            print_body("mech", &mech);
            print_body("avatar", &av);
        }
        printf("trace scene=%s ticks=%" PRIu64 " chain=%016" PRIx64
               " tick_ns_p50=%" PRIu64 " tick_ns_p99=%" PRIu64 " tick_ns_max=%" PRIu64
               " input_dropped=%" PRIu64 " snapshot_failed=%" PRIu64 "\n",
               interiors_scene_name(scene), total, p.chain, rank(step_ns, total, 50.0),
               rank(step_ns, total, 99.0), rank(step_ns, total, 100.0), p.input_dropped,
               p.snapshot_failed);
    }
    free(step_ns);
    interiors_predictor_free(&p);
    (void)orrery_host_destroy(host);
    return 0;
}

/* ---- rollback ---------------------------------------------------------------- */

typedef enum shape { SHAPE_IDENTITY = 0, SHAPE_SHIP = 1, SHAPE_AVATAR = 2, SHAPES = 3 } shape;

static const char *shape_name(shape s) {
    static const char *names[SHAPES] = {"identity", "ship", "avatar"};
    return names[s];
}

/* One arranged correction. */
typedef struct arrangement {
    int kind; /* interiors_transition_kind, or -1 for a control correction */
    shape shape;
    uint64_t transition_tick; /* Tc, or the control tick */
    uint64_t authoritative_tick; /* Ta */
    uint64_t divergence_tick; /* Ta - 1; unused for identity */
    uint64_t now; /* Ta + 9 */
    /* results */
    interiors_rollback_report report;
    unsigned mismatch_window, mismatch_after, compared_after;
    int applied;
} arrangement;

typedef struct bucket {
    unsigned n, mismatch_window, mismatch_after, hashes_changed, rollback, snap;
    int64_t residual_max;
    uint64_t total_ns[4096];
    unsigned total_count;
} bucket;

static void bucket_add(bucket *b, const arrangement *a) {
    b->n += 1;
    b->mismatch_window += a->mismatch_window;
    b->mismatch_after += a->mismatch_after;
    b->hashes_changed += a->report.hashes_changed;
    if (a->report.plan == INTERIORS_PLAN_ROLLBACK) {
        b->rollback += 1;
    } else {
        b->snap += 1;
    }
    if (a->report.residual_mm > b->residual_max) {
        b->residual_max = a->report.residual_mm;
    }
    if (b->total_count < 4096) {
        b->total_ns[b->total_count++] = a->report.total_ns;
    }
}

static int run_rollback(interiors_scene scene, uint64_t total, const char *report_path,
                        uint64_t control_every, int force_shape) {
    orrery_host *client_host = interiors_create_host(SEED);
    orrery_host *authority_host = interiors_create_host(SEED);
    interiors_predictor client;
    interiors_authority authority;
    interiors_transition transitions[4096];
    unsigned transition_count, i;
    arrangement *arr;
    unsigned arr_count = 0, arr_cap;
    uint64_t tick, last_tx = 0;
    double load_start[3], load_end[3];
    bucket by_kind[INTERIORS_TR_KINDS + 1][SHAPES];
    bucket by_depth[INTERIORS_WINDOW + 1];
    bucket all;
    unsigned lockstep_mismatch = 0;
    uint64_t lockstep_compared = 0;

    if (client_host == NULL || authority_host == NULL) {
        return 2;
    }
    read_loadavg(load_start);
    interiors_predictor_init(&client, client_host);
    interiors_authority_init(&authority, authority_host);
    transition_count = interiors_script_transitions(scene, total, transitions, 4096);

    /* Arrange every correction up front, in tick order. */
    arr_cap = transition_count + (control_every ? (unsigned)(total / control_every) + 1 : 0) + 1;
    arr = (arrangement *)calloc(arr_cap, sizeof *arr);
    for (i = 0; i < transition_count; ++i) {
        arrangement *a = &arr[arr_count++];
        uint64_t j = i % INTERIORS_WINDOW;
        a->kind = (int)transitions[i].kind;
        /* Shapes cycle every nine transitions so each shape sees every
         * depth; a scene with few transitions is run once per shape with
         * --shape instead (spike.sh does, for the mech scene). */
        a->shape = force_shape >= 0 ? (shape)force_shape : (shape)((i / INTERIORS_WINDOW) % SHAPES);
        a->transition_tick = transitions[i].tick;
        a->authoritative_tick = transitions[i].tick - j;
        a->divergence_tick = a->authoritative_tick - 1;
        a->now = a->authoritative_tick + INTERIORS_WINDOW;
    }
    if (control_every) {
        for (tick = control_every; tick + INTERIORS_WINDOW + 40 < total; tick += control_every) {
            int near = 0;
            for (i = 0; i < transition_count; ++i) {
                int64_t d = (int64_t)transitions[i].tick - (int64_t)tick;
                if (d > -60 && d < 60) {
                    near = 1;
                }
            }
            if (!near) {
                arrangement *a = &arr[arr_count++];
                a->kind = -1;
                a->shape = SHAPE_SHIP;
                a->transition_tick = tick;
                a->authoritative_tick = tick - 5;
                a->divergence_tick = a->authoritative_tick - 1;
                a->now = a->authoritative_tick + INTERIORS_WINDOW;
            }
        }
    }
    /* Sort by `now` (insertion sort; small). */
    for (i = 1; i < arr_count; ++i) {
        arrangement key = arr[i];
        unsigned k = i;
        while (k > 0 && arr[k - 1].now > key.now) {
            arr[k] = arr[k - 1];
            k -= 1;
        }
        arr[k] = key;
    }
    memset(by_kind, 0, sizeof by_kind);
    memset(by_depth, 0, sizeof by_depth);
    memset(&all, 0, sizeof all);

    {
        unsigned next_arr = 0;   /* the next arrangement whose correction has not fired */
        unsigned post_arr = 0;   /* index of the arrangement being post-compared, or arr_count */
        uint64_t post_until = 0; /* compare ticks < post_until after a correction */
        for (tick = 0; tick < total; ++tick) {
            /* A correction that arrives at this boundary. */
            while (next_arr < arr_count && arr[next_arr].now == tick) {
                arrangement *a = &arr[next_arr];
                interiors_correction_entity ent[INTERIORS_SCENE_ENTITIES];
                unsigned slot = (unsigned)(a->authoritative_tick % INTERIORS_AUTHORITY_RING);
                uint64_t t;
                unsigned e;
                if (authority.tick[slot] != a->authoritative_tick) {
                    fprintf(stderr, "authority ring miss at %" PRIu64 "\n", a->authoritative_tick);
                    return 2;
                }
                for (e = 0; e < INTERIORS_SCENE_ENTITIES; ++e) {
                    ent[e].entity = e + 1;
                    memcpy(ent[e].bytes, authority.state[slot][e], ORRERY_INTERIORS_BODY_BYTES);
                }
                a->report = interiors_apply_correction(&client, ent, INTERIORS_SCENE_ENTITIES,
                                                       a->authoritative_tick, a->now, now_ns);
                a->applied = 1;
                for (t = a->authoritative_tick; t < a->now; ++t) {
                    a->mismatch_window += interiors_compare_tick(&client, &authority, t);
                }
                post_arr = next_arr;
                post_until = a->now + 30;
                /* Stop before the next arranged divergence, which is the
                 * authority leaving the client again on purpose. */
                {
                    unsigned k;
                    for (k = next_arr + 1; k < arr_count; ++k) {
                        if (arr[k].shape != SHAPE_IDENTITY && arr[k].divergence_tick < post_until) {
                            post_until = arr[k].divergence_tick;
                        }
                    }
                }
                next_arr += 1;
            }
            /* The divergence the authority applies this tick, if any. */
            authority.has_extra = 0;
            {
                unsigned k;
                for (k = next_arr; k < arr_count; ++k) {
                    if (arr[k].divergence_tick == tick && arr[k].shape != SHAPE_IDENTITY) {
                        interiors_body ship;
                        authority.has_extra = 1;
                        authority.extra_tick = tick;
                        last_tx = tick;
                        if (arr[k].shape == SHAPE_SHIP) {
                            (void)interiors_read_body(authority_host, ORRERY_INTERIORS_SHIP, &ship);
                            interiors_cmd_cruise(&authority.extra, ORRERY_INTERIORS_SHIP,
                                                 ship.vel[0], ship.vel[1] + 3000, ship.vel[2],
                                                 ship.yaw_rate_urad_tick + 100,
                                                 ship.roll_rate_urad_tick + 200);
                        } else {
                            interiors_cmd_move(&authority.extra, ORRERY_INTERIORS_AVATAR, 1200, 0,
                                               0, 0);
                        }
                        break;
                    }
                }
            }
            if (interiors_predict_tick(&client, scene, total) != tick) {
                fprintf(stderr, "client step failed at %" PRIu64 "\n", tick);
                return 2;
            }
            if (interiors_authority_tick(&authority, scene, total) != tick) {
                fprintf(stderr, "authority step failed at %" PRIu64 "\n", tick);
                return 2;
            }
            {
                uint8_t sink[4096];
                size_t required = 0;
                (void)orrery_host_drain_events(client_host, sink, sizeof sink, &required);
            }
            if (post_arr < arr_count && tick < post_until && tick >= arr[post_arr].now) {
                unsigned d = interiors_compare_tick(&client, &authority, tick);
                arr[post_arr].mismatch_after += d;
                arr[post_arr].compared_after += 1;
            }
            /* Lockstep sanity when nothing has diverged: before the first
             * divergence the two hosts must agree tick for tick (the two
             * processes' chains rest on this). */
            if (last_tx == 0 && authority.has_extra == 0) {
                lockstep_mismatch += interiors_compare_tick(&client, &authority, tick);
                lockstep_compared += 1;
            }
        }
    }
    read_loadavg(load_end);

    for (i = 0; i < arr_count; ++i) {
        const arrangement *a = &arr[i];
        if (!a->applied) {
            continue;
        }
        bucket_add(&by_kind[a->kind < 0 ? INTERIORS_TR_KINDS : (unsigned)a->kind][a->shape], a);
        bucket_add(&by_depth[a->report.depth <= INTERIORS_WINDOW ? a->report.depth : 0], a);
        bucket_add(&all, a);
    }

    printf("rollback scene=%s ticks=%" PRIu64 " transitions=%u corrections=%u "
           "mismatch_window=%u mismatch_after=%u rollback=%u snap=%u restore_failed=%" PRIu64
           " replay_step_failed=%" PRIu64 " events_reemitted_by_replay=%" PRIu64
           " lockstep_compared=%" PRIu64 " lockstep_mismatch=%u total_ns_p50=%" PRIu64
           " total_ns_p99=%" PRIu64 " total_ns_max=%" PRIu64 " loadavg=%.2f/%.2f\n",
           interiors_scene_name(scene), total, transition_count, all.n, all.mismatch_window,
           all.mismatch_after, all.rollback, all.snap, client.restore_failed,
           client.replay_step_failed, client.events_reemitted_by_replay, lockstep_compared,
           lockstep_mismatch, rank(all.total_ns, all.total_count, 50.0),
           rank(all.total_ns, all.total_count, 99.0), rank(all.total_ns, all.total_count, 100.0),
           load_start[0], load_end[0]);
    printf("%-15s %-9s %4s %6s %6s %8s %8s %10s %10s\n", "transition", "shape", "n", "mis_w",
           "mis_a", "h_chg", "resid_mm", "ns_p50", "ns_p99");
    {
        unsigned k, s;
        for (k = 0; k <= INTERIORS_TR_KINDS; ++k) {
            for (s = 0; s < SHAPES; ++s) {
                bucket *b = &by_kind[k][s];
                if (b->n == 0) {
                    continue;
                }
                printf("%-15s %-9s %4u %6u %6u %8u %8" PRId64 " %10" PRIu64 " %10" PRIu64 "\n",
                       k == INTERIORS_TR_KINDS ? "control"
                                               : interiors_transition_name((interiors_transition_kind)k),
                       shape_name((shape)s), b->n, b->mismatch_window, b->mismatch_after,
                       b->hashes_changed, b->residual_max, rank(b->total_ns, b->total_count, 50.0),
                       rank(b->total_ns, b->total_count, 99.0));
            }
        }
        printf("%-6s %4s %6s %6s %10s %10s %10s\n", "depth", "n", "mis_w", "mis_a", "ns_p50",
               "ns_p99", "ns_max");
        for (k = 1; k <= INTERIORS_WINDOW; ++k) {
            bucket *b = &by_depth[k];
            if (b->n == 0) {
                continue;
            }
            printf("%-6u %4u %6u %6u %10" PRIu64 " %10" PRIu64 " %10" PRIu64 "\n", k, b->n,
                   b->mismatch_window, b->mismatch_after, rank(b->total_ns, b->total_count, 50.0),
                   rank(b->total_ns, b->total_count, 99.0),
                   rank(b->total_ns, b->total_count, 100.0));
        }
    }

    if (report_path != NULL) {
        FILE *f = fopen(report_path, "w");
        unsigned k, s;
        if (f == NULL) {
            fprintf(stderr, "cannot write %s\n", report_path);
            return 2;
        }
        fprintf(f, "{\n  \"schema\": \"orrery-interiors-rollback/1\",\n");
        fprintf(f, "  \"scene\": \"%s\", \"ticks\": %" PRIu64 ", \"transitions\": %u,\n",
                interiors_scene_name(scene), total, transition_count);
        fprintf(f, "  \"loadavg_start\": %.2f, \"loadavg_end\": %.2f,\n", load_start[0],
                load_end[0]);
        fprintf(f,
                "  \"corrections\": %u, \"mismatch_window\": %u, \"mismatch_after\": %u, "
                "\"rollback\": %u, \"snap\": %u, \"restore_failed\": %" PRIu64
                ", \"replay_step_failed\": %" PRIu64 ", \"events_reemitted_by_replay\": %" PRIu64
                ", \"lockstep_compared\": %" PRIu64 ", \"lockstep_mismatch\": %u,\n",
                all.n, all.mismatch_window, all.mismatch_after, all.rollback, all.snap,
                client.restore_failed, client.replay_step_failed,
                client.events_reemitted_by_replay, lockstep_compared, lockstep_mismatch);
        fprintf(f, "  \"total_ns\": {\"p50\": %" PRIu64 ", \"p99\": %" PRIu64 ", \"max\": %" PRIu64
                   "},\n",
                rank(all.total_ns, all.total_count, 50.0), rank(all.total_ns, all.total_count, 99.0),
                rank(all.total_ns, all.total_count, 100.0));
        fprintf(f, "  \"by_transition\": [\n");
        {
            int first = 1;
            for (k = 0; k <= INTERIORS_TR_KINDS; ++k) {
                for (s = 0; s < SHAPES; ++s) {
                    bucket *b = &by_kind[k][s];
                    if (b->n == 0) {
                        continue;
                    }
                    fprintf(f,
                            "%s    {\"transition\": \"%s\", \"shape\": \"%s\", \"n\": %u, "
                            "\"mismatch_window\": %u, \"mismatch_after\": %u, \"hashes_changed\": "
                            "%u, \"residual_mm_max\": %" PRId64 ", \"total_ns_p50\": %" PRIu64
                            ", \"total_ns_p99\": %" PRIu64 "}",
                            first ? "" : ",\n",
                            k == INTERIORS_TR_KINDS
                                ? "control"
                                : interiors_transition_name((interiors_transition_kind)k),
                            shape_name((shape)s), b->n, b->mismatch_window, b->mismatch_after,
                            b->hashes_changed, b->residual_max,
                            rank(b->total_ns, b->total_count, 50.0),
                            rank(b->total_ns, b->total_count, 99.0));
                    first = 0;
                }
            }
        }
        fprintf(f, "\n  ],\n  \"by_depth\": [\n");
        {
            int first = 1;
            for (k = 1; k <= INTERIORS_WINDOW; ++k) {
                bucket *b = &by_depth[k];
                if (b->n == 0) {
                    continue;
                }
                fprintf(f,
                        "%s    {\"depth\": %u, \"n\": %u, \"mismatch_window\": %u, "
                        "\"mismatch_after\": %u, \"total_ns_p50\": %" PRIu64
                        ", \"total_ns_p99\": %" PRIu64 ", \"total_ns_max\": %" PRIu64 "}",
                        first ? "" : ",\n", k, b->n, b->mismatch_window, b->mismatch_after,
                        rank(b->total_ns, b->total_count, 50.0),
                        rank(b->total_ns, b->total_count, 99.0),
                        rank(b->total_ns, b->total_count, 100.0));
                first = 0;
            }
        }
        fprintf(f, "\n  ],\n  \"corrections_detail\": [\n");
        for (i = 0; i < arr_count; ++i) {
            const arrangement *a = &arr[i];
            if (!a->applied) {
                continue;
            }
            fprintf(f,
                    "    {\"transition\": \"%s\", \"shape\": \"%s\", \"tc\": %" PRIu64
                    ", \"ta\": %" PRIu64 ", \"now\": %" PRIu64 ", \"depth\": %" PRIu64
                    ", \"plan\": \"%s\", \"mismatch_window\": %u, \"mismatch_after\": %u, "
                    "\"compared_after\": %u, \"hashes_changed\": %u, \"residual_mm\": %" PRId64
                    ", \"frame_before\": %" PRIu64 ", \"frame_after\": %" PRIu64
                    ", \"restore_ns\": %" PRIu64 ", \"install_ns\": %" PRIu64
                    ", \"replay_ns\": %" PRIu64 ", \"total_ns\": %" PRIu64 "}%s\n",
                    a->kind < 0 ? "control"
                                : interiors_transition_name((interiors_transition_kind)a->kind),
                    shape_name(a->shape), a->transition_tick, a->authoritative_tick, a->now,
                    a->report.depth, a->report.plan == INTERIORS_PLAN_ROLLBACK ? "rollback" : "snap",
                    a->mismatch_window, a->mismatch_after, a->compared_after,
                    a->report.hashes_changed, a->report.residual_mm, a->report.frame_before,
                    a->report.frame_after, a->report.restore_ns, a->report.install_ns,
                    a->report.replay_ns, a->report.total_ns, i + 1 < arr_count ? "," : "");
        }
        fprintf(f, "  ]\n}\n");
        fclose(f);
    }

    free(arr);
    interiors_predictor_free(&client);
    (void)orrery_host_destroy(client_host);
    (void)orrery_host_destroy(authority_host);
    return (all.mismatch_window + all.mismatch_after + lockstep_mismatch) == 0 ? 0 : 1;
}

/* ---- main -------------------------------------------------------------------- */

static int usage(void) {
    fprintf(stderr, "usage: interiors_consumer smoke | trace SCENE [TICKS] [CSV] | rollback SCENE "
                    "[TICKS] [--report PATH] [--control-every K]\n");
    return 2;
}

int main(int argc, char **argv) {
    if (argc < 2) {
        return usage();
    }
    if (strcmp(argv[1], "smoke") == 0) {
        return run_trace(INTERIORS_SCENE_ROLL, 400, NULL, 0);
    }
    if (strcmp(argv[1], "trace") == 0) {
        interiors_scene scene;
        uint64_t ticks;
        if (argc < 3 || !interiors_scene_parse(argv[2], &scene)) {
            return usage();
        }
        ticks = argc > 3 ? strtoull(argv[3], NULL, 10) : 0;
        if (ticks == 0) {
            ticks = default_ticks(scene);
        }
        return run_trace(scene, ticks, argc > 4 ? argv[4] : NULL, 0);
    }
    if (strcmp(argv[1], "rollback") == 0) {
        interiors_scene scene;
        uint64_t ticks, control_every = 120;
        const char *report = NULL;
        int i, force_shape = -1;
        if (argc < 3 || !interiors_scene_parse(argv[2], &scene)) {
            return usage();
        }
        ticks = default_ticks(scene);
        for (i = 3; i < argc; ++i) {
            if (strcmp(argv[i], "--report") == 0 && i + 1 < argc) {
                report = argv[++i];
            } else if (strcmp(argv[i], "--control-every") == 0 && i + 1 < argc) {
                control_every = strtoull(argv[++i], NULL, 10);
            } else if (strcmp(argv[i], "--shape") == 0 && i + 1 < argc) {
                int k;
                i += 1;
                for (k = 0; k < SHAPES; ++k) {
                    if (strcmp(shape_name((shape)k), argv[i]) == 0) {
                        force_shape = k;
                    }
                }
            } else {
                ticks = strtoull(argv[i], NULL, 10);
            }
        }
        return run_rollback(scene, ticks, report, control_every, force_shape);
    }
    return usage();
}
