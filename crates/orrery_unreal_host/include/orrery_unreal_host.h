#ifndef ORRERY_UNREAL_HOST_H
#define ORRERY_UNREAL_HOST_H

/*
 * Spike #1043, engine-independent half: what the staticlib exports beyond
 * orrery_sim_host.h.
 *
 * Two handles, side by side. The orrery_host handle is the generic one from
 * orrery_sim_host.h, created here over a real ruleset (orrery_games'
 * Skirmish) by the one factory a game adds. The orrery_app handle wraps a
 * headless bevy_app::App (MinimalPlugins, OrreryNetPlugin, OrreryPredictPlugin)
 * that the caller's own loop updates once per fixed tick. Neither handle knows
 * about the other: D53 records that no code connects the prediction plugin to
 * the host seam, and this spike measures the two beside each other rather
 * than pretending to have built that driver.
 *
 * Result codes are orrery_host_result from orrery_sim_host.h, with the same
 * meaning: every call catches Rust panics and reports ORRERY_HOST_PANIC; a
 * panic inside orrery_app_update poisons the app handle and only
 * orrery_app_destroy is accepted afterwards.
 *
 * Calls on one handle are serialized by the caller; concurrent calls are
 * unsupported.
 */

#include "orrery_sim_host.h"

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define ORRERY_UNREAL_HOST_APP_ABI_VERSION 1u

/* The canonical encoding length of one Skirmish craft state
 * (orrery_games::skirmish::state::CRAFT_ENCODED_LEN). The consumer's decoder
 * is written against it and checks the ruleset id before trusting it. */
#define ORRERY_SKIRMISH_CRAFT_BYTES 79u

/* ---- the game's factory and helpers ------------------------------------ */

/* Creates a host running Skirmish's honest rules. seed names 32 bytes. */
orrery_host_result orrery_skirmish_host_create(
    const uint8_t *seed, uint64_t first_tick, orrery_host **out_host);

/* The canonical bytes of the craft Skirmish spawns in slot, for
 * orrery_host_install_state. Buffer convention as orrery_sim_host.h. */
orrery_host_result orrery_skirmish_spawn_state(
    uint64_t entity,
    uint64_t slot,
    uint8_t *out_bytes,
    size_t capacity,
    size_t *out_required);

/* What Skirmish's honest pilot in slot asks for at tick, as
 * [u32 len LE][command bytes] records; each command is what
 * orrery_host_submit_command takes. peers names the peer_count other
 * entities the pilot may fire at. */
orrery_host_result orrery_skirmish_honest_commands(
    const uint8_t *seed,
    uint64_t entity,
    uint64_t slot,
    uint64_t tick,
    const uint64_t *peers,
    size_t peer_count,
    uint8_t *out_bytes,
    size_t capacity,
    size_t *out_required);

/* ---- the headless App ---------------------------------------------------- */

typedef struct orrery_app orrery_app;

/* Who advances Bevy's clock. */
typedef enum orrery_app_clock {
    /* TimePlugin reads the wall clock; Time<Fixed> steps 0, 1 or 2 times per
     * update depending on where the wall clock fell. */
    ORRERY_APP_CLOCK_AUTOMATIC = 0,
    /* Every update advances Bevy by exactly the dt_ns passed, so a caller
     * passing fixed_step_ns runs FixedMain exactly once per update. */
    ORRERY_APP_CLOCK_MANUAL = 1
} orrery_app_clock;

/* The tick counters the App carries, read back by the driver. */
typedef struct orrery_app_timeline {
    /* lightyear's LocalTimeline tick as orrery_predict's bridge saw it. */
    uint32_t lightyear_tick;
    /* The universe tick the bridge resolves that to. */
    uint64_t bridged_tick;
    /* How many times FixedUpdate has run. */
    uint64_t fixed_steps;
    /* Bevy's FrameCount. */
    uint32_t frames;
    /* Time<Virtual>::elapsed, nanoseconds. */
    uint64_t virtual_elapsed_ns;
    /* Time<Fixed>::timestep, nanoseconds. */
    uint64_t fixed_step_ns;
} orrery_app_timeline;

uint32_t orrery_app_abi_version(void);

/* Creates the App on the calling thread. clock is an orrery_app_clock. */
orrery_host_result orrery_app_create(uint32_t clock, orrery_app **out_app);

/* One App::update(). dt_ns is honoured under ORRERY_APP_CLOCK_MANUAL and
 * ignored under AUTOMATIC. */
orrery_host_result orrery_app_update(orrery_app *app, uint64_t dt_ns);

orrery_host_result orrery_app_timeline_read(orrery_app *app, orrery_app_timeline *out);

/* 1 if the calling thread created the App, else 0. */
uint32_t orrery_app_on_creating_thread(const orrery_app *app);

/* Arms a system that panics on the next update. */
orrery_host_result orrery_app_request_panic(orrery_app *app);

/* Accepted on a poisoned handle. */
orrery_host_result orrery_app_destroy(orrery_app *app);

#ifdef __cplusplus
}
#endif

#endif /* ORRERY_UNREAL_HOST_H */
