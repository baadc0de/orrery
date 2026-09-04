#ifndef ORRERY_UNREAL_INTERIORS_H
#define ORRERY_UNREAL_INTERIORS_H

/*
 * Spike #1045, moving interiors: what the staticlib exports beyond
 * orrery_sim_host.h.
 *
 * One handle, the generic orrery_host from orrery_sim_host.h, created here
 * over the throwaway nested-frame ruleset (crates/orrery_unreal_interiors/
 * src/rules.rs) by the one factory a game adds. Built on spike #1052's
 * non-App prong: no bevy_app::App, no second handle, no clock inside; the
 * prediction ring and the rollback live in the consumer
 * (examples/c/interiors_shared.h), driven through orrery_host_snapshot /
 * orrery_host_restore / orrery_host_install_state / orrery_host_step.
 *
 * Result codes are orrery_host_result from orrery_sim_host.h with the same
 * meaning. Calls on one handle are serialized by the caller.
 */

#include "orrery_sim_host.h"

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* The canonical encoding length of one Body (rules.rs, BODY_ENCODED_LEN):
 *   [kind u8][frame u64][pos i64 x3][vel i64 x3]
 *   [yaw i32][roll i32][yaw_rate i32][roll_rate i32][frame_changes u32] */
#define ORRERY_INTERIORS_BODY_BYTES 77u

/* The ruleset version the consumer's decoder was written against. */
#define ORRERY_INTERIORS_RULESET_VERSION 1u

/* Scene ids (host.rs, the population table). */
#define ORRERY_INTERIORS_UNIVERSE 0u
#define ORRERY_INTERIORS_STATION 1u
#define ORRERY_INTERIORS_SHIP 2u
#define ORRERY_INTERIORS_MECH 3u
#define ORRERY_INTERIORS_AVATAR 4u

/* Creates a host running the nested-frame rules, empty. seed names 32 bytes. */
orrery_host_result orrery_interiors_host_create(
    const uint8_t *seed, uint64_t first_tick, orrery_host **out_host);

/* How many bodies the scene holds (ids 1..len). */
uint64_t orrery_interiors_scene_len(void);

/* The canonical bytes of scene body `entity`, for
 * orrery_host_install_state. Buffer convention as orrery_sim_host.h. */
orrery_host_result orrery_interiors_scene_state(
    uint64_t entity, uint8_t *out_bytes, size_t capacity, size_t *out_required);

#ifdef __cplusplus
}
#endif

#endif /* ORRERY_UNREAL_INTERIORS_H */
