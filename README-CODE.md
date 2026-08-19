# wryayer — internals & architecture

> This document is for contributors and the curious: how wryayer is built, how
> the on-disk layout works, how the sandbox is assembled at runtime, and how to
> build and test it. If you just want to **install and use** apps, read
> [`README.md`](README.md) instead. If you want a task-oriented *"I want to
> change X — where do I go?"* guide, read
> [`README-PROGRAMMING.md`](README-PROGRAMMING.md).

---

## Source tree

```
src/
├── main.rs            ← clap CLI entry point; dispatches subcommands
├── lib.rs             ← module wiring; exposed for the integration tests
├── manifest.rs        ← .manifest.toml read/write, app dir helpers, list_all_apps
├── avahi_stub.rs      ← in-process owner of org.freedesktop.Avahi on a private bus
├── child_output.rs    ← sanitising a subprocess's output before it is drawn
├── test_support.rs    ← the one HOME lock every test that touches the FS takes
├── config.rs          ← AppConfig, INI parse/format, global defaults.ini
├── cpu.rs             ← CPU profiles + custom CPUs; /proc/cpuinfo & CPUID data
├── launcher.rs        ← /usr/bin/<app> shell wrapper create/remove
├── desktop.rs         ← host .desktop entries: menus, Open-with, link handling
├── veracrypt.rs       ← per-app container create/mount/unmount, sizing, marker
├── secrets.rs         ← master password store (Argon2id + AES-256-GCM)
├── entropy.rs         ← multi-source entropy pool + password generator
├── distro.rs          ← per-distro backend (pacman / apt / dnf), auto-detected
├── package/
│   ├── deps.rs        ← BFS dependency resolver, virtual/soname fallback
│   ├── download.rs    ← official download + AUR git clone/makepkg build
│   ├── extract.rs     ← unpack .pkg.tar.zst / .deb / .rpm into an app tree
│   └── soname_check.rs← scan ELF NEEDED entries, find owning packages
├── commands/
│   ├── install.rs     ← resolve → download → extract → manifest → dedup
│   ├── install_game.rs← wine-container import (folder → .exe → prefix)
│   ├── run/
│   │   ├── mod.rs     ← assemble and exec the bwrap sandbox
│   │   ├── bus.rs     ← D-Bus proxy, Avahi stub, portal listener, bound-app entries
│   │   └── spoof.rs   ← /proc, /sys and DMI overlays; device masks
│   ├── update.rs      ← re-resolve + re-extract; version checks
│   ├── remove.rs      ← delete tree + launcher + desktop entries; alias-aware
│   ├── relink.rs      ← rebuild shortcuts + desktop entries of installed apps
│   ├── snapshot.rs    ← hard-linked snapshots + rollback
│   ├── export.rs      ← zip an app tree with progress markers
│   ├── import.rs      ← recreate an app from an exported zip
│   ├── dedup.rs       ← cross-app hard-link identical files; du accounting
│   ├── repair.rs      ← resolve+install packages for missing sonames
│   ├── encrypt.rs     ← move an app into/out of a container; password sources
│   ├── list.rs        ← table + apparent/on-disk/savings totals
│   ├── clean.rs       ← wipe the shared download/build cache
│   ├── portal.rs      ← host-side listener for cross-container app binding
│   └── config.rs      ← `wryayer config` CLI surface
├── tui/
│   ├── mod.rs         ← App state, event loop, all key handling
│   ├── ui.rs          ← ratatui rendering for every screen
│   └── konami.rs      ← easter-egg FSM
└── gui/               ← optional GTK4 front-end (feature = "gui")
    ├── mod.rs         ← window, tabs, install/run/console flows
    ├── config.rs      ← per-app + global settings forms
    ├── encryption.rs  ← container dialogs; collects secrets for the child
    ├── install.rs     ← search-and-tick install flow
    └── op.rs          ← subprocess console, shared job queue

csrc/                  ← C helpers compiled by build.rs, embedded via include_bytes!
├── cpuid_spoof.c      ← LD_PRELOAD shim: intercept CPUID + sched_get/setaffinity
├── uptime_spoof.c     ← LD_PRELOAD shim: fake CLOCK_BOOTTIME / sysinfo uptime
└── portal_client.c    ← static helper symlinked into sandboxes as bound apps
```

---

## Architecture

```
                          ┌──────────────────────────────────────────┐
                          │                 wryayer                   │
                          └──────────────┬───────────────────────────┘
              ┌───────────────────────────┴──────────────────────────┐
              │  TUI (ratatui / crossterm)    CLI (clap)             │
              │  ┌───────────────────────┐   install   remove  list  │
              │  │ Installed │ Install   │   run       update  repair │
              │  │ Import    │ Games     │   config    export  import │
              │  │ Space     │ Settings  │   snapshot  rollback  tui  │
              │  └───────────────────────┘   snapshots  snapshot-prune│
              │                              dedup      completions   │
              └────────────────────────┬─────────────────────────────┘
                                       │
          ┌────────────────────────────┼──────────────────────────────┐
          │                      Core layer                           │
          │                                                           │
          │  manifest.rs           config.rs          launcher.rs     │
          │  ┌──────────────┐   ┌────────────────┐  ┌─────────────┐   │
          │  │ .manifest.   │   │ config.ini     │  │ /usr/bin/…  │   │
          │  │ toml R/W     │   │ INI parse/write│  │ shell wrap  │   │
          │  │ list apps    │   │ sandbox options│  │ create/rm   │   │
          │  └──────────────┘   └────────────────┘  └─────────────┘   │
          │                                                           │
          │  package/                        commands/dedup.rs        │
          │  ┌──────────────────────────┐   ┌─────────────────────┐   │
          │  │ deps.rs                  │   │ hard-link identical │   │
          │  │  BFS dep resolver        │   │ files across apps   │   │
          │  │  virtual/soname fallback │   │ (dev,ino) accounting│   │
          │  │ download.rs              │   │ format_bytes / du   │   │
          │  │  delegates to distro.rs  │   └─────────────────────┘   │
          │  │ extract.rs               │                             │
          │  │  delegates to distro.rs  │                             │
          │  │ soname_check.rs          │                             │
          │  │  delegates to distro.rs  │                             │
          │  └──────────────────────────┘                             │
          │                                                           │
          │  distro.rs  (auto-detected from /etc/os-release)          │
          │  ┌──────────────────────────────────────────────────┐     │
          │  │  Arch:   pacman -Si, pacman -Sp, tar --zstd,     │     │
          │  │          vercmp, AUR RPC + git clone + makepkg   │     │
          │  │  Debian: apt-cache show, apt-get download,       │     │
          │  │          dpkg-deb -x, dpkg -S,                   │     │
          │  │          dpkg --compare-versions                 │     │
          │  │  Fedora: dnf repoquery, dnf download,            │     │
          │  │          rpm2cpio | cpio, rpm -qf, rpm --eval    │     │
          │  └──────────────────────────────────────────────────┘     │
          └───────────────────────────────────────────────────────────┘
```

The distro backend is resolved once from `/etc/os-release` (`ID` and
`ID_LIKE`). Every package operation — resolve, download, extract, version
compare, soname owner lookup — routes through `distro.rs` so the rest of the
codebase is package-manager agnostic.

---

## Filesystem layout

```
~/.wryayer/
├── .containers/             ← VeraCrypt volumes backing encrypted apps
│    ├── signal.hc           ← mounted over ~/.wryayer/signal/ when unlocked
│    └── signal.toml         ← listing marker + password_source (readable when locked)
├── .passwords.vault         ← master password store (Argon2id + AES-256-GCM)
├── signal/                  ← encrypted app: an empty mount point while locked
├── firefox/                 ← isolated root (bind-mounted as / at runtime)
│   ├── usr/
│   │   ├── bin/firefox
│   │   └── lib/             ← shared libs, hard-linked with other apps
│   │        libz.so.1       │  where content is identical (dedup)
│   │        libpng.so.16    │
│   ├── etc/                 ← app-specific /etc
│   ├── home/                ← the sandbox's $HOME (browser profiles, caches)
│   ├── .snapshots/          ← hard-linked clones (see Snapshots)
│   ├── .spoof/              ← generated spoof files (cpuinfo, meminfo, …)
│   ├── .manifest.toml       ← package list + install metadata
│   └── config.ini           ← per-app sandbox settings
├── fastfetch/               ← target of an `install --into` chain
│   ├── usr/bin/{fastfetch,hyfetch}
│   └── .manifest.toml
├── hyfetch/                 ← thin alias dir — no extracted files
│   ├── .manifest.toml       ← alias_of = "fastfetch"
│   └── config.ini           ← independent sandbox config
├── defaults.ini             ← global default settings (Settings tab)
└── vlc/
     └── ...

/usr/bin/
├── firefox    ──►  exec wryayer run firefox "$@"
├── fastfetch  ──►  exec wryayer run fastfetch "$@"
├── hyfetch    ──►  exec wryayer run hyfetch "$@"   (bwrap roots on fastfetch/)
└── vlc

/usr/share/applications/
└── wryayer-firefox-firefox.desktop   ← the app's own entry, Exec'd through
                                        /usr/bin/firefox (menus, link handling)
```

Everything wryayer writes lives under `~/.wryayer/` (state, snapshots, spoof
files, global defaults), plus the two host locations that make an app reachable
the way a packaged one is: `/usr/bin/` (shortcuts) and
`/usr/share/applications/` (desktop entries). Build/download caches live under
`~/.cache/wryayer/{pkg,build}`. The only other writes are ephemeral ones in
`$XDG_RUNTIME_DIR`: a private `.wryayer-<app>/` per sandbox holding its bus
proxy and portal sockets, and the master store's per-boot derived key. Nothing
is written elsewhere.

---

## The bwrap sandbox

At runtime `commands/run/` builds a `bwrap` command line (`mod.rs` assembles it;
`bus.rs` and `spoof.rs` hold the D-Bus/portal and `/proc`-`/sys` halves). The app's own tree
becomes `/`, and a curated set of host paths are bound in:

```
~/.wryayer/<app>/   ──► /                   (app root, rw; alias roots on target)
/dev                ──► /dev                (devices; masked per config)
/proc               ──► /proc
/sys                ──► /sys                (read-only)
/run                ──► /run                (session + system D-Bus, Wayland, PipeWire)
/tmp                ──► /tmp                (system | tmpfs | local dir | uuid dir)
/etc/resolv.conf    ──► /etc/…              (read-only host network/identity files)
/etc/hosts               …
/etc/ssl/certs           …
/etc/ca-certificates ──► /etc/ca-certificates  (CA bundle; ssl/certs symlinks point here)
/usr/share/fonts    ──► /usr/share/fonts    (read-only; Chromium/Electron/NW.js)
/etc/fonts          ──► /etc/fonts          (fontconfig configuration)
/usr/share/fontconfig ──► /usr/share/fontconfig
/usr/lib/qt6/plugins ──► /usr/lib/qt6/plugins  (Qt platform plugins from host)
<shared_dirs>       ──► <same>              (user-configured, read-write)
```

`bwrap_cmd()` returns the assembled `Command` plus optional child handles
(the D-Bus filter proxy, the Avahi stub bus, the portal listener) whose
lifetimes are tied to the sandbox. The
launcher forks bwrap, waits, and on abnormal exit re-scans the sandbox `home/`
for missing sonames (self-updating apps like Discord write new ELF binaries
there) and retries once.

### Identity spoofing

Spoofs are materialised as small files under `~/.wryayer/<app>/.spoof/` and
bound read-only over the corresponding sandbox path:

| Setting | Mechanism |
|---|---|
| hostname | `--ro-bind` a generated file over `/etc/hostname`; `--setenv HOSTNAME` |
| username | `--setenv USER` / `LOGNAME`; a rewritten `/etc/passwd` when needed |
| machine-id | generated file bound over `/etc/machine-id` (`random` regenerates each launch) |
| cpuinfo | see **CPU spoofing** below — file bound over `/proc/cpuinfo` plus CPUID + `/proc/stat` + `/sys` |
| os-release | file bound over `/etc/os-release` **and** `/usr/lib/os-release` |
| terminal | walk the process tree to find the real terminal, set its env var (`TERM_PROGRAM`, `KITTY_WINDOW_ID`, …) |

### CPU spoofing

A spoofed CPU (`preset:<key>`, a `custom:<…>` value from the TUI configurator, or
`sample`) is presented to the sandbox through **four** layers, because different
tools read the CPU in different ways:

| Layer | What it fixes | Mechanism |
|---|---|---|
| `/proc/cpuinfo` | file parsers (`lscpu`, scripts) | render one block per thread from `cpu.rs`, bind over `/proc/cpuinfo` |
| CPUID instruction | `libcpuid` / CPU-X, anything running `cpuid` | `csrc/cpuid_spoof.c` LD_PRELOAD shim using CPUID-faulting |
| `/proc/stat` | htop (per-core meters + usage) | rendered per-thread, refreshed live by a background thread |
| `/sys/devices/system/cpu` | tools counting `cpuN` dirs; `sysconf`/`nproc` | tmpfs overlay rebuilt with `threads` cpuN dirs + spoofed `online`/`present`/`possible` |

The **CPUID shim** (`cpuid_spoof.c`) is the subtle part. CPUID faulting
(`arch_prctl(ARCH_SET_CPUID, 0)`, Intel-only) makes every `cpuid` raise `#GP`,
delivered as `SIGSEGV`; the shim's handler emulates the identity leaves (vendor,
brand, family/model/stepping) and a coherent **topology** — leaf 1 EBX, leaf 4 /
`0x80000008`, and a synthetic extended-topology leaf `0xB`/`0x1F` (SMT + Core
levels) — then steps over the instruction. It also interposes
`sched_getaffinity`/`sched_setaffinity` so libcpuid's "pin to each CPU and read
its APIC id" enumeration (and `nproc`) count exactly `threads` logical CPUs; the
setaffinity interposer reports success only for CPUs `[0, threads)`, feeding a
distinct APIC id back through leaf `0xB` per pinned CPU. It interposes
`sigaction`/`signal` too, so an app's own `SIGSEGV` handler can't displace ours.
Built without `-fvisibility=hidden` so the interposers win symbol resolution.

`/proc/stat` is regenerated by a background thread (~2 Hz): the container's first
N per-CPU lines mirror the host's real N cores **1:1** (so their busy/idle
counters carry real usage), and any surplus cores cycle back through the real
cores. htop therefore shows live per-core activity for the first host cores.

`sched_getaffinity` returns the real host mask through the raw syscall (Go and
other runtimes that bypass libc are not spoofed) — an accepted limitation.

### RAM limit and spoofed meminfo

When `ram_limit` is set, the bwrap command is wrapped in
`systemd-run --scope --user -p MemoryMax=NM -p MemorySwapMax=0`. Both limits are
required: without `MemorySwapMax=0` the kernel silently offloads pages to swap
(including zram) and the cap is exceeded. A background thread reads the scope's
cgroup `memory.current` and rewrites a spoofed `/proc/meminfo` (bound into the
sandbox) so the app sees `MemFree` shrink toward the configured ceiling.

### D-Bus portal filter (file pickers)

`portal_filter` (default on) routes the sandbox's D-Bus **session** bus through
an `xdg-dbus-proxy` instance whose filter hides the host desktop portal
(`org.freedesktop.portal.*`) while still forwarding Notifications, secrets,
MPRIS, etc. Together with synthetic XDG user-dir files written into the sandbox
(`user-dirs.dirs`, empty GTK bookmarks, a stub `recently-used.xbel`), this makes
in-sandbox file choosers list **only shared directories** instead of leaking the
real home tree. The proxy carries `PR_SET_PDEATHSIG` so it dies with the app.

### Cross-container app binding (portal)

`bound_apps` (config key `bind_app`) lets one sandbox launch another app in *its*
own container. For each bound app, `run.rs`:

1. spawns a host-side listener (`commands/portal.rs`, run as the hidden
   `wryayer portal-listener <sock> <allowed-csv>` subcommand) on an AF_UNIX
   socket under the app's isolated `XDG_RUNTIME_DIR` — which is bind-mounted
   through `/run`, so the same absolute path resolves inside the sandbox;
2. binds a **static** helper (`csrc/portal_client.c`, compiled by `build.rs` to
   `wryayer-portal`) read-only into the sandbox, and symlinks it under each bound
   app's name on a private `/.wryayer-bin` dir prepended to `PATH`;
3. also symlinks the generic openers (`xdg-open`, `x-www-browser`, …) to the
   helper and binds them over the real `/usr/bin/xdg-open`, setting
   `WRYAYER_OPEN_APP` to the chosen browser (`pick_open_app` prefers a
   browser-named bound app);
4. generates a **desktop-entry tree** (`desktop::sandbox_entries`, written by
   `bus::write_bound_app_entries` into the app's `.spoof` dir and bound at
   `/.wryayer-share`): one `.desktop` per bound app whose `Exec`/`TryExec` point
   at that app's shim, claiming the MIME types the bound app's own package
   declares, plus a `mimeapps.list` making the link handler the default. The
   directory goes first on `XDG_DATA_DIRS`, and its `xdg/mimeapps.list` copy
   first on `XDG_CONFIG_DIRS`.

When the sandboxed app runs `firefox <url>` (or `xdg-open <url>`), the helper
sends the target app name + args over the socket; the listener validates it
against the allowed set and runs `wryayer run <app> -- <args>` on the host. The
listener carries `PR_SET_PDEATHSIG` so it dies with the sandbox.

Step 4 is what makes *link clicks* work, as opposed to command lines.
Thunderbird never runs `firefox`: it asks the desktop which application handles
`x-scheme-handler/https`, and a container holding one app has no entry to
answer with, so the click does nothing at all. The generated entries are that
answer. Types the sandboxed app declares itself (Thunderbird's `mailto:`,
`message/rfc822`, …) are left out of them, so the app keeps answering for its
own — an unclaimed type is what makes GIO fall through to the container's own
entry.

### Avahi / zeroconf

Electron/Chromium, KDE and CUPS-linked apps probe Avahi over the system bus.
When no Avahi is reachable, `avahi-client` prints *"Failed to connect to Avahi
server: Daemon not running"* — and `avahi_client_new()` reaches that failure by
*blocking* on `Server.GetAPIVersion` / `Server.GetState` at startup, so merely
owning the name is not enough to silence it; something has to answer those calls.

The `avahi` setting (in `[network]`, default `stub`) picks how that happens:

- **`stub`** — each sandbox gets a *private* system bus: `run.rs` writes a
  throwaway `dbus-daemon` config into the app's `.spoof/` dir, spawns the daemon,
  and runs `wryayer avahi-stub` (see `avahi_stub.rs`) as a client that claims
  `org.freedesktop.Avahi` and answers the handful of `Server` methods
  `avahi_client_new()` needs (`GetAPIVersion → 516`, `GetState → RUNNING`, …).
  The private socket is bind-mounted over `/run/dbus/system_bus_socket` (after the
  `--bind /run /run`, so it overrides the host bus) and `DBUS_SYSTEM_BUS_ADDRESS`
  is pointed at it. The stub has no networking code, so it can never advertise the
  machine on the LAN, and the bus socket / config / readiness marker all live
  under `~/.wryayer/<app>/.spoof/` — nothing identifying is written outside the
  container. The stub process (and, via `PR_SET_PDEATHSIG`, its `dbus-daemon`)
  die with the sandbox. Actual browsing returns "nothing found", the honest
  answer for an isolated sandbox. The D-Bus wire protocol is marshaled by hand in
  `avahi_stub.rs` to avoid pulling in a D-Bus client crate.
- **`host`** — start the real host `avahi-daemon` best-effort
  (`systemctl start avahi-daemon`, falling back to a non-interactive `sudo`);
  does nothing if the unit is absent, already active, or the start is not
  permitted. This is a host-wide change and advertises the host on the LAN.
- **`off`** — do nothing; the harmless warning remains.

---

## Manifests and the alias / merge model

Each app dir carries a `.manifest.toml`:

```toml
[app]
name = "neovim"
main_binary = "nvim"
installed_at = "2026-07-02T…"
launchers = ["nvim"]
# alias_of = "…"        present only on alias dirs
# display_name = "…"    optional TUI label
# pkg_name = "…"        set when the app dir name differs from the package name
# [app.wine_game]       present only for imported wine games (exe + prefix)

[[packages]]
name = "neovim"
version = "0.10.0-1"
source = "official"     # or "aur"
```

`install <pkg> --into <target>` (merge mode) extracts `<pkg>` into the
**target's** tree (reusing deps already there), appends the new packages to the
target's manifest, and writes a **thin alias dir** at `~/.wryayer/<pkg>/`
containing only a manifest with `alias_of = <target>` plus its own `config.ini`.
The alias is a first-class entry in `list`/`tui`, gets its own `/usr/bin` launcher,
and its launcher calls `wryayer run <alias>` — which follows `alias_of` to the
real tree but reads the alias's own config. Removing an alias touches only the
alias dir + launcher; removing a target that still has aliases is refused
(unless `--cascade`).

---

## Snapshots

`wryayer snapshot` clones the live app dir into
`~/.wryayer/<app>/.snapshots/<timestamp>/` using hard links — instant and
near-free in disk space. Rollback restores the live tree from a chosen snapshot.

Snapshots survive updates because an update builds a fresh tree and swaps it in
rather than overwriting in place: re-extracted files get fresh inodes while the
snapshot's hard links keep pointing at the old content. `update.rs` carries the
`.snapshots` dir (`SNAP_DIR`) across the swap alongside the sandbox home and
config. Snapshots are excluded from `list` size totals, `dedup`, and the export
zip (via a `.snapshots` recursion guard).

---

## Update / reinstall internals

`update.rs::reinstall()` applies updates through a **staging tree and atomic
swap**, so the live app is never left half-wiped even if the process is killed
or the machine loses power mid-update:

1. Re-resolves the app's full dependency tree.
2. **Re-resolves every merged-in child** (`alias_of == app`) and folds their
   dep trees into the set — otherwise the fresh tree would be missing the child
   binaries. Any resolve failure bails *before* anything destructive, so a
   transient error can never damage the live tree.
3. **Plans a delta** (`plan_delta`): the resolved set is compared against the
   installed versions (the target manifest plus every child's). The `changed`
   set is the packages whose version differs or that are new. A delta is used
   unless `--full` is passed, the live tree is missing, nothing is installed, or
   a package *disappeared* — a removal needs a clean rebuild because an in-place
   overlay can't know which files the vanished package owned.
4. Downloads/builds only the `changed` packages (delta) or every package (full).
   Unchanged packages are never re-downloaded — the download cache would hit
   anyway when warm, but the delta skips the request entirely.
5. Builds a fresh sibling staging tree `.<app>.wr-new` and stamps the new
   manifest there — the live tree is untouched during this slow, fallible work.
   In a **delta** the staging tree starts as a hard-linked clone of the live
   package files (`clone_package_tree`, skipping `home`/`config.ini`/`.snapshots`/
   the manifest), then the changed packages are overlaid on top; a **full**
   rebuild extracts everything into an empty staging tree. Because
   `extract_package` unlink-firsts, overlaying a changed file writes a fresh
   inode and never mutates the shared clone/snapshot inode.
6. Swaps it in with two atomic renames: the old tree is parked as
   `.<app>.wr-old`, then the staging tree is moved into place. An **encrypted**
   app cannot be swapped this way — its directory is a VeraCrypt mount point,
   which the kernel refuses to rename, and nothing outside the container shares
   its filesystem, so even the hard-linked delta clone would fail with `EXDEV`.
   There `SwapLayout` puts the scratch space *inside* the tree (`.wr-new`,
   `.wr-old`, `.wr-phase`; the `.wr-` prefix is reserved) and swaps by moving
   top-level entries: every live entry into `.wr-old`, then every built entry
   out of `.wr-new`. Both halves leave entries spread across all three places,
   so `.wr-phase` records which half is running — written atomically, because
   reading it wrong is the one way to mix two versions of a tree.
7. Carries the user data (`home`, `config.ini`, `.snapshots`) from the parked
   old tree into the new one, then drops the old tree. `carry_over_user_data`
   **merges** rather than skips on collision, so a package-provided empty
   `home/` skeleton (the `filesystem` package ships one) can never shadow — and
   then get deleted with — the real profile.
8. Restores base symlinks, fixes permissions, re-runs the soname scan and
   `ldconfig`, regenerates runtime caches, and runs cross-app dedup.

`recover_interrupted_update()` runs at the start of every update and every
launch: it finishes a swap that was interrupted forward, or restores the parked
old tree if the new one never landed — so an interrupted update always heals to
a consistent, fully-extracted tree on the next run. It heals both swap forms,
and a launch runs it again after unlocking, since an encrypted app's scratch
space is invisible until its container is mounted. Where the in-place recovery
has to guess — a phase marker that never made it to disk — it rolls back, the
branch that trusts nothing it has not verified.

### Trade-off: delta cruft

A delta overlays new package versions onto the cloned tree but can't remove a
file that only the *old* version of a still-present package shipped (we don't
track per-package file lists). In practice this is rare — most files overwrite
in place at a stable soname path — and any package *removal* already forces a
full rebuild. `wryayer update --full` re-extracts into an empty tree and clears
any such residue.

### Package verification (`distro.rs`)

Because wryayer downloads packages itself instead of letting the package manager
install them, `download_pkg` authenticates every archive **before** it reaches
`extract_pkg`: Arch verifies the detached `.sig` against the pacman keyring with
`gpg`, Fedora runs `rpmkeys --checksig` against the rpm keyring, and Debian
treats an apt "cannot be authenticated" warning as fatal (apt otherwise vouches
for the `.deb` via the signed Release → Packages hash chain). AUR packages are
locally built, so they have no repo signature to check. `WRYAYER_SKIP_SIG_VERIFY=1`
bypasses verification. The pure verdict helpers (`rpm_checksig_ok`,
`apt_reports_unauthenticated`) are unit-tested.

`check_all_updates()` returns a `name → latest_version` map for every non-alias
app with a newer version available; the TUI runs it on a background thread at
startup and after each reload to drive the update dots, and `Shift+U` updates
every out-of-date app at once.

---

## Encrypted containers (`veracrypt.rs`, `secrets.rs`, `entropy.rs`, `commands/encrypt.rs`)

An encrypted app's tree lives inside a VeraCrypt volume at
`~/.wryayer/.containers/<app>.hc`, mounted **over** `~/.wryayer/<app>/`. Mounting
over the app's normal path is what makes the feature cheap: every other
subsystem — bwrap, update, snapshot, dedup — keeps operating on the same path it
always used and needs no knowledge of encryption. While locked it simply sees an
empty directory.

wryayer shells out to the `veracrypt` binary rather than driving `cryptsetup`
(which can open VeraCrypt volumes but not create them), so a container wryayer
makes is an ordinary VeraCrypt volume the user can open anywhere. Passwords are
fed on **stdin** (`--stdin`), never `--password=`, so they never appear in
`/proc/<pid>/cmdline`.

### Locked-state marker

`.containers/<app>.toml` records name, launchers, `alias_of`, display name and
`password_source`. `list_all_apps` and `read_manifest_or_marker` fall back to it
so a locked app still lists and can still be removed. It deliberately does
**not** carry the package list — that stays inside the container, so a locked
app reveals nothing about its contents.

It lives *beside the container*, not inside the app directory, because that
directory is a mount point: a marker there is hidden exactly when the container
is mounted, which makes it unwritable precisely when settings change. Keeping it
outside means it is readable and writable in both states — which is what lets
`password_source` be consulted while locked, the one moment the unlock path
needs it and `config.ini` (which lives *inside* the container) cannot be read.
`config::write_config` mirrors the setting across. The old in-app-directory
location is still read, so containers made before the move keep working.

### Root privileges

VeraCrypt needs root to attach a loop device, and its own escalation path is
unusable here: with `--non-interactive` it cannot ask for an admin password and
just fails, and without it, it prompts on a terminal a TUI-spawned process does
not have. So wryayer invokes `sudo veracrypt` itself. sudo reads its own password
from `/dev/tty` or a cached ticket, leaving stdin free for the volume password —
the two secrets never contend for the same channel. `prime_sudo` (`sudo -S -v`)
lets the TUI cache credentials from a password typed in an overlay, so the
install subprocess runs with fully piped stdio and its log stays in the TUI.

Running veracrypt as root means the container file and the freshly formatted
filesystem come out root-owned, so `create` chowns the file back and
`ensure_owner_writable` chowns the mount point. ext4 has no `uid=` mount option,
so this is the only way. `mkfs.ext4` also leaves a root-owned `lost+found`
inside an otherwise user-owned tree; it is chowned on every mount, because
leaving one unreadable directory there breaks every consumer that walks the app
tree (`export` used to abort its whole archive on the failed `read_dir`).

### Conversion is a rollback-safe swap

`commands::encrypt::run` orders its steps so that **the staging directory
`.<app>.wr-plain` existing means the conversion did not finish**, whatever else
is on disk: the tree is renamed aside (atomic) → container created → marker
written and container mounted → tree copied in → only then is staging deleted.
`recover_interrupted_encrypt` therefore rolls the whole thing back rather than
guessing how far it got — discarding a half-filled container is free, losing the
app is not. It is called from `run` and from `encrypt` itself, mirroring
`recover_interrupted_update`.

The copy preserves hard links via a `(dev, ino)` map. Without that, snapshots and
deduplicated libraries would each get their own inode and could multiply the
tree's real size several times over, overflowing a container sized from the
deduplicated total.

### Sizing

`recommended_size` runs *after* the install, against the finished tree, so it
measures rather than guesses: `used + headroom + ext4 overhead`, headroom being
half the tree clamped to 512 MiB…2 GiB, rounded up to 128 MiB. Small apps get
generous absolute room; large ones get proportionally less.

### Password sources and re-locking

`password_source = prompt` unmounts the container when the app exits, so the
password is genuinely required each launch. This is why `run/mod.rs` **disables
its `exec()` fast-path** for such apps — an `exec` would replace the process and
leave nothing to unmount afterwards, so the repaired-relaunch branch becomes a
spawn+wait. Unmount failure is never fatal: a second running instance keeps the
filesystem busy and the kernel refuses, which is the correct outcome.

`password_source = master` reads from `secrets.rs`: Argon2id stretches the master
password to a 256-bit key and AES-256-GCM encrypts the payload. "Type it once per
boot" needs no daemon — the *derived key* is cached in `$XDG_RUNTIME_DIR` (tmpfs,
gone on reboot) alongside the salt it came from, so re-keying invalidates the
cache automatically. `Store` has a hand-written `Debug` that redacts, because a
derived one would print every container password into any panic message.

### Merge installs and container growth

`install --into <encrypted-app>` writes into the target's container rather than
creating one, so the encryption prompt is skipped entirely. `install::run`
unlocks the target first — without that, every extracted file would land in the
directory *underneath* the mount point and disappear the moment it was mounted —
and keeps the password, because growing the container needs it again.

Space is enforced in two places. Once up front, when the archive sizes are known
but nothing has been written yet, and then again through a `SpaceGuard` handed
to the soname-satisfy loop: that loop discovers and extracts further packages
*after* the install was sized, and a single missing `libGL` can pull in an
entire graphics driver. `ensure_room_for` grows the volume by the shortfall plus
half again, so a run of merge installs doesn't rebuild the container each time.

Growth is create-bigger, copy, swap — VeraCrypt cannot resize in place. The
original container is only replaced once the new one holds a complete copy, so
an interruption leaves the app exactly as it was.

`commands::encrypt::grow` exposes the same operation directly, for the case
where nothing is being installed and the app has simply outgrown its volume.
Without `--to` it re-applies `recommended_size` to the data currently held,
floored at half again the current size — otherwise re-sizing a half-full
container would "grow" it to roughly what it already is.

### Reporting how full a container is

`veracrypt::usage` wraps `statvfs` into `used` / `available` / `total`, and
`Usage::percent_used` divides by `used + available` rather than `total`. The
difference is ext4's root reserve: dividing by the nominal size reports a
container the app can no longer write to as ~95% full, which is worst-case wrong
precisely when the user needs to act. It rounds up for the same reason.

`usage` deliberately does *not* check `is_mounted` first — that costs a fork, and
`SpaceGuard` calls `free_space` (built on `usage`) before every package extract.
Callers know their own mount state; `statvfs` on an unmounted mount point
silently describes whatever `~/.wryayer` sits on.

Three places surface it, all sharing `FULL_WARN_PERCENT`: the `encryption` status
table, the TUI details pane (via `EncState::fill`, refreshed on the same
once-a-second throttle as the mount scan), and `run`, which warns after
`ensure_unlocked` and before the app can start writing.

### Child output is not safe to draw

`tui::sanitize_log_line` runs on every line entering the operation log, at the
point `spawn_wryayer` reads it. Log lines end up in a ratatui `Paragraph`, which
passes their bytes to the terminal untouched — so anything a child emitted *for*
a terminal acts on the TUI's own screen.

`veracrypt --text --create` is the worst offender: it draws progress by
rewriting one line with `\r` and emits no newline until it finishes, so
`BufRead::lines()` yields the entire creation as a single line — 527 characters
with ten carriage returns, measured. Each one returned the cursor to column 0
mid-frame. Only the segment after the final `\r` is current, which is what a
terminal would have been showing, so that is what survives; escape sequences,
tabs and other control bytes go with it.

Sanitising at the reader rather than at draw time keeps the stored log clean for
everything that reads it, and leaves the `PROGRESS` / `PROMPT_*` protocol lines
the receiving end parses by prefix untouched (they are plain ASCII).

One consequence worth knowing: because veracrypt withholds its newline, its
progress is invisible until creation finishes. Live progress would mean reading
by byte and splitting on `\r`, which would then flood the log with one line per
update — a separate trade-off, not made here.

### The root has to be the root

`manifest::wryayer_root` refuses to hand out a path when `~/.wryayer` has been
seen on its own filesystem before but isn't on one now. An unmounted mount point
is an ordinary empty directory: without this check a boot that happened before
the container was mounted read nothing and wrote everything underneath the mount
point — installs, and a second `.passwords.vault` created by the "no store yet"
path in `obtain_password`. Mounting hid the shadow copy; the next boot restored
it, and the master password stopped working.

Detection is `st_dev(root) != st_dev(root.parent())`, which is stable across
reboots in a way the device number itself is not (a VeraCrypt volume lands on a
different `dm-` minor each time). The marker lives in `$XDG_STATE_HOME` because
its whole job is to be readable when `~/.wryayer` is not, and it holds one bit
and no app names.

The marker is deliberately one-way. A container that failed to mount looks
exactly like one the user has stopped using, and only the second is safe to
adopt silently — so "root is now a plain directory" never clears it.
`WRYAYER_ALLOW_UNMOUNTED_ROOT=1` does, once.

The verdict is cached in a `OnceLock`: this sits underneath every `app_dir` call,
including per-app-per-frame lookups in the TUI, and neither the mount table nor
the marker changes meaningfully within one run.

### Two front-ends, one set of rules

Everything the front-ends need to *know* about containers lives in
`commands::encrypt`, not in either of them: `AppEncryption` and `scan` (locked /
master-backed / how full, from a single `veracrypt --list` snapshot however many
apps there are), `password_source`, `apps_relying_on_the_store`. The TUI's
`EncState` is a re-export. `child_output::sanitize_line` is shared the same way —
the TUI would have its layout wrecked by a stray carriage return and the GUI would
render mojibake, but the fix is identical.

The same rule holds outside encryption, and for the same reason — two
front-ends that answer a question differently are two chances to be wrong:

| Shared | What both front-ends get from it |
|---|---|
| `child_output::classify` | The `PROGRESS` / `PROMPT_*` protocol a child speaks. A child has no terminal, so where the CLI would ask, it prints a line and exits; both front-ends put the question to the user and re-run the command with the answer folded in |
| `commands::run::running_instances` | How many sandboxes of each app are up, from one walk of `/proc` |
| `commands::run::sandbox_ram` | A ram-limited sandbox's live usage, read from the `/proc/meminfo` overlay the launcher maintains |
| `launcher::shortcut_plan` | Which `/usr/bin` paths a relink would write, and which it would leave alone with the reason — shown before the password is asked for, not after the fact |

What differs is only presentation. The TUI walks its password prompts one screen
at a time because it has one screen; the GUI shows the same set as a single form
(`gui::encryption::Needs` decides which fields appear, by the same rules as
`tui::build_secret_stages`). Both end at the same place: `--encrypt-secrets-stdin`
on the child, because neither has a terminal a child could prompt on.

### Converting an installed app from the TUI

`EncryptionRows` decides which rows a per-app config screen shows — `Offer` for
a plain app, `Manage` for an encrypted one, `Hidden` for an alias or when
veracrypt isn't installed. It replaced a bare `is_encrypted: bool` throughout
`config_sections` / `config_nav_order` / `config_nav_step`, so the renderer and
↑/↓ navigation cannot disagree about which rows exist.

The choice popup (`Screen::AskEncrypt`) is shared with the install-time prompt
and keyed on `EncryptAsk`. The two paths need separate choice tables because
`install` spells the options `--encrypt-master` / `--encrypt-generate` while
`encrypt` spells them `--master` / `--generate`; index 0 is the back-out row in
both, which is what `on_ask_encrypt` keys its cancel path on.

`encrypt` and `decrypt` take the same hidden `--encrypt-secrets-stdin` as
`install`, because the TUI owns the terminal — a child that prompted would be
painted over by the progress bar. `resolve_password_with` uses a supplied master
password to *open* the store rather than as the answer, which caches this boot's
key so nothing later in the operation drops to a prompt either.
`open_container_stages` is the shared "what still needs asking to mount an
existing container" rule, used by merge installs and by decryption.

### Guardrails

`require_unlocked` fails `repair`, `snapshot`, `rollback`, `export` and
`update --check` on a locked app. This matters beyond convenience: the app
directory is a mount point, so an operation run while locked would write its
result into the *underlying* directory, where the next mount would hide it.

`update` is the exception: naming an app is a request to update *that* app, so
it opens the container itself through `open_for_operation`, which returns an
`OpenContainer` guard that re-locks on drop — including on the error path, and
only for a container it actually opened. Across all apps there is no one
password to ask for, so the sweep unlocks only what `can_open_unattended`
answers for (a master-store app whose store is open this boot) and skips the
rest, as before.

### Password generation (`entropy.rs`)

`generate_password(len)` runs in four stages: collect, extract, expand, select.

**1. Collect.** Every source is folded into one SHA-512 state, each one
length-prefixed so that two different splits of the same bytes can't produce the
same input:

| Source | What is read |
|---|---|
| `/dev/urandom` | 64 bytes — the load-bearing source |
| `/dev/random` | 32 bytes, non-blocking |
| hwmon / thermal | every `/sys/class/hwmon/*/temp*_input` and `/sys/class/thermal/thermal_zone*/temp` |
| mouse position | `hyprctl cursorpos` (Wayland), else `xdotool getmouselocation` (X11/XWayland), else up to 32 bytes of movement deltas from `/dev/input/mice` |
| RAM usage | `/proc/meminfo` |
| scheduler | `/proc/stat` |
| interrupts | `/proc/interrupts` — per-device IRQ totals, which move on every keystroke |
| clock | `SystemTime` nanoseconds and the pid, then a *second* nanosecond read after collecting; the gap between them reflects how long collection actually took under live load |

Anything unavailable contributes nothing. A `SourceReport` records which sources
fired, which is what `wryayer genpw` prints on stderr.

**2. Extract.** `seed = SHA-512(collected)` — 64 bytes.

**3. Expand.** A hash-based DRBG in counter mode: block `i` is
`SHA-512(seed ‖ i)` with `i` as a little-endian `u64`, consumed byte by byte.
Output blocks reveal nothing about the seed or about each other.

**4. Select.** `below(n)` draws a byte and **rejects** anything at or above
`256 - (256 % n)`, redrawing instead — so every character is exactly equally
likely. A plain `% n` would over-represent the first `256 % n` characters of the
alphabet. One character is drawn from each of the four classes first so each is
guaranteed present, the rest come from the full alphabet, and a Fisher-Yates
shuffle (over the same stream) mixes them so the guaranteed characters aren't
always in front.

The alphabet is 90 characters — 26 lowercase, 26 uppercase, 10 digits and 28
symbols. Quotes, backslashes and backticks are excluded because these passwords
end up in shell-adjacent contexts; that costs about 0.05 bits per character.
At log2(90) ≈ 6.49 bits each, the 32-character default is ≈ 207 bits.

**On the extra sources.** `/dev/urandom` alone is already cryptographically
secure and nothing else here improves on it. Because everything is combined by
hashing, the auxiliary sources can only ever *add* — `SHA-512(strong ‖ weak)` is
no weaker than `SHA-512(strong)`. They exist for one specific failure mode: a
CSPRNG that is broken or unseeded (a freshly imaged VM, a container with a cloned
entropy pool, a kernel RNG bug), where sensor noise, cursor position and
cycle-level timing are the only things distinguishing two otherwise identical
machines.

**Liveness.** Every device read is non-blocking, and the mouse fallback is
`poll(2)`-bounded to 50 ms, so an unseeded `/dev/random` or a mouse nobody is
touching can never stall generation.

### AUR build quirks (`package/download.rs`)

- **`makepkg -df`** — `-f` overwrites a `.pkg.tar.zst` left in the clone dir by
  a prior build, so rebuilds don't abort with *"A package has already been
  built."*
- **debug subpackage** — split debug packages are named
  `<pkgbase>-debug-<ver>-<arch>.pkg.tar.zst`, with `-debug-` in the middle.
  `find_pkg_tarball` excludes any name containing `-debug-`, otherwise readdir
  order could hand back the symbols-only package and the real payload would
  never be extracted.
- **nw-builder proxy patch** — `nw-builder 3.8.3` forces a proxy tunnel; the
  build retries with `--noextract` after patching `rq.proxy = false`.

---

## Developing and testing

### Build

```fish
cargo build            # debug
cargo build --release  # optimized
```

### Run tests

Tests that touch the filesystem isolate themselves by temporarily redirecting
`HOME` to a temp directory. They all take the single lock in `test_support.rs`
while they do, and under `cfg(test)` `manifest::wryayer_root` refuses to hand
back a path outside that temp directory — so the suite is safe to run in
parallel:

```fish
cargo test --all-features
```

### Test coverage

The suite targets **≥ 90 % branch coverage** on all pure and
filesystem-dependent logic, via **equivalence-class partitioning** (one
representative per class) plus explicit boundary and error-path tests.

| Module / test file | What is covered |
|---|---|
| `config.rs` (`config_tests.rs`) | `parse_ini` (all keys, all enum variants, error paths, `ram_limit` disable aliases / integers / absent), `format_ini` (`[resources]` presence/absence), `parse_bool`, round-trip |
| `config.rs` (`global_config_tests.rs`) | `read_global_config` fallback when file absent, write+read round-trip |
| `manifest.rs` | `write_manifest`/`read_manifest` round-trip, `list_all_apps` (empty, sorted, skips bad dirs), atomicity |
| `launcher.rs` | `create_launcher` (content, permissions), `remove_launcher` (missing, non-wryayer, valid) |
| `commands/dedup.rs` | `format_bytes` (4 EC + 7 boundaries), `du_walk` (SKIP_DIRS, hard-link accounting) |
| `package/deps.rs` | `strip_version_constraint` (7 operators), `is_soname_dep` (5 EC), `parse_pacman_field`, `parse_pacman_depends` (5 EC) |
| `commands/run/` (`run_tests.rs`) | Arg stripping (5 cases), `no_other_instance`, `has_systemd_run`, `wrap_with_ram_limit` (program, `--user`/`--scope`/`--quiet`, `MemoryMax`, `MemorySwapMax=0`, `--` separator, inner args, env transfer) |
| `commands/install.rs` | `ensure_base_layout` (creates all symlinks, idempotent, preserves real dirs) |
| `commands/snapshot.rs` | `create` / `labels` / `latest` round-trip, inode sharing, `.snapshots` recursion guard, `rollback` (restores modifications, errors on missing label, preserves snapshots dir) |
| `commands/remove.rs` + alias model | `alias_of` serde round-trip, `skip_serializing_if`, legacy manifests parse, `list_all_apps` surfaces aliases, removing an alias leaves the target intact, removing a target with dependents is blocked with blockers named |
| `tui/mod.rs` (`option_picker_tests.rs`) | `setting_options` / `setting_title` / `setting_description` / `option_description` / `setting_current` / `apply_setting` / `cycle_setting` — full forward/backward/wrap cycles for all non-empty rows |
| `tui/mod.rs` | `parse_progress` parsing, konami FSM |
| `desktop.rs` | Entry rewriting (exec / icon / owner / field codes), `mimeapps.list` editing, and the entries generated for a sandbox's bound apps |
| `commands/update.rs` (`update_recovery_tests.rs`) | The in-place swap's phases and the direction each one heals in |
| `commands/install.rs` (`install_tests.rs`), alias model (`alias_tests.rs`), `package/deps.rs` (`deps_tests.rs`) | Base layout, alias resolution, dependency parsing |
| `cpu.rs`, `entropy.rs`, `distro.rs`, `avahi_stub.rs` | Preset/topology rendering, pool mixing and generator bias, backend detection and version compare, D-Bus message dispatch |

External-tool-dependent code (`bwrap_cmd`, `reinstall`, distro backends) is
covered by integration tests that require a live environment with `bwrap` and
either `pacman` (Arch) or `apt`/`dpkg` (Debian/Ubuntu) present.
