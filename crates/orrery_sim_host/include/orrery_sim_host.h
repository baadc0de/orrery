#ifndef ORRERY_SIM_HOST_H
#define ORRERY_SIM_HOST_H

/*
 * The ruleset-generic C ABI over orrery_sim_host::SimulationHost.
 *
 * This header names no ruleset, no state type and no game. Everything that
 * crosses it is an opaque handle, a fixed-width C record, or a caller-owned
 * contiguous byte buffer. A game's library adds exactly one symbol of its own,
 * a factory that produces an orrery_host for its ruleset, for example:
 *
 *   orrery_host_result my_game_host_create(
 *       const uint8_t seed[32], uint64_t first_tick, orrery_host **out_host);
 *
 * and everything after creation is this header.
 *
 * State crosses as the canonical bytes the kernel commits to, framed as
 * [entity: u64 LE][length: u32 LE][bytes]. The consumer decodes them with a
 * mirror of its own CoreCodec::decode - one function per state type, written
 * once, and the only per-component code this ABI asks for. Adding a field to
 * a state changes that function and the Rust codec; it never changes this
 * header. Check orrery_host_ruleset_id against the identity the decoder was
 * compiled for before decoding anything, so a drift fails at creation.
 *
 * Calls on one handle are serialized by the caller; concurrent calls are
 * unsupported. Every function catches Rust panics and reports them as
 * ORRERY_HOST_PANIC; after a panic inside a mutating call the handle is
 * poisoned and only orrery_host_destroy is accepted.
 *
 * Buffer convention: every function that returns bytes or records takes
 * (out, capacity, out_required). out_required always receives the size
 * needed. If capacity is too small the function returns
 * ORRERY_HOST_BUFFER_TOO_SMALL, writes nothing, and drains nothing; call
 * again with a larger buffer. Byte buffers are sized in bytes; record
 * buffers in records.
 */

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define ORRERY_SIM_HOST_ABI_VERSION 1u

/* The snapshot format version orrery_host_snapshot writes. */
#define ORRERY_SIM_HOST_SNAPSHOT_FORMAT_VERSION 1u

typedef struct orrery_host orrery_host;

typedef enum orrery_host_result {
    ORRERY_HOST_OK = 0,
    ORRERY_HOST_NULL_ARGUMENT = 1,
    ORRERY_HOST_BUFFER_TOO_SMALL = 2,
    ORRERY_HOST_MALFORMED_INPUT = 3,
    ORRERY_HOST_NOT_FOUND = 4,
    ORRERY_HOST_RECORD_TOO_LARGE = 5,
    ORRERY_HOST_POISONED = 6,
    ORRERY_HOST_PANIC = 7
} orrery_host_result;

typedef struct orrery_host_ruleset_identity {
    uint32_t version;
    uint8_t digest[32];
} orrery_host_ruleset_identity;

/* One state hash: blake3 over the entity's quantized canonical state after
 * the named tick executed. This is the value an authority's claim commits
 * to, so two runs that agree here agree on the bytes. */
typedef struct orrery_host_state_hash {
    uint64_t entity;
    uint64_t tick;
    uint8_t hash[32];
} orrery_host_state_hash;

/* The ABI version this library was built with. Compare with
 * ORRERY_SIM_HOST_ABI_VERSION before any other call. */
uint32_t orrery_host_abi_version(void);

/* The caller must stop using host after this returns. Accepted on a poisoned
 * handle. */
orrery_host_result orrery_host_destroy(orrery_host *host);

/* Reads the identity of the rules the host runs. */
orrery_host_result orrery_host_ruleset_id(
    const orrery_host *host, orrery_host_ruleset_identity *out_id);

/* Reads the absolute tick the next step will execute. */
orrery_host_result orrery_host_next_tick(
    const orrery_host *host, uint64_t *out_tick);

/* Queues one command for the next step. Its flat format is
 * [target entity: u64 LE][CoreInput canonical bytes]. A malformed command
 * is rejected whole and never enters the sealed input log. */
orrery_host_result orrery_host_submit_command(
    orrery_host *host, const uint8_t *bytes, size_t len);

/* Installs or replaces one entity's canonical state from its bytes, observed
 * at observed_tick. The host quantizes it before any tick reads it. Use 0 for
 * a fresh spawn; a state decoded from an authority's claim at tick T is
 * observed at T. */
orrery_host_result orrery_host_install_state(
    orrery_host *host,
    uint64_t entity,
    uint64_t observed_tick,
    const uint8_t *bytes,
    size_t len);

/* Removes one entity. ORRERY_HOST_NOT_FOUND if it was not installed. */
orrery_host_result orrery_host_remove_state(orrery_host *host, uint64_t entity);

/* Advances exactly ticks fixed ticks; the host never reads wall time. A
 * variable-rate loop keeps its accumulator outside and passes the count.
 * Either out-pointer may be NULL. The hashes each tick produced accumulate
 * for orrery_host_drain_state_hashes. */
orrery_host_result orrery_host_step(
    orrery_host *host,
    uint64_t ticks,
    uint64_t *out_first_tick,
    uint64_t *out_next_tick);

/* Drains accumulated state hashes, in execution order: tick ascending, then
 * entity ascending within a tick. capacity and out_required are in records. */
orrery_host_result orrery_host_drain_state_hashes(
    orrery_host *host,
    orrery_host_state_hash *out_hashes,
    size_t capacity,
    size_t *out_required);

/* Drains emitted events as [source entity: u64 LE][length: u32 LE]
 * [CoreEvent canonical bytes] records. */
orrery_host_result orrery_host_drain_events(
    orrery_host *host, uint8_t *out_bytes, size_t capacity, size_t *out_required);

/* Copies every entity's current canonical state, ascending by entity, as
 * [entity: u64 LE][length: u32 LE][CoreState canonical bytes] records.
 * Non-destructive; a renderer may read it after every frame. */
orrery_host_result orrery_host_collect_states(
    const orrery_host *host,
    uint8_t *out_bytes,
    size_t capacity,
    size_t *out_required);

/* Copies one entity's canonical state bytes. ORRERY_HOST_NOT_FOUND if it is
 * not installed. */
orrery_host_result orrery_host_state(
    const orrery_host *host,
    uint64_t entity,
    uint8_t *out_bytes,
    size_t capacity,
    size_t *out_required);

/* Copies a rewind point: the host's clock, every installed entity as its own
 * record, and the inputs queued for the next tick. Little-endian throughout:
 *   [format version: u32][ruleset version: u32][ruleset digest: 32 bytes]
 *   [next tick: u64][entity count: u64]
 *   then per entity, ascending:
 *   [entity: u64][observed tick: u64][length: u32][CoreState canonical bytes]
 *   then [recipient count: u64] and per recipient, ascending:
 *   [entity: u64][input count: u32] followed by that many
 *   [length: u32][CoreInput canonical bytes] in submission order.
 * Queued inputs travel with the point because the host produces some of them
 * itself (an event on tick T becomes an input for T + 1 through the game's
 * adapter). A consumer replaying its own input history after a restore
 * replays only what it submitted after the snapshot. Undrained events are
 * not part of a snapshot; they stay in the drain buffer. */
orrery_host_result orrery_host_snapshot(
    const orrery_host *host,
    uint8_t *out_bytes,
    size_t capacity,
    size_t *out_required);

/* Restores a snapshot, all or nothing. Afterwards the host holds exactly the
 * snapshot's entities - any installed since are removed - with their
 * snapshotted bytes and observation ticks, the next tick and the queued
 * inputs are the snapshot's (anything queued since is dropped, as are
 * undrained state hashes), and stepping with the same inputs submitted after
 * the snapshot reproduces the same state hashes and the same state bytes as
 * the original run. A malformed buffer, or one taken under another ruleset,
 * is ORRERY_HOST_MALFORMED_INPUT and the host is untouched. */
orrery_host_result orrery_host_restore(
    orrery_host *host, const uint8_t *bytes, size_t len);

#ifdef __cplusplus
}
#endif

#endif /* ORRERY_SIM_HOST_H */
