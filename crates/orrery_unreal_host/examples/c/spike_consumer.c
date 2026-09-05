/*
 * The C consumer of spike #1043's staticlib: links liborrery_unreal_host.a
 * (orrery_unreal_host.lib on windows-msvc), creates both handles, drives them
 * from its own fixed-step loop, and measures the predicted-tick latency the
 * way #920's observer measures ipc_added
 * (crates/orrery_ipc_transport/src/bench.rs, observer_game and phase_columns),
 * writing a report in the orrery-ipc-harness/1 schema that
 * scripts/ipc-report.py renders.
 *
 * Linux and Windows. The Windows leg is #1084: #920's bands are defined at
 * N = 24 on Windows, so the in-process number has to be taken there too, and
 * the nightly `inproc-ipc-windows` job takes it through spike-windows.sh.
 * Every platform difference is behind `#ifdef _WIN32` below and tabulated in
 * the crate README.
 *
 * Modes (argv[1]):
 *   bench      the measurement. Options:
 *                --entities N      (24)     craft installed; N=24 is #920's headline
 *                --ticks N         (36000)  sampled ticks; 10 min at 60 Hz
 *                --warmup N        (600)    ticks before sampling
 *                --clock manual|auto        who advances Bevy's clock (see header)
 *                --no-app                   control: the host alone, no App
 *                --idle-seconds S  (3)      length of each idle CPU window
 *                --time-period              Windows only: timeBeginPeriod(1)
 *                --report PATH              write the JSON report
 *                --note TEXT                appended to the report's notes (repeatable)
 *   smoke      bench at 120 ticks, no warmup, no idle windows; prints one line
 *   panic      a system panic inside App::update must arrive as a code
 *   threadhop  create the App on this thread, update it from another
 *
 * What is measured per tick, in order, on the one thread that owns the loop:
 *   App::update()                          app_update      (baseline column)
 *   honest orders for the remote craft     remote_inputs   (baseline column)
 *   t0  local input handed to the host   } hop_in    = t1 - t0   (the ABI call: decode + queue)
 *   t1  submit returned                  }
 *   t2  orrery_host_step returned          extract   = t2 - t1   (the tick, and its state hashes)
 *   t3  orrery_host_collect_states done    encode    = t3 - t2   (canonical bytes copied out)
 *   t4' one clock read later               hop_out   = t4' - t3  (there is no hop; this is its absence)
 *   td  every record decoded in C          decode_out = td - t4'
 *   ta  mirror actors written              phase     = ta - t4', phase_after_decode = ta - td
 *                                          ipc_added = td - t0   (here: inproc_added)
 *   drains                                 drains          (baseline column)
 *
 * #920's phase is the wait for the next engine tick, on which the observer
 * applies a frame that arrived mid-interval. In-process the mirror is applied
 * in the frame that produced it, so the wait is zero by construction and the
 * column collapses to the apply cost. It is recorded, not assumed.
 */

#ifdef _WIN32
/* GetThreadDescription is Windows 8+; the hosted runners are Server 2022. */
#ifndef _WIN32_WINNT
#define _WIN32_WINNT 0x0A00
#endif
#else
#define _POSIX_C_SOURCE 200809L
#define _DEFAULT_SOURCE
#endif

#include "orrery_unreal_host.h"

#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#include <windows.h>

#include <tlhelp32.h>
/* timeBeginPeriod lives in winmm, which rustc's `--print native-static-libs`
 * does not list: nothing in the Rust half calls it. The link line adds winmm
 * explicitly (`spike-windows.sh`). */
#include <timeapi.h>
#else
#include <dirent.h>
#include <pthread.h>
#include <sys/resource.h>
#include <time.h>
#include <unistd.h>
#endif

#define TICK_HZ 60u
#define HITCH_NS 16700000ull
#define MAX_NOTES 32
#define MAX_THREAD_NAMES 64
#define COMM_LEN 32

#ifdef _WIN32
#define PLATFORM_NAME "windows"
#define CLOCK_NAME "QueryPerformanceCounter"
#else
#define PLATFORM_NAME "linux"
#define CLOCK_NAME "CLOCK_MONOTONIC"
#endif

/* ---- clock and pacing, matching orrery_ipc_transport ---------------------- */

/* The two clocks orrery_ipc_transport::monotonic_now_ns reads
 * (crates/orrery_ipc_transport/src/lib.rs:311-368), read here the same way:
 * CLOCK_MONOTONIC on Unix, QueryPerformanceCounter on Windows. A failed clock
 * read stops the measurement rather than silently zeroing a timestamp. */
#ifdef _WIN32
static uint64_t now_ns(void) {
    static LONGLONG frequency = 0;
    LARGE_INTEGER value;
    if (frequency == 0) {
        if (!QueryPerformanceFrequency(&value) || value.QuadPart <= 0) {
            fprintf(stderr, "QueryPerformanceFrequency failed\n");
            exit(2);
        }
        frequency = value.QuadPart;
    }
    if (!QueryPerformanceCounter(&value)) {
        fprintf(stderr, "QueryPerformanceCounter failed\n");
        exit(2);
    }
    /* QPC ticks are not nanoseconds. Rust scales through u128; the split
     * below is the same value without a wide type, exact for every QPC
     * reading: ticks = q*f + r with r < f, so ns = q*1e9 + (r*1e9)/f, and
     * r*1e9 stays under 2^63 for any real frequency (f < 2^33). */
    uint64_t ticks = (uint64_t)value.QuadPart;
    uint64_t f = (uint64_t)frequency;
    return (ticks / f) * 1000000000ull + ((ticks % f) * 1000000000ull) / f;
}
#else
static uint64_t now_ns(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        fprintf(stderr, "clock_gettime(CLOCK_MONOTONIC) failed\n");
        exit(2);
    }
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}
#endif

/* sleep_until_ns from crates/orrery_ipc_transport/src/lib.rs: sleep in ~1 ms
 * quanta, spin the last ~1.5 ms. Pacing never produces a timestamp.
 *
 * On Windows the blocking wait is Sleep(), whose quantum is exactly what
 * timeBeginPeriod(1) changes -- 15.6 ms by default, ~1 ms raised. That is
 * #920 lie 1, and it is why this consumer is run both ways.
 *
 * Sleep() takes whole milliseconds, so the last sub-millisecond quantum
 * becomes Sleep(0) -- a yield rather than a wait. The spin window is then up
 * to ~1.5 ms rather than ~1.0 ms. It costs one core between ticks and it
 * produces no timestamp, so it cannot enter a measured column; it is recorded
 * here because it is a real difference from the Unix leg. */
static void sleep_until_ns(uint64_t deadline) {
    for (;;) {
        uint64_t now = now_ns();
        if (now >= deadline) {
            return;
        }
        uint64_t remaining = deadline - now;
        if (remaining <= 1500000ull) {
            continue;
        }
        uint64_t ns = remaining - 1000000ull;
#ifdef _WIN32
        Sleep((DWORD)(ns / 1000000ull));
#else
        struct timespec ts;
        ts.tv_sec = (time_t)(ns / 1000000000ull);
        ts.tv_nsec = (long)(ns % 1000000000ull);
        nanosleep(&ts, NULL);
#endif
    }
}

/* ---- timer resolution (#920 lie 1) --------------------------------------- */

/* Raised for the whole process and released on the way out, as
 * orrery_ipc_transport's TimerResolution guard does. The report's
 * `time_begin_period` is what this returned, never the flag that asked. */
#ifdef _WIN32
static int timer_resolution_raised = 0;

static int raise_timer_resolution(void) {
    if (timeBeginPeriod(1) != TIMERR_NOERROR) {
        return 0;
    }
    timer_resolution_raised = 1;
    return 1;
}

/* Paired with the successful raise above, and a no-op otherwise: an unmatched
 * timeEndPeriod would be a second process-wide edit nothing asked for. */
static void release_timer_resolution(void) {
    if (timer_resolution_raised) {
        (void)timeEndPeriod(1);
        timer_resolution_raised = 0;
    }
}
#else
/* There is no process-wide timer resolution to raise off Windows, so there is
 * no function to leave unused under -Werror either. */
#define release_timer_resolution() ((void)0)
#endif

/* ---- byte helpers ------------------------------------------------------- */

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

static uint16_t read_u16(const uint8_t *at) {
    return (uint16_t)(at[0] | ((uint16_t)at[1] << 8));
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

/* ---- the C mirror of Craft::decode (orrery_games/src/skirmish/state.rs) --- */

typedef struct craft {
    uint8_t archetype;
    int64_t pos[3];
    int64_t vel[3];
    int32_t yaw_urad;
    int32_t pitch_urad;
    int32_t hull;
    int32_t shield;
    uint16_t cooldown;
    uint32_t shots;
    uint64_t damage_dealt;
} craft;

static int decode_craft(const uint8_t *bytes, size_t len, craft *out) {
    if (len != ORRERY_SKIRMISH_CRAFT_BYTES) {
        return 0;
    }
    out->archetype = bytes[0];
    for (int axis = 0; axis < 3; ++axis) {
        out->pos[axis] = (int64_t)read_u64(bytes + 1 + 8 * axis);
        out->vel[axis] = (int64_t)read_u64(bytes + 25 + 8 * axis);
    }
    out->yaw_urad = (int32_t)read_u32(bytes + 49);
    out->pitch_urad = (int32_t)read_u32(bytes + 53);
    out->hull = (int32_t)read_u32(bytes + 57);
    out->shield = (int32_t)read_u32(bytes + 61);
    out->cooldown = read_u16(bytes + 65);
    out->shots = read_u32(bytes + 67);
    out->damage_dealt = read_u64(bytes + 71);
    return 1;
}

/* The mirror: what an actor's transform write would receive. */
typedef struct mirror_actor {
    uint64_t entity;
    int64_t pos[3];
    int32_t yaw_urad;
    int32_t pitch_urad;
    int32_t hull;
} mirror_actor;

/* The C mirror of Order::Thrust's encoding (skirmish/order.rs:117-126),
 * framed as the flat command orrery_host_submit_command takes. */
static size_t encode_thrust(uint8_t *out, uint64_t target, int32_t accel_mmss,
                            int32_t yaw_urad, int32_t pitch_urad) {
    write_u64(out, target);
    out[8] = 0;
    write_u32(out + 9, (uint32_t)accel_mmss);
    write_u32(out + 13, (uint32_t)yaw_urad);
    write_u32(out + 17, (uint32_t)pitch_urad);
    return 21;
}

/* ---- process observation ------------------------------------------------ */

typedef struct thread_name_count {
    char name[COMM_LEN];
    unsigned count;
} thread_name_count;

typedef struct thread_table {
    unsigned total;
    unsigned distinct;
    thread_name_count names[MAX_THREAD_NAMES];
} thread_table;

static void tally_thread(thread_table *table, const char *name) {
    table->total += 1;
    unsigned i;
    for (i = 0; i < table->distinct; ++i) {
        if (strcmp(table->names[i].name, name) == 0) {
            table->names[i].count += 1;
            return;
        }
    }
    if (table->distinct < MAX_THREAD_NAMES) {
        snprintf(table->names[i].name, COMM_LEN, "%s", name);
        table->names[i].count = 1;
        table->distinct += 1;
    }
}

/* Read from the OS (#1043: "thread counts are read from the OS"), never from
 * either engine's own claim of what it started.
 *
 * Windows has no /proc: the thread list comes from a Toolhelp snapshot
 * filtered to this process, and the names from GetThreadDescription, which is
 * the API Rust's std and Bevy's task pools set through SetThreadDescription.
 * A thread whose description was never set reads as the empty string -- that
 * is a fact about the thread, not a read failure, and it is recorded as one. */
#ifdef _WIN32
static void read_threads(thread_table *table) {
    memset(table, 0, sizeof *table);
    HANDLE snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
    if (snapshot == INVALID_HANDLE_VALUE) {
        return;
    }
    THREADENTRY32 entry;
    memset(&entry, 0, sizeof entry);
    entry.dwSize = sizeof entry;
    const DWORD self = GetCurrentProcessId();
    if (Thread32First(snapshot, &entry)) {
        do {
            if (entry.th32OwnerProcessID != self) {
                continue;
            }
            char name[COMM_LEN] = {0};
            HANDLE thread = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, FALSE, entry.th32ThreadID);
            if (thread != NULL) {
                PWSTR wide = NULL;
                if (SUCCEEDED(GetThreadDescription(thread, &wide)) && wide != NULL) {
                    if (WideCharToMultiByte(CP_UTF8, 0, wide, -1, name, (int)sizeof name, NULL,
                                            NULL) == 0) {
                        name[0] = '\0';
                    }
                    LocalFree(wide);
                }
                CloseHandle(thread);
            }
            tally_thread(table, name);
        } while (Thread32Next(snapshot, &entry));
    }
    CloseHandle(snapshot);
}
#else
static void read_threads(thread_table *table) {
    memset(table, 0, sizeof *table);
    DIR *dir = opendir("/proc/self/task");
    if (dir == NULL) {
        return;
    }
    struct dirent *entry;
    while ((entry = readdir(dir)) != NULL) {
        if (entry->d_name[0] < '0' || entry->d_name[0] > '9') {
            continue;
        }
        char path[sizeof "/proc/self/task//comm" + sizeof entry->d_name];
        snprintf(path, sizeof path, "/proc/self/task/%s/comm", entry->d_name);
        FILE *comm = fopen(path, "r");
        if (comm == NULL) {
            continue;
        }
        char name[COMM_LEN] = {0};
        if (fgets(name, sizeof name, comm) != NULL) {
            name[strcspn(name, "\n")] = '\0';
        }
        fclose(comm);
        tally_thread(table, name);
    }
    closedir(dir);
}
#endif

/* Windows has no load average, so the three fields stay zero -- the same
 * zeros #920's own Windows sidecar report carries
 * (docs/data/sidecar-ipc-windows-2026-09-04-n24.json). They are an absence,
 * not a measurement of an idle machine. */
static void read_loadavg(double out[3]) {
    out[0] = out[1] = out[2] = 0.0;
#ifndef _WIN32
    FILE *f = fopen("/proc/loadavg", "r");
    if (f == NULL) {
        return;
    }
    if (fscanf(f, "%lf %lf %lf", &out[0], &out[1], &out[2]) != 3) {
        out[0] = out[1] = out[2] = 0.0;
    }
    fclose(f);
#endif
}

static uint64_t cpu_time_ns(void) {
#ifdef _WIN32
    FILETIME creation, exited, kernel, user;
    if (!GetProcessTimes(GetCurrentProcess(), &creation, &exited, &kernel, &user)) {
        return 0;
    }
    ULARGE_INTEGER k, u;
    k.LowPart = kernel.dwLowDateTime;
    k.HighPart = kernel.dwHighDateTime;
    u.LowPart = user.dwLowDateTime;
    u.HighPart = user.dwHighDateTime;
    /* FILETIME counts 100 ns intervals. */
    return (uint64_t)(k.QuadPart + u.QuadPart) * 100ull;
#else
    struct rusage usage;
    getrusage(RUSAGE_SELF, &usage);
    uint64_t user = (uint64_t)usage.ru_utime.tv_sec * 1000000000ull +
                    (uint64_t)usage.ru_utime.tv_usec * 1000ull;
    uint64_t sys = (uint64_t)usage.ru_stime.tv_sec * 1000000000ull +
                   (uint64_t)usage.ru_stime.tv_usec * 1000ull;
    return user + sys;
#endif
}

/* GetSystemInfo reports the calling thread's processor GROUP, so on a machine
 * with more than 64 logical processors this undercounts. The hosted runners
 * have four; a box that would trip it is not one this measurement runs on,
 * and the number is context in the report rather than an input to any
 * column. */
static long cores_online(void) {
#ifdef _WIN32
    SYSTEM_INFO info;
    GetSystemInfo(&info);
    return (long)info.dwNumberOfProcessors;
#else
    return sysconf(_SC_NPROCESSORS_ONLN);
#endif
}

/* Process CPU% over a paced idle window: the loop wakes at 60 Hz and does
 * nothing but (when app is non-null) App::update(). The difference between
 * the two windows is what the idle App costs the process. */
static double idle_cpu_pct(orrery_app *app, uint64_t dt_ns, double seconds) {
    uint64_t period = 1000000000ull / TICK_HZ;
    uint64_t frames = (uint64_t)(seconds * (double)TICK_HZ);
    uint64_t cpu0 = cpu_time_ns();
    uint64_t wall0 = now_ns();
    for (uint64_t i = 0; i < frames; ++i) {
        sleep_until_ns(wall0 + (i + 1) * period);
        if (app != NULL) {
            (void)orrery_app_update(app, dt_ns);
        }
    }
    uint64_t cpu = cpu_time_ns() - cpu0;
    uint64_t wall = now_ns() - wall0;
    return wall == 0 ? 0.0 : 100.0 * (double)cpu / (double)wall;
}

/* ---- summaries, matching bench.rs Summary::of ---------------------------- */

typedef struct summary {
    size_t n;
    double mean_ns;
    uint64_t min_ns, p50_ns, p99_ns, p99_9_ns, max_ns;
} summary;

static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a;
    uint64_t y = *(const uint64_t *)b;
    return x < y ? -1 : x > y ? 1 : 0;
}

/* Nearest rank over the sorted samples: index = ceil(pct/100 * n), clamped
 * to [1, n], minus one. */
static uint64_t rank(const uint64_t *sorted, size_t n, double pct) {
    double index = (pct / 100.0) * (double)n;
    double ceiled = (double)(uint64_t)index;
    if (ceiled < index) {
        ceiled += 1.0;
    }
    if (ceiled < 1.0) {
        ceiled = 1.0;
    }
    size_t i = (size_t)ceiled - 1;
    return sorted[i < n - 1 ? i : n - 1];
}

static summary summarize(uint64_t *samples, size_t n) {
    summary s;
    memset(&s, 0, sizeof s);
    if (n == 0) {
        return s;
    }
    qsort(samples, n, sizeof *samples, cmp_u64);
    double total = 0.0;
    for (size_t i = 0; i < n; ++i) {
        total += (double)samples[i];
    }
    s.n = n;
    s.mean_ns = total / (double)n;
    s.min_ns = samples[0];
    s.p50_ns = rank(samples, n, 50.0);
    s.p99_ns = rank(samples, n, 99.0);
    s.p99_9_ns = rank(samples, n, 99.9);
    s.max_ns = samples[n - 1];
    return s;
}

static void json_summary(FILE *f, const char *name, uint64_t *samples, size_t n, int last) {
    summary s = summarize(samples, n);
    fprintf(f,
            "    \"%s\": {\"n\": %zu, \"mean_ns\": %.6f, \"min_ns\": %" PRIu64
            ", \"p50_ns\": %" PRIu64 ", \"p99_ns\": %" PRIu64 ", \"p99_9_ns\": %" PRIu64
            ", \"max_ns\": %" PRIu64 "}%s\n",
            name, s.n, s.mean_ns, s.min_ns, s.p50_ns, s.p99_ns, s.p99_9_ns, s.max_ns,
            last ? "" : ",");
}

static void json_string(FILE *f, const char *text) {
    fputc('"', f);
    for (const char *c = text; *c != '\0'; ++c) {
        if (*c == '"' || *c == '\\') {
            fputc('\\', f);
            fputc(*c, f);
        } else if (*c == '\n') {
            fputs("\\n", f);
        } else {
            fputc(*c, f);
        }
    }
    fputc('"', f);
}

static void json_threads(FILE *f, const char *name, const thread_table *t, int last) {
    fprintf(f, "    \"%s\": {\"total\": %u, \"by_name\": {", name, t->total);
    for (unsigned i = 0; i < t->distinct; ++i) {
        json_string(f, t->names[i].name);
        fprintf(f, ": %u%s", t->names[i].count, i + 1 < t->distinct ? ", " : "");
    }
    fprintf(f, "}}%s\n", last ? "" : ",");
}

/* ---- the run ------------------------------------------------------------ */

typedef struct options {
    uint32_t entities;
    uint64_t ticks;
    uint64_t warmup;
    uint32_t clock;
    int with_app;
    int time_period;
    double idle_seconds;
    const char *report;
    const char *notes[MAX_NOTES];
    unsigned note_count;
} options;

typedef struct columns {
    uint64_t *hop_in, *extract, *encode, *hop_out, *decode_out, *phase, *phase_after_decode,
        *ipc_added, *app_update, *remote_inputs, *drains, *frame_total;
} columns;

static uint64_t *column(uint64_t n) {
    uint64_t *c = calloc((size_t)n, sizeof *c);
    if (c == NULL) {
        fprintf(stderr, "out of memory\n");
        exit(2);
    }
    return c;
}

static int check(orrery_host_result result, const char *operation) {
    if (result == ORRERY_HOST_OK) {
        return 1;
    }
    fprintf(stderr, "%s failed with result %d\n", operation, (int)result);
    return 0;
}

static orrery_host *create_host(const uint8_t seed[32], uint32_t entities) {
    orrery_host *host = NULL;
    orrery_host_ruleset_identity id;
    if (orrery_host_abi_version() != ORRERY_SIM_HOST_ABI_VERSION) {
        fprintf(stderr, "host abi version mismatch\n");
        return NULL;
    }
    if (!check(orrery_skirmish_host_create(seed, 0, &host), "orrery_skirmish_host_create")) {
        return NULL;
    }
    /* decode_craft was written against Skirmish rules v3; refuse anything
     * else before decoding a byte (orrery_sim_host.h:22-23). */
    if (!check(orrery_host_ruleset_id(host, &id), "orrery_host_ruleset_id") || id.version != 3u) {
        fprintf(stderr, "ruleset id mismatch: version %" PRIu32 "\n", id.version);
        (void)orrery_host_destroy(host);
        return NULL;
    }
    uint8_t state[ORRERY_SKIRMISH_CRAFT_BYTES];
    for (uint32_t slot = 0; slot < entities; ++slot) {
        uint64_t entity = (uint64_t)slot + 1;
        size_t required = 0;
        if (!check(orrery_skirmish_spawn_state(entity, slot, state, sizeof state, &required),
                   "orrery_skirmish_spawn_state") ||
            required != sizeof state ||
            !check(orrery_host_install_state(host, entity, 0, state, required),
                   "orrery_host_install_state")) {
            (void)orrery_host_destroy(host);
            return NULL;
        }
    }
    return host;
}

static int run_bench(const options *opt, int smoke) {
    static const uint8_t seed[32] = {0x43, 0x10};
    const uint64_t period = 1000000000ull / TICK_HZ;
    const uint64_t total = opt->warmup + opt->ticks;
    const uint32_t n = opt->entities;

    double loadavg_start[3], loadavg_end[3];
    read_loadavg(loadavg_start);

    thread_table threads_before, threads_after_app, threads_end;
    read_threads(&threads_before);
    double idle_without_app = 0.0, idle_with_app = 0.0;
    if (!smoke) {
        idle_without_app = idle_cpu_pct(NULL, 0, opt->idle_seconds);
    }

    orrery_app *app = NULL;
    uint64_t dt_ns = period;
    if (opt->with_app) {
        if (orrery_app_abi_version() != ORRERY_UNREAL_HOST_APP_ABI_VERSION) {
            fprintf(stderr, "app abi version mismatch\n");
            return 1;
        }
        if (!check(orrery_app_create(opt->clock, &app), "orrery_app_create")) {
            return 1;
        }
        orrery_app_timeline tl;
        if (!check(orrery_app_timeline_read(app, &tl), "orrery_app_timeline_read")) {
            return 1;
        }
        dt_ns = tl.fixed_step_ns;
    }
    read_threads(&threads_after_app);
    if (!smoke) {
        idle_with_app = idle_cpu_pct(app, dt_ns, opt->idle_seconds);
    }

    /* The App's counters where the measured loop starts: the idle window
     * above already ran updates (and, under the manual clock, absorbed
     * Bevy's zero-delta first frame), so drift is judged over the loop. */
    orrery_app_timeline tl_loop_start;
    memset(&tl_loop_start, 0, sizeof tl_loop_start);
    if (app != NULL &&
        !check(orrery_app_timeline_read(app, &tl_loop_start), "orrery_app_timeline_read")) {
        return 1;
    }

    orrery_host *host = create_host(seed, n);
    if (host == NULL) {
        return 1;
    }

    /* Buffers, sized once. */
    const size_t record_bytes = 8 + 4 + ORRERY_SKIRMISH_CRAFT_BYTES;
    size_t states_cap = record_bytes * n;
    uint8_t *states = malloc(states_cap);
    craft *crafts = calloc(n, sizeof *crafts);
    mirror_actor *actors = calloc(n, sizeof *actors);
    orrery_host_state_hash *hashes = calloc(n, sizeof *hashes);
    size_t events_cap = 1 << 16;
    uint8_t *events = malloc(events_cap);
    size_t commands_cap = 1 << 12;
    uint8_t *commands = malloc(commands_cap);
    uint64_t *peers = calloc(n, sizeof *peers);
    if (states == NULL || crafts == NULL || actors == NULL || hashes == NULL || events == NULL ||
        commands == NULL || peers == NULL) {
        fprintf(stderr, "out of memory\n");
        return 2;
    }

    columns c;
    c.hop_in = column(opt->ticks);
    c.extract = column(opt->ticks);
    c.encode = column(opt->ticks);
    c.hop_out = column(opt->ticks);
    c.decode_out = column(opt->ticks);
    c.phase = column(opt->ticks);
    c.phase_after_decode = column(opt->ticks);
    c.ipc_added = column(opt->ticks);
    c.app_update = column(opt->ticks);
    c.remote_inputs = column(opt->ticks);
    c.drains = column(opt->ticks);
    c.frame_total = column(opt->ticks);

    size_t samples = 0;
    uint64_t input_dropped = 0, step_failed = 0, app_update_failed = 0, tick_overruns = 0;
    uint64_t hitch_app_update = 0, hitch_host_path = 0, hitch_frame = 0;
    uint64_t events_total = 0, events_max_bytes = 0, decode_failures = 0;
    uint64_t checksum = 0;
    uint64_t host_tick = 0;
    if (!check(orrery_host_next_tick(host, &host_tick), "orrery_host_next_tick")) {
        return 1;
    }

    const uint64_t start = now_ns() + period;
    for (uint64_t tick = 0; tick < total; ++tick) {
        sleep_until_ns(start + tick * period);
        const int sampling = tick >= opt->warmup;
        const uint64_t t_frame = now_ns();

        /* 1. The net/prediction loop, once per fixed tick, on this thread. */
        uint64_t app_update_ns = 0;
        if (app != NULL) {
            uint64_t ta = now_ns();
            orrery_host_result r = orrery_app_update(app, dt_ns);
            app_update_ns = now_ns() - ta;
            if (r != ORRERY_HOST_OK) {
                app_update_failed += 1;
                fprintf(stderr, "orrery_app_update returned %d at tick %" PRIu64 "\n", (int)r,
                        tick);
                if (r == ORRERY_HOST_POISONED || r == ORRERY_HOST_PANIC) {
                    break;
                }
            }
        }

        /* 2. What the remote population asks for this tick: the honest pilot
         *    for every craft but the local one, through the generic submit. */
        uint64_t tr0 = now_ns();
        for (uint32_t slot = 1; slot < n; ++slot) {
            uint64_t entity = (uint64_t)slot + 1;
            size_t peer_count = 0;
            for (uint32_t other = 0; other < n; ++other) {
                if (other != slot) {
                    peers[peer_count++] = (uint64_t)other + 1;
                }
            }
            size_t required = 0;
            orrery_host_result r = orrery_skirmish_honest_commands(
                seed, entity, slot, host_tick, peers, peer_count, commands, commands_cap,
                &required);
            if (r != ORRERY_HOST_OK) {
                input_dropped += 1;
                continue;
            }
            size_t at = 0;
            while (at + 4 <= required) {
                uint32_t len = read_u32(commands + at);
                at += 4;
                if (orrery_host_submit_command(host, commands + at, len) != ORRERY_HOST_OK) {
                    input_dropped += 1;
                }
                at += len;
            }
        }
        uint64_t remote_inputs_ns = now_ns() - tr0;

        /* 3. The local player's input: one sample per tick, as #920's input
         *    batch carries one. t0 is the hand-over. */
        uint8_t thrust[21];
        size_t thrust_len = encode_thrust(thrust, 1, 3000, 5000, (tick % 7 == 0) ? 200 : -200);
        const uint64_t t0 = now_ns();
        orrery_host_result submit = orrery_host_submit_command(host, thrust, thrust_len);
        const uint64_t t1 = now_ns();
        if (submit != ORRERY_HOST_OK) {
            input_dropped += 1;
        }

        /* 4. The tick. */
        uint64_t first = 0, next = 0;
        orrery_host_result stepped = orrery_host_step(host, 1, &first, &next);
        const uint64_t t2 = now_ns();
        if (stepped != ORRERY_HOST_OK) {
            step_failed += 1;
            fprintf(stderr, "orrery_host_step returned %d at tick %" PRIu64 "\n", (int)stepped,
                    tick);
            if (stepped == ORRERY_HOST_POISONED || stepped == ORRERY_HOST_PANIC) {
                break;
            }
        }
        host_tick = next;

        /* 5. Canonical bytes out. */
        size_t states_len = 0;
        orrery_host_result collected =
            orrery_host_collect_states(host, states, states_cap, &states_len);
        const uint64_t t3 = now_ns();
        const uint64_t t_arrive = now_ns();
        int frame_ok = collected == ORRERY_HOST_OK;

        /* 6. Decode every record in C, then apply to the mirror. */
        uint32_t decoded = 0;
        if (frame_ok) {
            size_t at = 0;
            while (at + 12 <= states_len && decoded < n) {
                uint64_t entity = read_u64(states + at);
                uint32_t len = read_u32(states + at + 8);
                at += 12;
                if (!decode_craft(states + at, len, &crafts[decoded])) {
                    decode_failures += 1;
                    frame_ok = 0;
                    break;
                }
                actors[decoded].entity = entity;
                at += len;
                decoded += 1;
            }
        }
        const uint64_t t_decode = now_ns();
        if (frame_ok) {
            for (uint32_t i = 0; i < decoded; ++i) {
                actors[i].pos[0] = crafts[i].pos[0];
                actors[i].pos[1] = crafts[i].pos[1];
                actors[i].pos[2] = crafts[i].pos[2];
                actors[i].yaw_urad = crafts[i].yaw_urad;
                actors[i].pitch_urad = crafts[i].pitch_urad;
                actors[i].hull = crafts[i].hull;
                checksum = checksum * 0x100000001B3ull + actors[i].entity +
                           (uint64_t)actors[i].pos[0] + (uint64_t)crafts[i].shots;
            }
        }
        const uint64_t t_apply = now_ns();

        /* 7. Drains: hashes so the host's buffer does not grow, events so the
         *    adapter's routing is exercised and counted. */
        size_t hash_count = 0, event_bytes = 0;
        (void)orrery_host_drain_state_hashes(host, hashes, n, &hash_count);
        orrery_host_result drained = orrery_host_drain_events(host, events, events_cap, &event_bytes);
        if (drained == ORRERY_HOST_OK) {
            size_t at = 0;
            while (at + 12 <= event_bytes) {
                uint32_t len = read_u32(events + at + 8);
                at += 12 + len;
                events_total += 1;
            }
            if (event_bytes > events_max_bytes) {
                events_max_bytes = event_bytes;
            }
        }
        const uint64_t t_end = now_ns();

        if (app_update_ns > HITCH_NS) {
            hitch_app_update += 1;
        }
        if (t_apply - tr0 > HITCH_NS) {
            hitch_host_path += 1;
        }
        if (t_end - t_frame > HITCH_NS) {
            hitch_frame += 1;
        }

        if (sampling && frame_ok && stepped == ORRERY_HOST_OK && submit == ORRERY_HOST_OK) {
            c.hop_in[samples] = t1 - t0;
            c.extract[samples] = t2 - t1;
            c.encode[samples] = t3 - t2;
            c.hop_out[samples] = t_arrive - t3;
            c.decode_out[samples] = t_decode - t_arrive;
            c.phase[samples] = t_apply - t_arrive;
            c.phase_after_decode[samples] = t_apply - t_decode;
            c.ipc_added[samples] = t_decode - t0;
            c.app_update[samples] = app_update_ns;
            c.remote_inputs[samples] = remote_inputs_ns;
            c.drains[samples] = t_end - t_apply;
            c.frame_total[samples] = t_end - t_frame;
            samples += 1;
        }
        if (now_ns() > start + (tick + 1) * period) {
            tick_overruns += 1;
        }
    }
    const uint64_t finished = now_ns();
    const double duration_s = (double)(finished - (start + opt->warmup * period)) / 1e9;

    read_threads(&threads_end);
    read_loadavg(loadavg_end);

    orrery_app_timeline tl;
    memset(&tl, 0, sizeof tl);
    int timeline_ok = 0;
    if (app != NULL) {
        timeline_ok = orrery_app_timeline_read(app, &tl) == ORRERY_HOST_OK;
    }
    uint64_t host_next = 0;
    (void)orrery_host_next_tick(host, &host_next);

    if (smoke) {
        printf("ticks=%" PRIu64 " host_next_tick=%" PRIu64 " lightyear_tick=%" PRIu32
               " bridged_tick=%" PRIu64 " fixed_steps=%" PRIu64 " frames=%" PRIu32
               " events=%" PRIu64 " samples=%zu input_dropped=%" PRIu64
               " step_failed=%" PRIu64 " app_update_failed=%" PRIu64 " decode_failures=%" PRIu64
               " threads_before=%u threads_after_app=%u checksum=%" PRIx64 "\n",
               total, host_next, tl.lightyear_tick, tl.bridged_tick, tl.fixed_steps, tl.frames,
               events_total, samples, input_dropped, step_failed, app_update_failed,
               decode_failures, threads_before.total, threads_after_app.total, checksum);
    } else {
        summary ipc = summarize(c.ipc_added, samples);
        summary upd = summarize(c.app_update, samples);
        /* summarize sorts in place; the JSON writer below re-sorts, which is
         * idempotent. */
        printf("inproc_added p50 %.1f us, p99 %.1f us, p99.9 %.1f us, max %.1f us over %zu samples "
               "(N=%u); App::update p50 %.1f us p99 %.1f us; host next_tick %" PRIu64
               ", lightyear tick %" PRIu32 ", fixed_steps %" PRIu64 "; threads %u -> %u -> %u; "
               "idle CPU %.2f%% -> %.2f%%; overruns %" PRIu64 "\n",
               (double)ipc.p50_ns / 1000.0, (double)ipc.p99_ns / 1000.0,
               (double)ipc.p99_9_ns / 1000.0, (double)ipc.max_ns / 1000.0, samples, n,
               (double)upd.p50_ns / 1000.0, (double)upd.p99_ns / 1000.0, host_next,
               tl.lightyear_tick - tl_loop_start.lightyear_tick,
               tl.fixed_steps - tl_loop_start.fixed_steps, threads_before.total,
               threads_after_app.total,
               threads_end.total, idle_without_app, idle_with_app, tick_overruns);
    }

    if (opt->report != NULL) {
        FILE *f = fopen(opt->report, "w");
        if (f == NULL) {
            fprintf(stderr, "cannot write %s\n", opt->report);
            return 1;
        }
        fprintf(f, "{\n");
        fprintf(f, "  \"schema\": \"orrery-ipc-harness/1\",\n");
        fprintf(f, "  \"role\": \"observer\",\n");
        fprintf(f, "  \"platform\": \"%s\",\n", PLATFORM_NAME);
        fprintf(f, "  \"arch\": \"x86_64\",\n");
        fprintf(f, "  \"clock\": \"%s\",\n", CLOCK_NAME);
        fprintf(f, "  \"transport\": \"%s\",\n",
                opt->with_app ? "inproc-staticlib+app" : "inproc-staticlib-no-app");
        fprintf(f, "  \"measured_quantity\": \"inproc_added\",\n");
        /* Schema fields the renderer prints. There is no socket, so nothing
         * was set. `time_begin_period` is what raise_timer_resolution()
         * actually returned, not what --time-period asked for. */
        fprintf(f, "  \"tcp_nodelay\": false,\n");
        fprintf(f, "  \"time_begin_period\": %s,\n", opt->time_period ? "true" : "false");
        fprintf(f, "  \"entities\": %u,\n", n);
        fprintf(f, "  \"tick_hz\": %u,\n", TICK_HZ);
        fprintf(f, "  \"warmup_ticks\": %" PRIu64 ",\n", opt->warmup);
        fprintf(f, "  \"ticks\": %" PRIu64 ",\n", opt->ticks);
        fprintf(f, "  \"samples\": %zu,\n", samples);
        fprintf(f, "  \"duration_s\": %.9f,\n", duration_s);
        fprintf(f, "  \"loadavg_start\": [%.2f, %.2f, %.2f],\n", loadavg_start[0],
                loadavg_start[1], loadavg_start[2]);
        fprintf(f, "  \"loadavg_end\": [%.2f, %.2f, %.2f],\n", loadavg_end[0], loadavg_end[1],
                loadavg_end[2]);
        fprintf(f, "  \"phases_ns\": {\n");
        json_summary(f, "hop_in", c.hop_in, samples, 0);
        json_summary(f, "extract", c.extract, samples, 0);
        json_summary(f, "encode", c.encode, samples, 0);
        json_summary(f, "hop_out", c.hop_out, samples, 0);
        json_summary(f, "decode_out", c.decode_out, samples, 0);
        json_summary(f, "phase", c.phase, samples, 0);
        json_summary(f, "phase_after_decode", c.phase_after_decode, samples, 0);
        json_summary(f, "ipc_added", c.ipc_added, samples, 1);
        fprintf(f, "  },\n");
        fprintf(f, "  \"baselines_ns\": {\n");
        json_summary(f, "app_update", c.app_update, samples, 0);
        json_summary(f, "remote_inputs", c.remote_inputs, samples, 0);
        json_summary(f, "drains", c.drains, samples, 0);
        json_summary(f, "frame_total", c.frame_total, samples, 1);
        fprintf(f, "  },\n");
        fprintf(f, "  \"drops\": {\n");
        fprintf(f, "    \"input_dropped\": %" PRIu64 ",\n", input_dropped);
        fprintf(f, "    \"step_failed\": %" PRIu64 ",\n", step_failed);
        fprintf(f, "    \"app_update_failed\": %" PRIu64 ",\n", app_update_failed);
        fprintf(f, "    \"decode_failures\": %" PRIu64 ",\n", decode_failures);
        fprintf(f, "    \"tick_overruns\": %" PRIu64 "\n", tick_overruns);
        fprintf(f, "  },\n");
        fprintf(f, "  \"coexistence\": {\n");
        fprintf(f, "    \"app_present\": %s,\n", opt->with_app ? "true" : "false");
        fprintf(f, "    \"clock_mode\": \"%s\",\n",
                opt->with_app ? (opt->clock == ORRERY_APP_CLOCK_MANUAL ? "manual" : "automatic")
                              : "none");
        fprintf(f, "    \"cores_online\": %ld,\n", cores_online());
        json_threads(f, "threads_before_app", &threads_before, 0);
        json_threads(f, "threads_after_app_create", &threads_after_app, 0);
        json_threads(f, "threads_at_end", &threads_end, 0);
        fprintf(f, "    \"idle_cpu_pct_without_app\": %.4f,\n", idle_without_app);
        fprintf(f, "    \"idle_cpu_pct_with_app\": %.4f,\n", idle_with_app);
        fprintf(f, "    \"idle_window_s\": %.1f,\n", opt->idle_seconds);
        fprintf(f, "    \"hitches_over_16_7ms\": {\"app_update\": %" PRIu64
                   ", \"host_path\": %" PRIu64 ", \"frame\": %" PRIu64 "},\n",
                hitch_app_update, hitch_host_path, hitch_frame);
        fprintf(f, "    \"events_routed\": %" PRIu64 ",\n", events_total);
        fprintf(f, "    \"events_max_bytes_per_tick\": %" PRIu64 ",\n", events_max_bytes);
        fprintf(f, "    \"mirror_checksum\": \"%016" PRIx64 "\"\n", checksum);
        fprintf(f, "  },\n");
        fprintf(f, "  \"timeline\": {\n");
        fprintf(f, "    \"ticks_issued_by_c\": %" PRIu64 ",\n", total);
        fprintf(f, "    \"host_next_tick\": %" PRIu64 ",\n", host_next);
        if (timeline_ok) {
            fprintf(f, "    \"lightyear_tick\": %" PRIu32 ",\n", tl.lightyear_tick);
            fprintf(f, "    \"bridged_tick\": %" PRIu64 ",\n", tl.bridged_tick);
            fprintf(f, "    \"fixed_steps\": %" PRIu64 ",\n", tl.fixed_steps);
            fprintf(f, "    \"frames\": %" PRIu32 ",\n", tl.frames);
            fprintf(f, "    \"virtual_elapsed_ns\": %" PRIu64 ",\n", tl.virtual_elapsed_ns);
            fprintf(f, "    \"fixed_step_ns\": %" PRIu64 ",\n", tl.fixed_step_ns);
            fprintf(f, "    \"lightyear_ticks_during_loop\": %" PRIu32 ",\n",
                    tl.lightyear_tick - tl_loop_start.lightyear_tick);
            fprintf(f, "    \"fixed_steps_during_loop\": %" PRIu64 ",\n",
                    tl.fixed_steps - tl_loop_start.fixed_steps);
            fprintf(f, "    \"frames_during_loop\": %" PRIu32 ",\n",
                    tl.frames - tl_loop_start.frames);
            fprintf(f, "    \"drift_ticks_lightyear_minus_host\": %" PRId64 "\n",
                    (int64_t)(tl.lightyear_tick - tl_loop_start.lightyear_tick) -
                        (int64_t)total);
        } else {
            fprintf(f, "    \"app_timeline\": null\n");
        }
        fprintf(f, "  },\n");
        fprintf(f, "  \"notes\": [\n");
        const char *fixed_notes[] = {
            "ipc_added in this report is inproc_added (#1043): (t_decode - t0) with no hop; "
            "hop_in is the submit call (command decode + queue), extract is orrery_host_step, "
            "encode is orrery_host_collect_states, hop_out is one clock read, decode_out is the "
            "C-side decode of every record",
            "phase is the apply cost, not a tick wait: in-process the mirror is applied in the "
            "frame that produced it, so #920's wait-for-next-tick is zero by construction",
#ifdef _WIN32
            "Windows staticlib plus C consumer: the in-process half of #920's comparison on the "
            "platform its bands are defined on. The bands themselves are the SIDECAR's and are "
            "not applied here (scripts/ipc-report.py refuses a verdict on a non-sidecar "
            "transport); this is not the in-process Unreal number G10.2 turns on -- nothing here "
            "ran inside a UE process, beside UE's task graph, or drew a frame -- and it settles "
            "neither G10.2 nor D52/D53 (Proposed)",
#else
            "Linux staticlib plus C consumer on clang, informational: not the in-process Unreal "
            "number G10.2 turns on, and it settles neither G10.2 nor D52/D53 (Proposed)",
#endif
            "the App and the host are not connected (D53 section 5): App::update is the "
            "net/prediction loop's per-frame cost on the game thread, measured beside the host path",
            "the App prong of D53's fork (a full bevy_app::App beside the ABI handle). GD3's "
            "chosen configuration -- App prong, pool-capped, driver-connected -- is a third shape "
            "neither this spike nor #1052's non-App spike measures",
        };
        size_t fixed_count = sizeof fixed_notes / sizeof fixed_notes[0];
        for (size_t i = 0; i < fixed_count; ++i) {
            fprintf(f, "    ");
            json_string(f, fixed_notes[i]);
            fprintf(f, "%s\n", (i + 1 < fixed_count || opt->note_count > 0) ? "," : "");
        }
        for (unsigned i = 0; i < opt->note_count; ++i) {
            fprintf(f, "    ");
            json_string(f, opt->notes[i]);
            fprintf(f, "%s\n", i + 1 < opt->note_count ? "," : "");
        }
        fprintf(f, "  ]\n");
        fprintf(f, "}\n");
        fclose(f);
    }

    int rc = 0;
    if (!check(orrery_host_destroy(host), "orrery_host_destroy")) {
        rc = 1;
    }
    if (app != NULL && !check(orrery_app_destroy(app), "orrery_app_destroy")) {
        rc = 1;
    }
    free(states);
    free(crafts);
    free(actors);
    free(hashes);
    free(events);
    free(commands);
    free(peers);
    free(c.hop_in);
    free(c.extract);
    free(c.encode);
    free(c.hop_out);
    free(c.decode_out);
    free(c.phase);
    free(c.phase_after_decode);
    free(c.ipc_added);
    free(c.app_update);
    free(c.remote_inputs);
    free(c.drains);
    free(c.frame_total);
    return rc;
}

/* ---- panic containment on the App handle -------------------------------- */

static int run_panic(void) {
    orrery_app *app = NULL;
    if (!check(orrery_app_create(ORRERY_APP_CLOCK_MANUAL, &app), "orrery_app_create")) {
        return 1;
    }
    orrery_app_timeline tl;
    if (!check(orrery_app_timeline_read(app, &tl), "orrery_app_timeline_read") ||
        !check(orrery_app_update(app, tl.fixed_step_ns), "orrery_app_update") ||
        !check(orrery_app_request_panic(app), "orrery_app_request_panic")) {
        return 1;
    }
    orrery_host_result update = orrery_app_update(app, tl.fixed_step_ns);
    orrery_host_result after = orrery_app_update(app, tl.fixed_step_ns);
    orrery_host_result destroy = orrery_app_destroy(app);
    printf("update=%d after=%d destroy=%d\n", (int)update, (int)after, (int)destroy);
    return 0;
}

/* ---- update from a thread that did not create the App ------------------- */

typedef struct hop_args {
    orrery_app *app;
    uint64_t dt_ns;
    uint32_t on_creating_thread;
    orrery_host_result results[3];
} hop_args;

static void hop_body(hop_args *args) {
    args->on_creating_thread = orrery_app_on_creating_thread(args->app);
    for (int i = 0; i < 3; ++i) {
        args->results[i] = orrery_app_update(args->app, args->dt_ns);
    }
}

#ifdef _WIN32
static DWORD WINAPI hop_thread(LPVOID raw) {
    hop_body((hop_args *)raw);
    return 0;
}
#else
static void *hop_thread(void *raw) {
    hop_body(raw);
    return NULL;
}
#endif

/* Run `hop_body` on a thread that is not this one, and join it. */
static int run_off_thread(hop_args *args) {
#ifdef _WIN32
    HANDLE thread = CreateThread(NULL, 0, hop_thread, args, 0, NULL);
    if (thread == NULL) {
        fprintf(stderr, "CreateThread failed\n");
        return 0;
    }
    WaitForSingleObject(thread, INFINITE);
    CloseHandle(thread);
    return 1;
#else
    pthread_t thread;
    if (pthread_create(&thread, NULL, hop_thread, args) != 0) {
        fprintf(stderr, "pthread_create failed\n");
        return 0;
    }
    pthread_join(thread, NULL);
    return 1;
#endif
}

static int run_threadhop(void) {
    orrery_app *app = NULL;
    if (!check(orrery_app_create(ORRERY_APP_CLOCK_MANUAL, &app), "orrery_app_create")) {
        return 1;
    }
    orrery_app_timeline tl;
    if (!check(orrery_app_timeline_read(app, &tl), "orrery_app_timeline_read") ||
        !check(orrery_app_update(app, tl.fixed_step_ns), "orrery_app_update")) {
        return 1;
    }
    hop_args args;
    memset(&args, 0, sizeof args);
    args.app = app;
    args.dt_ns = tl.fixed_step_ns;
    if (!run_off_thread(&args)) {
        return 1;
    }
    orrery_host_result back = orrery_app_update(app, tl.fixed_step_ns);
    orrery_app_timeline end;
    orrery_host_result read = orrery_app_timeline_read(app, &end);
    orrery_host_result destroy = orrery_app_destroy(app);
    printf("threadhop on_creating_thread=%" PRIu32 " update=%d,%d,%d back_on_creator=%d "
           "fixed_steps=%" PRIu64 " destroy=%d\n",
           args.on_creating_thread, (int)args.results[0], (int)args.results[1],
           (int)args.results[2], (int)back, read == ORRERY_HOST_OK ? end.fixed_steps : 0,
           (int)destroy);
    return 0;
}

/* ---- main ---------------------------------------------------------------- */

static int parse_options(int argc, char **argv, options *opt) {
    for (int i = 2; i < argc; ++i) {
        const char *arg = argv[i];
        const char *value = (i + 1 < argc) ? argv[i + 1] : NULL;
        if (strcmp(arg, "--no-app") == 0) {
            opt->with_app = 0;
            continue;
        }
        if (strcmp(arg, "--time-period") == 0) {
#ifdef _WIN32
            /* Raised here, before the first paced sleep, and reported as
             * whatever the call returned. A refusal is not silently rewritten
             * into a "default resolution" run: the flag asked for a condition
             * the report would then misdescribe. */
            if (!raise_timer_resolution()) {
                fprintf(stderr, "timeBeginPeriod(1) was refused\n");
                return 0;
            }
            opt->time_period = 1;
            continue;
#else
            fprintf(stderr, "--time-period is Windows-only; there is no timer resolution to "
                            "raise on this platform\n");
            return 0;
#endif
        }
        if (value == NULL) {
            fprintf(stderr, "%s needs a value\n", arg);
            return 0;
        }
        if (strcmp(arg, "--entities") == 0) {
            opt->entities = (uint32_t)strtoul(value, NULL, 10);
        } else if (strcmp(arg, "--ticks") == 0) {
            opt->ticks = strtoull(value, NULL, 10);
        } else if (strcmp(arg, "--warmup") == 0) {
            opt->warmup = strtoull(value, NULL, 10);
        } else if (strcmp(arg, "--clock") == 0) {
            if (strcmp(value, "manual") == 0) {
                opt->clock = ORRERY_APP_CLOCK_MANUAL;
            } else if (strcmp(value, "auto") == 0) {
                opt->clock = ORRERY_APP_CLOCK_AUTOMATIC;
            } else {
                fprintf(stderr, "--clock manual|auto\n");
                return 0;
            }
        } else if (strcmp(arg, "--idle-seconds") == 0) {
            opt->idle_seconds = strtod(value, NULL);
        } else if (strcmp(arg, "--report") == 0) {
            opt->report = value;
        } else if (strcmp(arg, "--note") == 0) {
            if (opt->note_count < MAX_NOTES) {
                opt->notes[opt->note_count++] = value;
            }
        } else {
            fprintf(stderr, "unknown option %s\n", arg);
            return 0;
        }
        i += 1;
    }
    if (opt->entities < 2) {
        fprintf(stderr, "--entities must be at least 2\n");
        return 0;
    }
    return 1;
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: spike_consumer bench|smoke|panic|threadhop [options]\n");
        return 2;
    }
    options opt;
    memset(&opt, 0, sizeof opt);
    opt.entities = 24;
    opt.ticks = 36000;
    opt.warmup = 600;
    opt.clock = ORRERY_APP_CLOCK_MANUAL;
    opt.with_app = 1;
    opt.idle_seconds = 3.0;

    if (strcmp(argv[1], "bench") == 0) {
        if (!parse_options(argc, argv, &opt)) {
            release_timer_resolution();
            return 2;
        }
        int rc = run_bench(&opt, 0);
        release_timer_resolution();
        return rc;
    }
    if (strcmp(argv[1], "smoke") == 0) {
        opt.ticks = 120;
        opt.warmup = 0;
        if (!parse_options(argc, argv, &opt)) {
            release_timer_resolution();
            return 2;
        }
        int rc = run_bench(&opt, 1);
        release_timer_resolution();
        return rc;
    }
    if (strcmp(argv[1], "panic") == 0) {
        return run_panic();
    }
    if (strcmp(argv[1], "threadhop") == 0) {
        return run_threadhop();
    }
    fprintf(stderr, "unknown mode %s\n", argv[1]);
    return 2;
}
