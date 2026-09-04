#ifndef ORRERY_UNREAL_DIRECT_H
#define ORRERY_UNREAL_DIRECT_H

/*
 * Spike #1052, the non-App prong: what the staticlib exports beyond
 * orrery_sim_host.h.
 *
 * One handle. The orrery_host handle is the generic one from
 * orrery_sim_host.h, created here over a real ruleset (orrery_games'
 * Skirmish) by the one factory a game adds. There is no second handle: no
 * bevy_app::App, no schedule runner, no task pool, no lightyear, no iroh
 * endpoint, no tokio runtime. The prediction ring, the correction intake,
 * the rollback and the replay live in the consumer, on the far side of this
 * header, driven through orrery_host_snapshot / orrery_host_restore /
 * orrery_host_step and nothing else (see examples/c/direct_consumer.c).
 *
 * Result codes are orrery_host_result from orrery_sim_host.h, with the same
 * meaning: every call catches Rust panics and reports ORRERY_HOST_PANIC; a
 * panic inside a mutating call poisons the handle and only
 * orrery_host_destroy is accepted afterwards.
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

/* The canonical encoding length of one Skirmish craft state
 * (orrery_games::skirmish::state::CRAFT_ENCODED_LEN, state.rs:70). The
 * consumer's decoder is written against it and checks the ruleset id before
 * trusting it. */
#define ORRERY_SKIRMISH_CRAFT_BYTES 79u

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

#ifdef __cplusplus
}
#endif

#endif /* ORRERY_UNREAL_DIRECT_H */
