#ifndef ORRERY_SIM_H
#define ORRERY_SIM_H

/*
 * Pre-S5 spike ABI.  All data crossing this boundary is either an opaque
 * handle, a fixed-width C record, or a caller-owned contiguous byte buffer.
 * This header is C and C++ compatible; no Rust headers or runtime types are
 * required by its callers.
 */

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define ORRERY_SIM_ABI_VERSION 1u

typedef struct orrery_sim orrery_sim;

typedef enum orrery_sim_result {
    ORRERY_SIM_OK = 0,
    ORRERY_SIM_NULL_ARGUMENT = 1,
    ORRERY_SIM_BUFFER_TOO_SMALL = 2,
    ORRERY_SIM_MALFORMED_INPUT = 3,
    ORRERY_SIM_UNANCHORED_DELTA = 4,
    ORRERY_SIM_PANIC = 5
} orrery_sim_result;

/* A renderer-ready craft mirror. Positions are canonical millimetres and
 * rotations are canonical microradians; presentation converts/interpolates
 * these values but must not write them back into the simulation. */
typedef struct orrery_sim_craft_transform {
    uint64_t craft_id;
    int64_t x_mm;
    int64_t y_mm;
    int64_t z_mm;
    int32_t yaw_urad;
    int32_t pitch_urad;
} orrery_sim_craft_transform;

/* Allocates a headless Regolith mirror. Destroy it with orrery_sim_destroy. */
orrery_sim_result orrery_sim_create(orrery_sim **out_sim);

/* The caller must stop using sim after this returns. Calls on one handle are
 * serialized by the caller; concurrent calls are unsupported. */
orrery_sim_result orrery_sim_destroy(orrery_sim *sim);

/* Advances the fixed simulation by exactly ticks; it never reads wall time. */
orrery_sim_result orrery_sim_step(orrery_sim *sim, uint64_t ticks);

/* Queues one command frame for the next step. Its flat wire format is:
 * [entity_id: u64 little-endian][Regolith Order canonical bytes]. */
orrery_sim_result orrery_sim_submit_command(
    orrery_sim *sim, const uint8_t *command_bytes, size_t command_len);

/* Applies one current orrery_protocol state-replication datagram. It accepts
 * the ordinary/compressed keyframe formats and keyframe-anchored deltas.
 * Feed the inner orrery_protocol datagram, after any transport-specific outer
 * envelope has been stripped. */
orrery_sim_result orrery_sim_apply_replication(
    orrery_sim *sim, const uint8_t *replication_bytes, size_t replication_len);

/* Reads the number of available craft transforms. */
orrery_sim_result orrery_sim_craft_transform_count(
    const orrery_sim *sim, size_t *out_count);

/* Copies all current craft transforms into the caller-owned contiguous array.
 * If capacity is too small, out_required receives the required record count
 * and no records are written. */
orrery_sim_result orrery_sim_copy_craft_transforms(
    const orrery_sim *sim,
    orrery_sim_craft_transform *out_transforms,
    size_t capacity,
    size_t *out_required);

/* Drains emitted events into a caller-owned flat byte buffer. Each record is
 * [source_entity: u64 little-endian][event_len: u32 little-endian]
 * [Regolith Outcome canonical bytes]. If capacity is too small, out_required
 * receives the required byte count and the event queue is retained. */
orrery_sim_result orrery_sim_drain_events(
    orrery_sim *sim, uint8_t *out_bytes, size_t capacity, size_t *out_required);

#ifdef __cplusplus
}
#endif

#endif /* ORRERY_SIM_H */
