/*
 * cpuid_spoof — an LD_PRELOAD shim that spoofs the CPUID instruction.
 *
 * Bind-mounting /proc/cpuinfo only fools tools that parse that file. Hardware
 * detection libraries (libcpuid, used by CPU-X, and many others) execute the
 * CPUID instruction directly, which reads the real silicon. The only way to
 * intercept that in userspace is Intel's "CPUID faulting": arch_prctl can make
 * every CPUID in the process raise #GP (delivered as SIGSEGV). We catch that,
 * return spoofed register values for the leaves that carry the CPU's identity
 * (vendor string, brand string, family/model), pass everything else through by
 * briefly re-enabling CPUID, then skip past the instruction.
 *
 * Apps like CPU-X install their own SIGSEGV handler (a crash reporter) which
 * would clobber ours, so we also interpose sigaction()/signal(): a request to
 * handle SIGSEGV is stored, not installed, and our handler chains genuine
 * (non-CPUID) faults through to it.
 *
 * Spoof data comes from the environment (set by wryayer):
 *   WRYAYER_CPUID_VENDOR  — 12-char vendor id, e.g. "AuthenticAMD"
 *   WRYAYER_CPUID_BRAND   — up to 48-char brand string (the displayed name)
 *   WRYAYER_CPUID_FMS     — leaf-1 EAX (family/model/stepping), hex; 0 = keep real
 *
 * CPUID faulting is Intel-only. On AMD (and other unsupported CPUs) it cannot
 * be enabled, so the SIGSEGV path above never activates. For those machines we
 * fall back to interposing libcpuid's public raw-data API (cpuid_get_raw_data /
 * cpuid_get_all_raw_data, used by CPU-X): we let the real call read the silicon,
 * then overwrite the identity and topology leaves in the returned dump so the
 * library derives the spoofed vendor, brand, family/model, socket and core
 * counts. Tools that don't use libcpuid keep the /proc + affinity spoofing.
 */
#if defined(__x86_64__)

#define _GNU_SOURCE
#include <stdint.h>
#include <string.h>
#include <stdlib.h>
#include <signal.h>
#include <ucontext.h>
#include <unistd.h>
#include <dlfcn.h>
#include <errno.h>
#include <sched.h>
#include <sys/syscall.h>

/* Parse a hex/decimal u32 without libc's strtoul (avoids a GLIBC_2.38 symbol
 * dependency that would keep the shim from loading against older glibc). */
static uint32_t parse_u32(const char *s) {
    if (!s) return 0;
    uint32_t v = 0;
    int hex = (s[0] == '0' && (s[1] == 'x' || s[1] == 'X'));
    if (hex) s += 2;
    for (; *s; s++) {
        char c = *s;
        uint32_t d;
        if (c >= '0' && c <= '9') d = (uint32_t)(c - '0');
        else if (c >= 'a' && c <= 'f') d = (uint32_t)(c - 'a' + 10);
        else if (c >= 'A' && c <= 'F') d = (uint32_t)(c - 'A' + 10);
        else break;
        v = hex ? (v << 4) + d : v * 10 + d;
    }
    return v;
}

#ifndef ARCH_SET_CPUID
#define ARCH_SET_CPUID 0x1012
#endif

typedef int (*sigaction_fn)(int, const struct sigaction *, struct sigaction *);
typedef void (*sighandler_t)(int);
typedef sighandler_t (*signal_fn)(int, sighandler_t);

static char g_vendor[16];
static char g_brand[52];
static uint32_t g_fms;
static uint32_t g_cores, g_threads;
static int g_have_vendor, g_have_brand, g_have_topo, g_active;

static sigaction_fn real_sigaction;
static signal_fn real_signal;

typedef int (*getaffinity_fn)(pid_t, size_t, cpu_set_t *);
static getaffinity_fn real_sched_getaffinity;
typedef int (*setaffinity_fn)(pid_t, size_t, const cpu_set_t *);
static setaffinity_fn real_sched_setaffinity;
/* The logical CPU libcpuid last "pinned" to; fed back as the x2APIC id in leaf
 * 0xB/0x1F so its per-CPU enumeration sees g_threads distinct logical CPUs. */
static uint32_t g_cur_cpu;

/* libcpuid raw-dump fallback (used when CPUID faulting is unavailable, i.e. AMD).
 * We only need the two leading arrays of struct cpu_raw_data_t — everything we
 * patch lives there, and they sit at the front of the struct in every libcpuid
 * version, so we never depend on the (growing) tail or on its exact size. */
struct lc_raw_front {
    uint32_t basic_cpuid[32][4];   /* leaves 0x00000000..0x0000001F */
    uint32_t ext_cpuid[32][4];     /* leaves 0x80000000..0x8000001F */
};
/* struct cpu_raw_data_array_t: { bool with_affinity; int32_t num_raw;
 * cpu_raw_data_t* raw; } — on LP64 the raw pointer sits at offset 8. */
struct lc_raw_array {
    unsigned char head[8];
    struct lc_raw_front *raw;
};
typedef int (*get_raw_fn)(struct lc_raw_front *);
typedef int (*get_all_raw_fn)(struct lc_raw_array *);
static get_raw_fn real_cpuid_get_raw_data;
static get_all_raw_fn real_cpuid_get_all_raw_data;

/* The handler the app tried to install for SIGSEGV; we chain real faults here. */
static struct sigaction g_app_sa;
static int g_app_has;

static int set_cpuid(unsigned long on) {
    return (int)syscall(SYS_arch_prctl, (unsigned long)ARCH_SET_CPUID, on);
}

/* Execute a real CPUID with faulting momentarily disabled. */
static void real_cpuid(uint32_t leaf, uint32_t sub, uint32_t r[4]) {
    set_cpuid(1);
    __asm__ volatile("cpuid"
                     : "=a"(r[0]), "=b"(r[1]), "=c"(r[2]), "=d"(r[3])
                     : "a"(leaf), "c"(sub));
    set_cpuid(0);
}

static void handler(int sig, siginfo_t *si, void *ucv) {
    ucontext_t *uc = (ucontext_t *)ucv;
    greg_t *regs = uc->uc_mcontext.gregs;
    unsigned char *rip = (unsigned char *)regs[REG_RIP];

    if (rip[0] == 0x0F && rip[1] == 0xA2) {
        /* A CPUID instruction — spoof the identity leaves, pass the rest. */
        uint32_t leaf = (uint32_t)regs[REG_RAX];
        uint32_t sub = (uint32_t)regs[REG_RCX];
        uint32_t r[4];
        real_cpuid(leaf, sub, r);

        if (leaf == 0 && g_have_vendor) {
            memcpy(&r[1], g_vendor + 0, 4); /* EBX */
            memcpy(&r[3], g_vendor + 4, 4); /* EDX */
            memcpy(&r[2], g_vendor + 8, 4); /* ECX */
        } else if (leaf == 1 && g_fms) {
            r[0] = g_fms;
        } else if (leaf >= 0x80000002u && leaf <= 0x80000004u && g_have_brand) {
            int off = (int)(leaf - 0x80000002u) * 16;
            memcpy(&r[0], g_brand + off + 0, 4);
            memcpy(&r[1], g_brand + off + 4, 4);
            memcpy(&r[2], g_brand + off + 8, 4);
            memcpy(&r[3], g_brand + off + 12, 4);
        }

        /* Topology spoofing so libcpuid (CPU-X) reports the fake core/thread
         * counts. We drive the *legacy* leaves (1/4/0x80000008) and neutralise
         * the extended-topology leaves (0xB/0x1F) so libcpuid falls back to the
         * legacy path we control instead of reading the real silicon's leaf B. */
        if (g_have_topo) {
            uint32_t cores = g_cores ? g_cores : 1;
            uint32_t threads = g_threads ? g_threads : cores;
            uint32_t tpc = (cores > 0) ? (threads / cores) : 1;
            if (tpc < 1) tpc = 1;
            if (leaf == 1) {
                /* EBX[23:16] = logical processors per package; EDX[28] = HTT. */
                r[1] = (r[1] & 0x00FFFFFFu) | ((threads & 0xFFu) << 16);
                if (threads > 1) r[3] |= (1u << 28);
            } else if (leaf == 4) {
                /* Intel: EAX[31:26] = max addressable core IDs per package - 1. */
                r[0] = (r[0] & 0x03FFFFFFu) | (((cores - 1) & 0x3Fu) << 26);
            } else if (leaf == 0xB || leaf == 0x1F) {
                /* Provide a coherent extended-topology hierarchy so libcpuid
                 * derives cores = logical / threads-per-core directly, without
                 * pinning to every (partly fake) CPU:
                 *   sub 0 = SMT level  → EBX = threads per core
                 *   sub 1 = Core level → EBX = logical per package
                 *   sub ≥2 = invalid. ECX = level | (type<<8). */
                uint32_t smt_shift = 0; while ((1u << smt_shift) < tpc)     smt_shift++;
                uint32_t pkg_shift = 0; while ((1u << pkg_shift) < threads) pkg_shift++;
                if (sub == 0) {
                    r[0] = smt_shift;
                    r[1] = tpc;
                    r[2] = 0u | (1u << 8);       /* level 0, type = SMT(1) */
                } else if (sub == 1) {
                    r[0] = pkg_shift;
                    r[1] = threads;
                    r[2] = 1u | (2u << 8);       /* level 1, type = Core(2) */
                } else {
                    r[0] = 0;
                    r[1] = 0;
                    r[2] = sub & 0xFFu;          /* type 0 = invalid */
                }
                r[3] = g_cur_cpu;                /* x2APIC id of "current" CPU */
            } else if (leaf == 0x80000008u) {
                /* AMD: ECX[7:0] = NC = number of cores - 1. */
                r[2] = (r[2] & 0xFFFFFF00u) | ((cores - 1) & 0xFFu);
            }
        }

        regs[REG_RAX] = r[0];
        regs[REG_RBX] = r[1];
        regs[REG_RCX] = r[2];
        regs[REG_RDX] = r[3];
        regs[REG_RIP] = (greg_t)(rip + 2);
        return;
    }

    /* A genuine fault — hand it to the app's own SIGSEGV handler if it set one. */
    if (g_app_has) {
        if (g_app_sa.sa_flags & SA_SIGINFO) {
            if (g_app_sa.sa_sigaction) {
                g_app_sa.sa_sigaction(sig, si, ucv);
                return;
            }
        } else if (g_app_sa.sa_handler == SIG_IGN) {
            return;
        } else if (g_app_sa.sa_handler && g_app_sa.sa_handler != SIG_DFL) {
            g_app_sa.sa_handler(sig);
            return;
        }
    }
    /* Nothing to chain to: restore the default action and let it crash. */
    struct sigaction dfl;
    memset(&dfl, 0, sizeof(dfl));
    dfl.sa_handler = SIG_DFL;
    if (real_sigaction) real_sigaction(SIGSEGV, &dfl, NULL);
}

/* Interpose sigaction: stash the app's SIGSEGV request, keep ours installed. */
int sigaction(int signum, const struct sigaction *act, struct sigaction *old) {
    if (!real_sigaction) real_sigaction = (sigaction_fn)dlsym(RTLD_NEXT, "sigaction");
    if (g_active && signum == SIGSEGV) {
        if (old) {
            if (g_app_has) {
                *old = g_app_sa;
            } else {
                memset(old, 0, sizeof(*old));
                old->sa_handler = SIG_DFL;
            }
        }
        if (act) {
            g_app_sa = *act;
            g_app_has = 1;
        }
        return 0;
    }
    return real_sigaction(signum, act, old);
}

/* Interpose signal() too — translate to our sigaction semantics for SIGSEGV. */
sighandler_t signal(int signum, sighandler_t h) {
    if (!real_signal) real_signal = (signal_fn)dlsym(RTLD_NEXT, "signal");
    if (g_active && signum == SIGSEGV) {
        sighandler_t old = SIG_DFL;
        if (g_app_has && !(g_app_sa.sa_flags & SA_SIGINFO)) old = g_app_sa.sa_handler;
        struct sigaction sa;
        memset(&sa, 0, sizeof(sa));
        sa.sa_handler = h;
        sigemptyset(&sa.sa_mask);
        g_app_sa = sa;
        g_app_has = 1;
        return old;
    }
    return real_signal(signum, h);
}

/* Interpose sched_getaffinity so the process appears to be able to run on the
 * spoofed number of logical CPUs. libcpuid (CPU-X) and glibc's get_nprocs /
 * coreutils' nproc derive the logical-CPU count from this mask, so faking the
 * CPUID topology leaves alone isn't enough — the real affinity mask still has
 * only the host's CPUs. (Runtimes that issue the raw syscall directly, e.g. Go,
 * bypass this; that's an accepted limitation of a userspace shim.) */
int sched_getaffinity(pid_t pid, size_t cpusetsize, cpu_set_t *mask) {
    if (!real_sched_getaffinity)
        real_sched_getaffinity = (getaffinity_fn)dlsym(RTLD_NEXT, "sched_getaffinity");
    if (g_have_topo && g_threads > 0 && mask && cpusetsize > 0) {
        memset(mask, 0, cpusetsize);
        size_t maxbits = cpusetsize * 8;
        for (uint32_t i = 0; i < g_threads && i < maxbits; i++)
            CPU_SET_S(i, cpusetsize, mask);
        return 0;
    }
    if (real_sched_getaffinity)
        return real_sched_getaffinity(pid, cpusetsize, mask);
    errno = ENOSYS;
    return -1;
}

/* Interpose sched_setaffinity: libcpuid enumerates topology by pinning to each
 * logical CPU in turn and reading its APIC id. Record the selected CPU (fed to
 * leaf 0xB above) and report the pin as successful even for the fake surplus
 * CPUs that don't physically exist. Real CPUs are still pinned for real. */
int sched_setaffinity(pid_t pid, size_t cpusetsize, const cpu_set_t *mask) {
    if (!real_sched_setaffinity)
        real_sched_setaffinity = (setaffinity_fn)dlsym(RTLD_NEXT, "sched_setaffinity");
    if (g_have_topo && mask && cpusetsize > 0 && CPU_COUNT_S(cpusetsize, mask) == 1) {
        size_t maxbits = cpusetsize * 8;
        size_t idx = maxbits;
        for (size_t i = 0; i < maxbits; i++) {
            if (CPU_ISSET_S(i, cpusetsize, mask)) { idx = i; break; }
        }
        /* Only the spoofed CPUs [0, g_threads) exist to the sandbox: those pins
         * report success; pins to any other CPU fail, so libcpuid's enumeration
         * counts exactly g_threads logical CPUs — even when the host has more
         * real CPUs than the (smaller) spoofed count. */
        if (idx < g_threads) {
            g_cur_cpu = (uint32_t)idx;
            if (real_sched_setaffinity) real_sched_setaffinity(pid, cpusetsize, mask);
            return 0;
        }
        errno = EINVAL;
        return -1;
    }
    if (real_sched_setaffinity)
        return real_sched_setaffinity(pid, cpusetsize, mask);
    errno = ENOSYS;
    return -1;
}

/* Rewrite the identity and topology leaves of a libcpuid raw dump in place so
 * cpu_identify() derives the spoofed CPU. Mirrors the SIGSEGV handler's spoof,
 * but writes the stored dump instead of live registers — the path used on AMD,
 * where CPUID can't be trapped. */
static void patch_front(struct lc_raw_front *r) {
    if (!r) return;
    uint32_t (*b)[4] = r->basic_cpuid;
    uint32_t (*e)[4] = r->ext_cpuid;

    if (g_have_vendor) {
        memcpy(&b[0][1], g_vendor + 0, 4); /* EBX */
        memcpy(&b[0][3], g_vendor + 4, 4); /* EDX */
        memcpy(&b[0][2], g_vendor + 8, 4); /* ECX */
    }
    if (g_fms) b[1][0] = g_fms;            /* leaf 1 EAX: family/model/stepping */
    if (g_have_brand) {
        for (int i = 0; i < 3; i++) {      /* leaves 0x80000002..4: brand string */
            memcpy(&e[2 + i][0], g_brand + i * 16 + 0, 4);
            memcpy(&e[2 + i][1], g_brand + i * 16 + 4, 4);
            memcpy(&e[2 + i][2], g_brand + i * 16 + 8, 4);
            memcpy(&e[2 + i][3], g_brand + i * 16 + 12, 4);
        }
        if (e[0][0] < 0x80000004u) e[0][0] = 0x80000004u; /* advertise the leaves */
    }
    if (g_have_topo) {
        uint32_t cores = g_cores ? g_cores : 1;
        uint32_t threads = g_threads ? g_threads : cores;
        uint32_t tpc = cores ? threads / cores : 1;
        if (tpc < 1) tpc = 1;
        /* leaf 1: EBX[23:16] = logical CPUs per package; EDX[28] = HTT. */
        b[1][1] = (b[1][1] & 0x00FFFFFFu) | ((threads & 0xFFu) << 16);
        if (threads > 1) b[1][3] |= (1u << 28);
        /* AMD leaf 0x80000008 ECX[7:0] = NC = physical cores - 1. */
        e[8][2] = (e[8][2] & 0xFFFFFF00u) | ((cores - 1) & 0xFFu);
        /* AMD leaf 0x8000001E EBX[15:8] = threads per core - 1 (needs TopoExt). */
        e[0x1E][1] = (e[0x1E][1] & 0xFFFF00FFu) | (((tpc - 1) & 0xFFu) << 8);
        e[1][2] |= (1u << 22);             /* leaf 0x80000001 ECX[22] = TopoExt */
        if (e[0][0] < 0x8000001Eu) e[0][0] = 0x8000001Eu;
    }
}

/* Interpose libcpuid's single-CPU raw dump: fill it for real, then spoof it.
 * Only engaged when CPUID faulting is off (g_active == 0) — with faulting the
 * dump already reads through the spoofing handler. */
int cpuid_get_raw_data(void *data) {
    if (!real_cpuid_get_raw_data)
        real_cpuid_get_raw_data = (get_raw_fn)dlsym(RTLD_NEXT, "cpuid_get_raw_data");
    int rc = real_cpuid_get_raw_data ? real_cpuid_get_raw_data((struct lc_raw_front *)data) : -1;
    if (!g_active) patch_front((struct lc_raw_front *)data);
    return rc;
}

/* Interpose libcpuid's all-CPU raw dump. The affinity interposers already make
 * this enumerate g_threads logical CPUs; patch the representative entry so the
 * derived identity and per-core counts match the spoof. */
int cpuid_get_all_raw_data(void *arr) {
    if (!real_cpuid_get_all_raw_data)
        real_cpuid_get_all_raw_data = (get_all_raw_fn)dlsym(RTLD_NEXT, "cpuid_get_all_raw_data");
    int rc = real_cpuid_get_all_raw_data ? real_cpuid_get_all_raw_data((struct lc_raw_array *)arr) : -1;
    if (!g_active && arr) patch_front(((struct lc_raw_array *)arr)->raw);
    return rc;
}

__attribute__((constructor)) static void init(void) {
    const char *v = getenv("WRYAYER_CPUID_VENDOR");
    const char *b = getenv("WRYAYER_CPUID_BRAND");
    const char *f = getenv("WRYAYER_CPUID_FMS");
    const char *co = getenv("WRYAYER_CPUID_CORES");
    const char *th = getenv("WRYAYER_CPUID_THREADS");
    if ((!v || !*v) && (!b || !*b) && (!f || !*f) && (!th || !*th)) {
        return; /* nothing to spoof */
    }

    if (th && *th) {
        g_threads = parse_u32(th);
        g_cores = (co && *co) ? parse_u32(co) : g_threads;
        if (g_cores < 1) g_cores = 1;
        if (g_threads < g_cores) g_threads = g_cores;
        g_have_topo = (g_threads >= 1);
    }

    if (v && *v) {
        memset(g_vendor, 0, sizeof(g_vendor));
        strncpy(g_vendor, v, 12);
        g_have_vendor = 1;
    }
    if (b && *b) {
        memset(g_brand, 0, sizeof(g_brand)); /* NUL-pad, like real CPUs */
        size_t n = strlen(b);
        if (n > 48) n = 48;
        memcpy(g_brand, b, n);
        g_have_brand = 1;
    }
    if (f && *f) {
        g_fms = parse_u32(f);
    }

    real_sigaction = (sigaction_fn)dlsym(RTLD_NEXT, "sigaction");
    real_signal = (signal_fn)dlsym(RTLD_NEXT, "signal");
    if (!real_sigaction) return;

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = handler;
    sa.sa_flags = SA_SIGINFO | SA_RESTART;
    sigemptyset(&sa.sa_mask);
    if (real_sigaction(SIGSEGV, &sa, NULL) != 0) return;

    /* Enable CPUID faulting; fails (harmlessly) on AMD / unsupported kernels. */
    if (set_cpuid(0) != 0) {
        struct sigaction dfl;
        memset(&dfl, 0, sizeof(dfl));
        dfl.sa_handler = SIG_DFL;
        real_sigaction(SIGSEGV, &dfl, NULL);
        return;
    }
    g_active = 1;
}

#endif /* __x86_64__ */
