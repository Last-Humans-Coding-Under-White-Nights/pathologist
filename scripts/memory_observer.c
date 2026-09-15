#define _GNU_SOURCE
// Diagnostic Linux/glibc observer loaded only by scripts/profile_memory.py.
// Sample at existing stage messages; the application's allocator is unchanged.
#include <unistd.h>
#include <sys/syscall.h>
#include <fcntl.h>
#include <malloc.h>
#include <string.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

static __thread int observing;
// Whether TRACE_MEMORY_TRIM is set: read once, on the first stage message.
static int trim_requested = -1;

static size_t rss_kib(void) {
    char status[8192];
    int fd = syscall(SYS_openat, AT_FDCWD, "/proc/self/status", O_RDONLY, 0);
    if (fd < 0) return 0;
    ssize_t n = syscall(SYS_read, fd, status, sizeof(status) - 1);
    syscall(SYS_close, fd);
    if (n <= 0) return 0;
    status[n] = 0;
    char *rss = strstr(status, "VmRSS:");
    return rss ? strtoull(rss + 6, NULL, 10) : 0;
}

static void snapshot(const char *stage, const char *event) {
    struct mallinfo2 m = mallinfo2();
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    char line[512];
    int n = snprintf(line, sizeof(line),
        "[memory] stage=%s event=%s rss_kib=%zu allocated_kib=%zu free_arena_kib=%zu arena_kib=%zu mmap_kib=%zu time=%ld.%09ld\n",
        stage, event, rss_kib(), (m.uordblks + m.hblkhd) / 1024,
        m.fordblks / 1024, m.arena / 1024, m.hblkhd / 1024, t.tv_sec, t.tv_nsec);
    syscall(SYS_write, STDERR_FILENO, line, n);
}

ssize_t write(int fd, const void *buffer, size_t count) {
    static const char *stages[] = {
        "discover: ", "include-graph: ", "preprocess: ",
        "preprocess-done: ", "pch-done: ", "index: ", "analyze: ", "export: "
    };
    if (fd == STDERR_FILENO && !observing) {
        for (size_t i = 0; i < sizeof(stages) / sizeof(*stages); ++i) {
            size_t length = strlen(stages[i]);
            if (count >= length && memcmp(buffer, stages[i], length) == 0) {
                observing = 1;
                snapshot(stages[i], "before");
                if (trim_requested < 0) trim_requested = getenv("TRACE_MEMORY_TRIM") != NULL;
                if (trim_requested && i >= 4) {
                    malloc_trim(0);
                    snapshot(stages[i], "after_trim");
                }
                observing = 0;
                break;
            }
        }
    }
    return syscall(SYS_write, fd, buffer, count);
}
