/*
 * A real C caller for the observer ABI — the shape an Unreal module has, with
 * the actors replaced by printf.
 *
 * It is the reference for `ObserverScenario.cpp`: the same four calls in the
 * same order, so a disagreement between this and the engine is a disagreement
 * about C++ and not about the crossing.
 *
 *   observer_consumer ADDR [FRAMES]
 *
 * Exits 0 when it observed at least one entity, 1 on any link or ABI problem.
 */

/* `-std=c11` alone hides `nanosleep`: it is POSIX, not ISO C. */
#define _POSIX_C_SOURCE 200809L

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "orrery_unreal_observer.h"

/* One frame of an engine that draws with printf. */
static int render(void *observer, unsigned long frame, int verbose)
{
    /* Ask for the size first, exactly as a renderer sizing its actor pool
     * would; then copy out. Two calls, one lock each, no allocation across
     * the boundary. */
    uint32_t required = 0;
    int32_t status = orrery_observer_snapshot(observer, NULL, 0, &required);
    if (status != ORRERY_OBSERVER_TOO_SMALL && status != ORRERY_OBSERVER_OK) {
        fprintf(stderr, "observer_consumer: snapshot size query failed: %d\n", status);
        return -1;
    }
    if (required == 0) {
        return 0;
    }

    OrreryObservedEntity *entities = calloc(required, sizeof(OrreryObservedEntity));
    if (entities == NULL) {
        fprintf(stderr, "observer_consumer: out of memory\n");
        return -1;
    }
    status = orrery_observer_snapshot(observer, entities, required, &required);
    if (status != ORRERY_OBSERVER_OK) {
        fprintf(stderr, "observer_consumer: snapshot failed: %d\n", status);
        free(entities);
        return -1;
    }

    if (verbose) {
        for (uint32_t index = 0; index < required; ++index) {
            const OrreryObservedEntity *entity = &entities[index];
            printf("frame=%lu id=%llu class=%s x=%lld tick=%llu basis=%llu..%llu@%u corrected=%u\n",
                   frame, (unsigned long long)entity->persist_id,
                   entity->timeline == ORRERY_OBSERVER_PREDICTED ? "predicted" : "interpolated",
                   (long long)entity->x_mm, (unsigned long long)entity->presented_at,
                   (unsigned long long)entity->basis_from, (unsigned long long)entity->basis_to,
                   (unsigned)entity->basis_alpha, (unsigned)entity->corrected);
        }
    }

    int count = (int)required;
    free(entities);
    return count;
}

int main(int argc, char **argv)
{
    if (argc < 2) {
        fprintf(stderr, "usage: observer_consumer ADDR [FRAMES]\n");
        return 2;
    }
    const char *addr = argv[1];
    unsigned long frames = argc > 2 ? strtoul(argv[2], NULL, 10) : 120UL;

    /* The archive and the header must agree before anything else happens. */
    if (orrery_observer_abi_version() != ORRERY_OBSERVER_ABI_VERSION) {
        fprintf(stderr, "observer_consumer: ABI %u, header %u\n",
                orrery_observer_abi_version(), ORRERY_OBSERVER_ABI_VERSION);
        return 1;
    }
    if (orrery_observer_entity_size() != (uint32_t)sizeof(OrreryObservedEntity)) {
        fprintf(stderr, "observer_consumer: record %u bytes, header %u\n",
                orrery_observer_entity_size(), (unsigned)sizeof(OrreryObservedEntity));
        return 1;
    }

    void *observer = orrery_observer_connect(addr);
    if (observer == NULL) {
        fprintf(stderr, "observer_consumer: cannot dial %s\n", addr);
        return 1;
    }
    printf("observer_consumer: watching %s\n", addr);
    fflush(stdout);

    int seen = 0;
    int ended = 0;
    for (unsigned long frame = 0; frame < frames; ++frame) {
        uint32_t applied = 0;
        int32_t status = orrery_observer_poll(observer, &applied);
        if (status == ORRERY_OBSERVER_LINK_CLOSED || status == ORRERY_OBSERVER_LINK_FAILED) {
            /* Draw the last frame anyway, then stop: a dead sidecar is not a
             * reason to blank the screen. */
            ended = status;
        } else if (status != ORRERY_OBSERVER_OK) {
            fprintf(stderr, "observer_consumer: poll failed: %d\n", status);
            orrery_observer_destroy(observer);
            return 1;
        }

        int drawn = render(observer, frame, frame % 30 == 0);
        if (drawn < 0) {
            orrery_observer_destroy(observer);
            return 1;
        }
        if (drawn > 0) {
            seen = 1;
        }
        if (ended != 0) {
            printf("observer_consumer: link ended with %d after %lu frames\n", ended, frame);
            break;
        }

        /* An engine's frame. 120 Hz, matching the sidecar's presentation rate. */
        struct timespec frame_time = {0, 8333333L};
        nanosleep(&frame_time, NULL);
    }

    orrery_observer_destroy(observer);
    if (!seen) {
        fprintf(stderr, "observer_consumer: never observed an entity\n");
        return 1;
    }
    printf("observer_consumer: done\n");
    return 0;
}
