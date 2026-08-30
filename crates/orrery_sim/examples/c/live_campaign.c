#include "orrery_sim.h"

#include <stdio.h>
#include <stdlib.h>

static int check(orrery_sim_result result, const char *operation) {
    if (result == ORRERY_SIM_OK) {
        return 1;
    }
    fprintf(stderr, "%s failed with result %d\n", operation, (int)result);
    return 0;
}

static unsigned char *read_packet(const char *path, size_t *out_len) {
    FILE *file = fopen(path, "rb");
    long file_len;
    unsigned char *bytes;

    if (file == NULL) {
        perror(path);
        return NULL;
    }
    if (fseek(file, 0L, SEEK_END) != 0 || (file_len = ftell(file)) < 0 ||
        fseek(file, 0L, SEEK_SET) != 0) {
        perror("reading replication packet");
        fclose(file);
        return NULL;
    }
    bytes = malloc((size_t)file_len);
    if (bytes == NULL && file_len != 0) {
        perror("allocating replication packet");
        fclose(file);
        return NULL;
    }
    if ((size_t)file_len != fread(bytes, 1, (size_t)file_len, file)) {
        perror("reading replication packet");
        free(bytes);
        fclose(file);
        return NULL;
    }
    fclose(file);
    *out_len = (size_t)file_len;
    return bytes;
}

int main(int argc, char **argv) {
    unsigned char *packet;
    size_t packet_len;
    size_t transform_count;
    size_t required;
    orrery_sim_craft_transform *transforms;
    orrery_sim *sim = NULL;

    if (argc != 2) {
        fprintf(stderr, "usage: %s REPLICATION_DATAGRAM\n", argv[0]);
        return EXIT_FAILURE;
    }
    packet = read_packet(argv[1], &packet_len);
    if (packet == NULL) {
        return EXIT_FAILURE;
    }
    if (!check(orrery_sim_create(&sim), "orrery_sim_create") ||
        !check(orrery_sim_apply_replication(sim, packet, packet_len),
               "orrery_sim_apply_replication") ||
        !check(orrery_sim_step(sim, 1), "orrery_sim_step") ||
        !check(orrery_sim_craft_transform_count(sim, &transform_count),
               "orrery_sim_craft_transform_count")) {
        free(packet);
        (void)orrery_sim_destroy(sim);
        return EXIT_FAILURE;
    }
    transforms = calloc(transform_count, sizeof(*transforms));
    if (transforms == NULL && transform_count != 0) {
        perror("allocating transforms");
        free(packet);
        (void)orrery_sim_destroy(sim);
        return EXIT_FAILURE;
    }
    if (!check(orrery_sim_copy_craft_transforms(sim, transforms, transform_count,
                                                &required),
               "orrery_sim_copy_craft_transforms")) {
        free(transforms);
        free(packet);
        (void)orrery_sim_destroy(sim);
        return EXIT_FAILURE;
    }
    for (size_t index = 0; index < required; ++index) {
        const orrery_sim_craft_transform *craft = &transforms[index];
        printf("craft id=%llu position_mm=(%lld, %lld, %lld) yaw_urad=%d pitch_urad=%d\n",
               (unsigned long long)craft->craft_id, (long long)craft->x_mm,
               (long long)craft->y_mm, (long long)craft->z_mm, craft->yaw_urad,
               craft->pitch_urad);
    }

    free(transforms);
    free(packet);
    return check(orrery_sim_destroy(sim), "orrery_sim_destroy") ? EXIT_SUCCESS
                                                               : EXIT_FAILURE;
}
