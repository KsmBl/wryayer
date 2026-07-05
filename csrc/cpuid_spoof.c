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
 * CPUID faulting is Intel-only; on unsupported CPUs the shim quietly does
 * nothing and CPUID passes through unchanged (so the app still runs fine).
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
static int g_have_vendor, g_have_brand, g_active;

static sigaction_fn real_sigaction;
static signal_fn real_signal;

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

__attribute__((constructor)) static void init(void) {
    const char *v = getenv("WRYAYER_CPUID_VENDOR");
    const char *b = getenv("WRYAYER_CPUID_BRAND");
    const char *f = getenv("WRYAYER_CPUID_FMS");
    if ((!v || !*v) && (!b || !*b) && (!f || !*f)) {
        return; /* nothing to spoof */
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
