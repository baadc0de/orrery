/*
 * orrery_unreal_observer — the engine side of the sidecar IPC crossing.
 *
 * Link `liborrery_unreal_observer.a`, include this header, and move actors
 * from the records `orrery_observer_snapshot` copies out. The engine never
 * decodes a frame, never opens a socket, and never blocks its game thread on
 * either.
 *
 * Hand-written and kept in step with `src/lib.rs` by `tests/c_consumer.rs`,
 * which compiles a real C program against a real archive and checks the
 * record size the archive reports against the one this header declares. There
 * is no `cbindgen` in this tree (ADR-0053 clause (c) item 7).
 *
 * THIS LINK IS ONE-DIRECTIONAL. There is deliberately no submit, no send and
 * no input symbol here. An engine holding this handle can render what the
 * ruleset asserted and cannot assert anything itself — ADR-0053 clause (f)
 * items 1 and 2, made a property of the surface rather than a rule to
 * remember.
 *
 * THREADING. `orrery_observer_connect` starts one reader thread per handle;
 * every other function is called from the game thread and returns promptly.
 * A handle is not safe to use from two threads at once.
 */

#ifndef ORRERY_UNREAL_OBSERVER_H
#define ORRERY_UNREAL_OBSERVER_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Version of this ABI; compare against orrery_observer_abi_version(). */
#define ORRERY_OBSERVER_ABI_VERSION 1u

/* Status codes returned by poll and snapshot. */
#define ORRERY_OBSERVER_OK 0
#define ORRERY_OBSERVER_BAD_ARGUMENT 1
#define ORRERY_OBSERVER_LINK_CLOSED 2
#define ORRERY_OBSERVER_LINK_FAILED 3
#define ORRERY_OBSERVER_TOO_SMALL 4
#define ORRERY_OBSERVER_PANIC 5
#define ORRERY_OBSERVER_POISONED 6

/* Values of OrreryObservedEntity::timeline. */
#define ORRERY_OBSERVER_PREDICTED 0u
#define ORRERY_OBSERVER_INTERPOLATED 1u

/*
 * One presented entity.
 *
 * Field order is widest-first and the record is exactly 80 bytes with
 * alignment 8, so it carries no padding under any compiler that implements
 * the natural alignment rules. `orrery_observer_entity_size()` reports what
 * the archive believes; assert it against `sizeof` before the first frame.
 *
 * Translation is grid-relative millimetres on the protocol's lattice.
 * Orientation is the protocol's signed-int16 direction quantization: only the
 * direction of `forward` and `up` carries meaning, not their magnitude, so
 * normalise before building a rotation.
 *
 * `basis_from == basis_to` is an exact sample — a predicted entity, presented
 * at the tick the rules produced it. `basis_from != basis_to` is an
 * interpolated one, rendered `basis_alpha / 65535` of the way between two
 * confirmed snapshots. Carry the pair unchanged into any later hit claim.
 */
typedef struct OrreryObservedEntity {
    uint64_t persist_id;
    int64_t x_mm;
    int64_t y_mm;
    int64_t z_mm;
    uint64_t presented_at;
    uint64_t basis_from;
    uint64_t basis_to;
    uint64_t corrected_at;
    int16_t forward_x;
    int16_t forward_y;
    int16_t forward_z;
    int16_t up_x;
    int16_t up_y;
    int16_t up_z;
    uint16_t basis_alpha;
    uint8_t timeline;
    uint8_t corrected;
} OrreryObservedEntity;

/* The ABI version this archive was built with. */
uint32_t orrery_observer_abi_version(void);

/* sizeof(OrreryObservedEntity) as the archive sees it. */
uint32_t orrery_observer_entity_size(void);

/*
 * Dial a serving sidecar, e.g. "127.0.0.1:7899". Returns NULL when `addr` is
 * NULL, is not valid UTF-8, or cannot be dialled. Release with
 * orrery_observer_destroy.
 */
void *orrery_observer_connect(const char *addr);

/*
 * Take up whatever arrived since the last call. Writes the number of newly
 * applied messages to `out_applied` when it is non-NULL — pass NULL to ignore
 * it. Returns OK while the link is live, LINK_CLOSED after a clean end of
 * stream, LINK_FAILED after a failure. In both terminal cases the last
 * snapshot stays readable: the last thing the sidecar presented is still the
 * best thing to draw.
 */
int32_t orrery_observer_poll(void *handle, uint32_t *out_applied);

/*
 * Copy the whole presentation set into `out`.
 *
 * `*out_required` is written with the number of entities presented whenever
 * `out_required` is non-NULL, including when the buffer is too small — so the
 * supported way to ask for the size alone is (NULL, 0, &required), which
 * answers TOO_SMALL with the size filled in. When `capacity >= *out_required`
 * that many records are written and the call answers OK. Nothing is written
 * on TOO_SMALL.
 *
 * The set is copied under one lock, so the array is one consistent
 * presentation set rather than a torn read of two.
 */
int32_t orrery_observer_snapshot(void *handle, OrreryObservedEntity *out, uint32_t capacity,
                                 uint32_t *out_required);

/* Release the handle. Accepted on a poisoned handle; a no-op on NULL. */
void orrery_observer_destroy(void *handle);

#ifdef __cplusplus
}
#endif

#endif /* ORRERY_UNREAL_OBSERVER_H */
