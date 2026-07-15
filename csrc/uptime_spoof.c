// LD_PRELOAD shim that reports a fake system uptime.
//
// A /proc/uptime bind only fools tools that parse that file. fastfetch (and
// others) read the uptime from clock_gettime(CLOCK_BOOTTIME) — served by the
// vDSO, so it can't be bound as a file — or from sysinfo(2). This shim
// interposes both libc symbols and shifts their result by a constant offset so
// the *absolute* uptime reads as WRYAYER_UPTIME seconds while intervals between
// two calls stay real (time still advances from the fake value).
//
// Only CLOCK_BOOTTIME is rewritten; CLOCK_MONOTONIC and friends are left intact
// so timers and animations are unaffected.

#define _GNU_SOURCE
#include <time.h>
#include <sys/sysinfo.h>
#include <stdlib.h>
#include <dlfcn.h>

static int  g_init_done = 0;
static long g_fake_uptime = -1; // seconds; <0 = disabled (no env / bad value)
static long g_boot_offset = 0;  // real CLOCK_BOOTTIME at init - g_fake_uptime

static int (*real_clock_gettime)(clockid_t, struct timespec *) = NULL;
static int (*real_sysinfo)(struct sysinfo *) = NULL;

static void spoof_init(void) {
    if (g_init_done) return;
    g_init_done = 1;

    real_clock_gettime = (int (*)(clockid_t, struct timespec *))
        dlsym(RTLD_NEXT, "clock_gettime");
    real_sysinfo = (int (*)(struct sysinfo *)) dlsym(RTLD_NEXT, "sysinfo");

    const char *e = getenv("WRYAYER_UPTIME");
    if (e && *e) {
        char *end = NULL;
        long v = strtol(e, &end, 10);
        if (end != e && v >= 0) g_fake_uptime = v;
    }

    // Anchor the offset to the real boot clock once, so subsequent reads keep
    // advancing at the real rate from the fake starting point.
    if (g_fake_uptime >= 0 && real_clock_gettime) {
        struct timespec ts;
        if (real_clock_gettime(CLOCK_BOOTTIME, &ts) == 0)
            g_boot_offset = (long) ts.tv_sec - g_fake_uptime;
    }
}

int clock_gettime(clockid_t clk, struct timespec *tp) {
    spoof_init();
    int r = real_clock_gettime ? real_clock_gettime(clk, tp) : -1;
    if (r == 0 && g_fake_uptime >= 0 && clk == CLOCK_BOOTTIME) {
        tp->tv_sec -= g_boot_offset;
        if (tp->tv_sec < 0) tp->tv_sec = 0;
    }
    return r;
}

int sysinfo(struct sysinfo *info) {
    spoof_init();
    int r = real_sysinfo ? real_sysinfo(info) : -1;
    if (r == 0 && g_fake_uptime >= 0) {
        // info->uptime is the real uptime; the same constant offset that shifts
        // CLOCK_BOOTTIME turns it into the fake value while preserving deltas.
        info->uptime -= g_boot_offset;
        if (info->uptime < 0) info->uptime = 0;
    }
    return r;
}
