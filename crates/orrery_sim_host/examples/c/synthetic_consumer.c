/*
 * A C consumer of the generic ABI, using nothing but orrery_sim_host.h and
 * the one factory symbol the synthetic reference library adds.
 *
 * The only ruleset-specific code here is `decode_state`, the C mirror of the
 * synthetic state's CoreCodec, and `encode_state`, its inverse. That is the
 * whole of what a game author writes per state type.
 *
 * Modes (argv[1]):
 *   loop            a variable-rate frame loop with a fixed-step accumulator,
 *                   jitter and a 250 ms hitch, driving exactly 120 ticks
 *   rewind          snapshot, step, restore, replay; self-checks equality
 *   events          step, then drain emitted events, decoding them in C;
 *                   also proves a too-small buffer drains nothing
 *   panic           a poisoned command; the panic must arrive as a code
 *   fixture PATH    restore a snapshot the Rust host wrote, step once, print
 */

#include "orrery_sim_host.h"

#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* orrery_core::TICK_NANOS: 1e9 / 60, integer division. */
#define TICK_NANOS 16666666ull
#define STATE_BYTES 64u
#define INPUT_IMPULSE 1u
#define INPUT_POISON 3u
#define EVENT_STRUCK 1u

typedef struct synthetic_state {
    int64_t position_um[3];
    int64_t velocity_um_per_tick[3];
    int32_t health;
    uint64_t target;
    uint32_t sightings;
} synthetic_state;

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

/* The C mirror of SyntheticState::decode. */
static int decode_state(const uint8_t *bytes, size_t len, synthetic_state *out) {
    if (len != STATE_BYTES) {
        return 0;
    }
    for (int axis = 0; axis < 3; ++axis) {
        out->position_um[axis] = (int64_t)read_u64(bytes + 8 * axis);
        out->velocity_um_per_tick[axis] = (int64_t)read_u64(bytes + 24 + 8 * axis);
    }
    out->health = (int32_t)read_u32(bytes + 48);
    out->target = read_u64(bytes + 52);
    out->sightings = read_u32(bytes + 60);
    return 1;
}

/* The C mirror of SyntheticState::encode. */
static void encode_state(const synthetic_state *state, uint8_t out[STATE_BYTES]) {
    for (int axis = 0; axis < 3; ++axis) {
        write_u64(out + 8 * axis, (uint64_t)state->position_um[axis]);
        write_u64(out + 24 + 8 * axis, (uint64_t)state->velocity_um_per_tick[axis]);
    }
    write_u32(out + 48, (uint32_t)state->health);
    write_u64(out + 52, state->target);
    write_u32(out + 60, state->sightings);
}

static void print_state(uint64_t entity, const synthetic_state *state) {
    printf("entity=%" PRIu64 " pos=(%" PRId64 ",%" PRId64 ",%" PRId64
           ") vel=(%" PRId64 ",%" PRId64 ",%" PRId64
           ") health=%" PRId32 " target=%" PRIu64 " sightings=%" PRIu32 "\n",
           entity, state->position_um[0], state->position_um[1],
           state->position_um[2], state->velocity_um_per_tick[0],
           state->velocity_um_per_tick[1], state->velocity_um_per_tick[2],
           state->health, state->target, state->sightings);
}

static int check(orrery_host_result result, const char *operation) {
    if (result == ORRERY_HOST_OK) {
        return 1;
    }
    fprintf(stderr, "%s failed with result %d\n", operation, (int)result);
    return 0;
}

/* Every factory a game exports has this shape; the header does not name it. */
orrery_host_result orrery_synthetic_host_create(
    const uint8_t *seed, uint64_t first_tick, orrery_host **out_host);

static orrery_host *create(uint64_t first_tick) {
    static const uint8_t seed[32] = {0};
    orrery_host *host = NULL;
    orrery_host_ruleset_identity id;
    if (orrery_host_abi_version() != ORRERY_SIM_HOST_ABI_VERSION) {
        fprintf(stderr, "abi version mismatch\n");
        return NULL;
    }
    if (!check(orrery_synthetic_host_create(seed, first_tick, &host),
               "orrery_synthetic_host_create")) {
        return NULL;
    }
    /* The decoder above was written against synthetic rules v1; refuse
     * anything else before decoding a byte. */
    if (!check(orrery_host_ruleset_id(host, &id), "orrery_host_ruleset_id") ||
        id.version != 1u || id.digest[0] != 0x5Au) {
        fprintf(stderr, "ruleset id mismatch\n");
        (void)orrery_host_destroy(host);
        return NULL;
    }
    return host;
}

static int install(orrery_host *host, uint64_t entity, uint64_t observed,
                   const synthetic_state *state) {
    uint8_t bytes[STATE_BYTES];
    encode_state(state, bytes);
    return check(orrery_host_install_state(host, entity, observed, bytes,
                                           sizeof bytes),
                 "orrery_host_install_state");
}

static int submit_impulse(orrery_host *host, uint64_t entity,
                          const int64_t delta[3]) {
    uint8_t command[8 + 1 + 24];
    write_u64(command, entity);
    command[8] = INPUT_IMPULSE;
    for (int axis = 0; axis < 3; ++axis) {
        write_u64(command + 9 + 8 * axis, (uint64_t)delta[axis]);
    }
    return check(orrery_host_submit_command(host, command, sizeof command),
                 "orrery_host_submit_command");
}

/* Grows a buffer until a (out, capacity, required) call fits. */
typedef orrery_host_result (*byte_call)(orrery_host *, uint8_t *, size_t,
                                        size_t *);

static uint8_t *fetch_bytes(orrery_host *host, byte_call call, const char *name,
                            size_t *out_len) {
    size_t required = 0;
    uint8_t *buffer = NULL;
    orrery_host_result result = call(host, NULL, 0, &required);
    if (result != ORRERY_HOST_OK && result != ORRERY_HOST_BUFFER_TOO_SMALL) {
        check(result, name);
        return NULL;
    }
    buffer = malloc(required == 0 ? 1 : required);
    if (buffer == NULL) {
        perror(name);
        return NULL;
    }
    if (!check(call(host, buffer, required, &required), name)) {
        free(buffer);
        return NULL;
    }
    *out_len = required;
    return buffer;
}

static orrery_host_result collect_states(orrery_host *host, uint8_t *out,
                                         size_t capacity, size_t *required) {
    return orrery_host_collect_states(host, out, capacity, required);
}

static orrery_host_result snapshot(orrery_host *host, uint8_t *out,
                                   size_t capacity, size_t *required) {
    return orrery_host_snapshot(host, out, capacity, required);
}

static orrery_host_state_hash *drain_hashes(orrery_host *host, size_t *out_count) {
    size_t required = 0;
    orrery_host_state_hash *hashes;
    orrery_host_result result = orrery_host_drain_state_hashes(host, NULL, 0, &required);
    if (result != ORRERY_HOST_OK && result != ORRERY_HOST_BUFFER_TOO_SMALL) {
        check(result, "orrery_host_drain_state_hashes");
        return NULL;
    }
    hashes = calloc(required == 0 ? 1 : required, sizeof *hashes);
    if (hashes == NULL) {
        perror("hashes");
        return NULL;
    }
    if (!check(orrery_host_drain_state_hashes(host, hashes, required, &required),
               "orrery_host_drain_state_hashes")) {
        free(hashes);
        return NULL;
    }
    *out_count = required;
    return hashes;
}

static int print_states(const uint8_t *records, size_t len) {
    size_t at = 0;
    while (at < len) {
        uint64_t entity;
        uint32_t length;
        synthetic_state state;
        if (len - at < 12) {
            fprintf(stderr, "truncated state record header\n");
            return 0;
        }
        entity = read_u64(records + at);
        length = read_u32(records + at + 8);
        at += 12;
        if (len - at < length || !decode_state(records + at, length, &state)) {
            fprintf(stderr, "malformed state record\n");
            return 0;
        }
        at += length;
        print_state(entity, &state);
    }
    return 1;
}

static void print_hex(const uint8_t *bytes, size_t len) {
    for (size_t i = 0; i < len; ++i) {
        printf("%02x", bytes[i]);
    }
}

/* ── loop ─────────────────────────────────────────────────────────────── */

static int run_loop(void) {
    orrery_host *host = create(0);
    synthetic_state mover = {{0, 0, 0}, {1234, -567, 89}, 100, 0, 0};
    const int64_t impulse[3] = {10, 0, 0};
    uint64_t accumulator = 0;
    uint64_t total_ticks = 0;
    uint64_t next_tick = 0;
    size_t frames = 0;
    size_t hash_count = 0;
    size_t states_len = 0;
    uint8_t *states;
    orrery_host_state_hash *hashes;

    if (host == NULL || !install(host, 7, 0, &mover) ||
        !submit_impulse(host, 7, impulse)) {
        return EXIT_FAILURE;
    }

    /* A deterministic frame schedule: 30 steady frames, 30 jittered frames,
     * one 250 ms hitch, then steady until exactly 120 ticks were issued. The
     * accumulator is the caller's, as the host has no clock. */
    while (total_ticks < 120) {
        uint64_t dt;
        uint64_t ticks;
        if (frames < 30) {
            dt = TICK_NANOS;
        } else if (frames < 60) {
            dt = (frames % 2 == 0) ? 12000000ull : 21000000ull;
        } else if (frames == 60) {
            dt = 250000000ull;
        } else {
            dt = TICK_NANOS;
        }
        ++frames;
        accumulator += dt;
        ticks = accumulator / TICK_NANOS;
        accumulator %= TICK_NANOS;
        if (ticks > 120 - total_ticks) {
            ticks = 120 - total_ticks;
        }
        if (!check(orrery_host_step(host, ticks, NULL, &next_tick),
                   "orrery_host_step")) {
            return EXIT_FAILURE;
        }
        total_ticks += ticks;
    }

    printf("frames=%zu ticks=%" PRIu64 " next_tick=%" PRIu64 "\n", frames,
           total_ticks, next_tick);
    states = fetch_bytes(host, collect_states, "orrery_host_collect_states",
                         &states_len);
    hashes = drain_hashes(host, &hash_count);
    if (states == NULL || hashes == NULL || !print_states(states, states_len)) {
        return EXIT_FAILURE;
    }
    printf("hashes=%zu last_tick=%" PRIu64 " last_hash=", hash_count,
           hashes[hash_count - 1].tick);
    print_hex(hashes[hash_count - 1].hash, 32);
    printf("\n");
    free(states);
    free(hashes);
    return check(orrery_host_destroy(host), "orrery_host_destroy")
               ? EXIT_SUCCESS
               : EXIT_FAILURE;
}

/* ── rewind ───────────────────────────────────────────────────────────── */

static int hashes_equal(const orrery_host_state_hash *a, size_t a_len,
                        const orrery_host_state_hash *b, size_t b_len) {
    if (a_len != b_len) {
        return 0;
    }
    for (size_t i = 0; i < a_len; ++i) {
        if (a[i].entity != b[i].entity || a[i].tick != b[i].tick ||
            memcmp(a[i].hash, b[i].hash, 32) != 0) {
            return 0;
        }
    }
    return 1;
}

static int run_rewind(void) {
    orrery_host *host = create(0);
    synthetic_state watcher = {{0, 0, 0}, {1000, 0, 0}, 100, 2, 0};
    synthetic_state watched = {{5000, 0, 0}, {0, 1000, 0}, 5, 1, 0};
    synthetic_state bystander = {{0, 0, 0}, {0, 0, 0}, 1, 0, 0};
    const int64_t impulse[3] = {0, 0, 333};
    uint8_t *snap;
    uint8_t *before;
    uint8_t *after_restore;
    uint8_t *first_run;
    uint8_t *second_run;
    size_t snap_len, before_len, after_restore_len, first_len, second_len;
    size_t first_hashes_len, second_hashes_len, discarded;
    orrery_host_state_hash *first_hashes;
    orrery_host_state_hash *second_hashes;
    orrery_host_state_hash *scratch;
    uint8_t probe[STATE_BYTES];
    size_t probe_len = 0;
    uint64_t next_tick = 0;
    int restore_exact, states_equal, hashes_match;

    if (host == NULL || !install(host, 1, 0, &watcher) ||
        !install(host, 2, 0, &watched) ||
        !check(orrery_host_step(host, 3, NULL, NULL), "orrery_host_step")) {
        return EXIT_FAILURE;
    }
    scratch = drain_hashes(host, &discarded);
    free(scratch);

    snap = fetch_bytes(host, snapshot, "orrery_host_snapshot", &snap_len);
    before = fetch_bytes(host, collect_states, "orrery_host_collect_states",
                         &before_len);
    if (snap == NULL || before == NULL) {
        return EXIT_FAILURE;
    }

    /* First run: an input, five ticks, an extra entity that must not survive
     * the restore. */
    if (!submit_impulse(host, 1, impulse) ||
        !check(orrery_host_step(host, 5, NULL, NULL), "orrery_host_step") ||
        !install(host, 3, 8, &bystander)) {
        return EXIT_FAILURE;
    }
    first_run = fetch_bytes(host, collect_states, "orrery_host_collect_states",
                            &first_len);
    first_hashes = drain_hashes(host, &first_hashes_len);

    if (!check(orrery_host_restore(host, snap, snap_len), "orrery_host_restore")) {
        return EXIT_FAILURE;
    }
    after_restore = fetch_bytes(host, collect_states,
                                "orrery_host_collect_states", &after_restore_len);
    if (first_run == NULL || first_hashes == NULL || after_restore == NULL) {
        return EXIT_FAILURE;
    }
    restore_exact = after_restore_len == before_len &&
                    memcmp(after_restore, before, before_len) == 0;
    printf("restore_exact=%d\n", restore_exact);
    printf("extra_entity_after_restore=%s\n",
           orrery_host_state(host, 3, probe, sizeof probe, &probe_len) ==
                   ORRERY_HOST_NOT_FOUND
               ? "not_found"
               : "present");
    if (!check(orrery_host_next_tick(host, &next_tick), "orrery_host_next_tick")) {
        return EXIT_FAILURE;
    }
    printf("next_tick_after_restore=%" PRIu64 "\n", next_tick);

    /* Second run: the same input history replayed from the restored point.
     * The bystander is not reinstalled; the first run's states are compared
     * without it. */
    if (!submit_impulse(host, 1, impulse) ||
        !check(orrery_host_step(host, 5, NULL, NULL), "orrery_host_step")) {
        return EXIT_FAILURE;
    }
    second_run = fetch_bytes(host, collect_states, "orrery_host_collect_states",
                             &second_len);
    second_hashes = drain_hashes(host, &second_hashes_len);
    if (second_run == NULL || second_hashes == NULL) {
        return EXIT_FAILURE;
    }
    /* The first run's records end with the bystander's record; strip it. */
    states_equal = first_len == second_len + 12 + STATE_BYTES &&
                   memcmp(first_run, second_run, second_len) == 0;
    hashes_match = hashes_equal(first_hashes, first_hashes_len, second_hashes,
                                second_hashes_len);
    printf("replay_states_equal=%d\n", states_equal);
    printf("replay_hashes_equal=%d hashes=%zu\n", hashes_match, second_hashes_len);
    if (!print_states(second_run, second_len)) {
        return EXIT_FAILURE;
    }

    free(snap);
    free(before);
    free(after_restore);
    free(first_run);
    free(second_run);
    free(first_hashes);
    free(second_hashes);
    if (!check(orrery_host_destroy(host), "orrery_host_destroy")) {
        return EXIT_FAILURE;
    }
    return (restore_exact && states_equal && hashes_match) ? EXIT_SUCCESS
                                                           : EXIT_FAILURE;
}

/* ── events ───────────────────────────────────────────────────────────── */

/* The C mirror of the synthetic event's CoreCodec, the second and last piece
 * of ruleset-specific code a consumer writes. */
typedef struct struck_event {
    uint64_t source;
    uint64_t target;
    int32_t damage;
} struck_event;

static int decode_struck(const uint8_t *bytes, size_t len, struck_event *out) {
    if (len != 13u || bytes[0] != EVENT_STRUCK) {
        return 0;
    }
    out->target = read_u64(bytes + 1);
    out->damage = (int32_t)read_u32(bytes + 9);
    return 1;
}

static orrery_host_result drain_events(orrery_host *host, uint8_t *out,
                                       size_t capacity, size_t *required) {
    return orrery_host_drain_events(host, out, capacity, required);
}

static int print_events(const uint8_t *records, size_t len) {
    size_t at = 0;
    size_t count = 0;
    struck_event decoded[64];
    while (at < len) {
        uint64_t source;
        uint32_t length;
        if (len - at < 12u || count == sizeof decoded / sizeof *decoded) {
            fprintf(stderr, "truncated or overlong event record stream\n");
            return 0;
        }
        source = read_u64(records + at);
        length = read_u32(records + at + 8);
        at += 12;
        if (len - at < length ||
            !decode_struck(records + at, length, &decoded[count])) {
            fprintf(stderr, "malformed event record\n");
            return 0;
        }
        decoded[count].source = source;
        at += length;
        ++count;
    }
    printf("events=%zu\n", count);
    for (size_t i = 0; i < count; ++i) {
        printf("event source=%" PRIu64 " target=%" PRIu64 " damage=%" PRId32 "\n",
               decoded[i].source, decoded[i].target, decoded[i].damage);
    }
    return 1;
}

static int run_events(void) {
    orrery_host *host = create(0);
    synthetic_state watcher = {{0, 0, 0}, {1000, 0, 0}, 100, 2, 0};
    synthetic_state watched = {{5000, 0, 0}, {0, 1000, 0}, 100, 1, 0};
    uint8_t *events;
    size_t events_len = 0;
    size_t required = 0;
    size_t after = 0;

    if (host == NULL || !install(host, 1, 0, &watcher) ||
        !install(host, 2, 0, &watched) ||
        !check(orrery_host_step(host, 2, NULL, NULL), "orrery_host_step")) {
        return EXIT_FAILURE;
    }

    /* A buffer one byte short must drain nothing: the host encodes without
     * clearing, so the caller can size its copy and retry without losing
     * output it never received. */
    if (orrery_host_drain_events(host, NULL, 0, &required) !=
        ORRERY_HOST_BUFFER_TOO_SMALL) {
        fprintf(stderr, "a zero-capacity drain did not report the size\n");
        return EXIT_FAILURE;
    }
    printf("required=%zu\n", required);
    {
        uint8_t *narrow = malloc(required);
        if (narrow == NULL) {
            perror("narrow");
            return EXIT_FAILURE;
        }
        if (orrery_host_drain_events(host, narrow, required - 1, &required) !=
            ORRERY_HOST_BUFFER_TOO_SMALL) {
            fprintf(stderr, "a short drain did not refuse\n");
            free(narrow);
            return EXIT_FAILURE;
        }
        free(narrow);
    }

    events = fetch_bytes(host, drain_events, "orrery_host_drain_events",
                         &events_len);
    if (events == NULL || !print_events(events, events_len)) {
        free(events);
        return EXIT_FAILURE;
    }
    free(events);

    /* The successful drain emptied the buffer. */
    if (!check(orrery_host_drain_events(host, NULL, 0, &after),
               "orrery_host_drain_events")) {
        return EXIT_FAILURE;
    }
    printf("events_after_drain=%zu\n", after);
    return check(orrery_host_destroy(host), "orrery_host_destroy")
               ? EXIT_SUCCESS
               : EXIT_FAILURE;
}

/* ── panic ────────────────────────────────────────────────────────────── */

static int run_panic(void) {
    orrery_host *host = create(0);
    synthetic_state mover = {{0, 0, 0}, {0, 0, 0}, 1, 0, 0};
    uint8_t poison[9];
    uint64_t tick = 0;
    orrery_host_result step, after, destroy;

    if (host == NULL || !install(host, 7, 0, &mover)) {
        return EXIT_FAILURE;
    }
    write_u64(poison, 7);
    poison[8] = INPUT_POISON;
    if (!check(orrery_host_submit_command(host, poison, sizeof poison),
               "orrery_host_submit_command")) {
        return EXIT_FAILURE;
    }
    step = orrery_host_step(host, 1, NULL, NULL);
    after = orrery_host_next_tick(host, &tick);
    destroy = orrery_host_destroy(host);
    printf("step=%d after=%d destroy=%d\n", (int)step, (int)after, (int)destroy);
    return (step == ORRERY_HOST_PANIC && after == ORRERY_HOST_POISONED &&
            destroy == ORRERY_HOST_OK)
               ? EXIT_SUCCESS
               : EXIT_FAILURE;
}

/* ── fixture ──────────────────────────────────────────────────────────── */

static uint8_t *read_file(const char *path, size_t *out_len) {
    FILE *file = fopen(path, "rb");
    long file_len;
    uint8_t *bytes;
    if (file == NULL) {
        perror(path);
        return NULL;
    }
    if (fseek(file, 0L, SEEK_END) != 0 || (file_len = ftell(file)) < 0 ||
        fseek(file, 0L, SEEK_SET) != 0) {
        perror(path);
        fclose(file);
        return NULL;
    }
    bytes = malloc(file_len == 0 ? 1 : (size_t)file_len);
    if (bytes == NULL || (size_t)file_len != fread(bytes, 1, (size_t)file_len, file)) {
        perror(path);
        free(bytes);
        fclose(file);
        return NULL;
    }
    fclose(file);
    *out_len = (size_t)file_len;
    return bytes;
}

static int run_fixture(const char *path) {
    size_t len = 0;
    uint8_t *bytes = read_file(path, &len);
    orrery_host *host = create(0);
    uint64_t next_tick = 0;
    uint8_t *states;
    size_t states_len = 0;

    if (bytes == NULL || host == NULL ||
        !check(orrery_host_restore(host, bytes, len), "orrery_host_restore") ||
        !check(orrery_host_next_tick(host, &next_tick), "orrery_host_next_tick")) {
        return EXIT_FAILURE;
    }
    printf("restored_next_tick=%" PRIu64 "\n", next_tick);
    if (!check(orrery_host_step(host, 1, NULL, &next_tick), "orrery_host_step")) {
        return EXIT_FAILURE;
    }
    printf("next_tick=%" PRIu64 "\n", next_tick);
    states = fetch_bytes(host, collect_states, "orrery_host_collect_states",
                         &states_len);
    if (states == NULL || !print_states(states, states_len)) {
        return EXIT_FAILURE;
    }
    free(states);
    free(bytes);
    return check(orrery_host_destroy(host), "orrery_host_destroy")
               ? EXIT_SUCCESS
               : EXIT_FAILURE;
}

int main(int argc, char **argv) {
    if (argc >= 2 && strcmp(argv[1], "loop") == 0) {
        return run_loop();
    }
    if (argc >= 2 && strcmp(argv[1], "rewind") == 0) {
        return run_rewind();
    }
    if (argc >= 2 && strcmp(argv[1], "events") == 0) {
        return run_events();
    }
    if (argc >= 2 && strcmp(argv[1], "panic") == 0) {
        return run_panic();
    }
    if (argc >= 3 && strcmp(argv[1], "fixture") == 0) {
        return run_fixture(argv[2]);
    }
    fprintf(stderr, "usage: %s loop|rewind|events|panic|fixture PATH\n", argv[0]);
    return EXIT_FAILURE;
}
