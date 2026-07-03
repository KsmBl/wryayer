# wryayer — internals & architecture

> This document is for contributors and the curious: how wryayer is built, how
> the on-disk layout works, how the sandbox is assembled at runtime, and how to
> build and test it. If you just want to **install and use** apps, read
> [`README.md`](README.md) instead.

---

## Source tree

```
src/
├── main.rs            ← clap CLI entry point; dispatches subcommands
├── lib.rs             ← module wiring; exposed for the integration tests
├── manifest.rs        ← .manifest.toml read/write, app dir helpers, list_all_apps
├── config.rs          ← AppConfig, INI parse/format, global defaults.ini
├── launcher.rs        ← ~/bin/<app> shell wrapper create/remove
├── distro.rs          ← per-distro backend (pacman / apt / dnf), auto-detected
├── package/
│   ├── deps.rs        ← BFS dependency resolver, virtual/soname fallback
│   ├── download.rs    ← official download + AUR git clone/makepkg build
│   ├── extract.rs     ← unpack .pkg.tar.zst / .deb / .rpm into an app tree
│   └── soname_check.rs← scan ELF NEEDED entries, find owning packages
├── commands/
│   ├── install.rs     ← resolve → download → extract → manifest → dedup
│   ├── install_game.rs← wine-container import (folder → .exe → prefix)
│   ├── run.rs         ← assemble and exec the bwrap sandbox
│   ├── update.rs      ← re-resolve + re-extract; version checks
│   ├── remove.rs      ← delete tree + launcher; alias-aware
│   ├── snapshot.rs    ← hard-linked snapshots + rollback
│   ├── export.rs      ← zip an app tree with progress markers
│   ├── import.rs      ← recreate an app from an exported zip
│   ├── dedup.rs       ← cross-app hard-link identical files; du accounting
│   ├── repair.rs      ← resolve+install packages for missing sonames
│   ├── list.rs        ← table + apparent/on-disk/savings totals
│   └── config.rs      ← `wryayer config` CLI surface
└── tui/
    ├── mod.rs         ← App state, event loop, all key handling
    ├── ui.rs          ← ratatui rendering for every screen
    └── konami.rs      ← easter-egg FSM
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
              │  │ Import    │ Space     │   config    export  import │
              │  │ Settings (global cfg) │   snapshot  rollback  tui  │
              │  └───────────────────────┘   snapshots  snapshot-prune│
              │                              dedup      completions   │
              └────────────────────────┬─────────────────────────────┘
                                       │
          ┌────────────────────────────┼──────────────────────────────┐
          │                      Core layer                           │
          │                                                           │
          │  manifest.rs           config.rs          launcher.rs     │
          │  ┌──────────────┐   ┌────────────────┐  ┌─────────────┐   │
          │  │ .manifest.   │   │ config.ini     │  │ ~/bin/<app> │   │
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

~/bin/
├── firefox    ──►  exec wryayer run firefox "$@"
├── fastfetch  ──►  exec wryayer run fastfetch "$@"
├── hyfetch    ──►  exec wryayer run hyfetch "$@"   (bwrap roots on fastfetch/)
└── vlc
```

Everything wryayer writes lives under `~/.wryayer/` (state, snapshots, spoof
files, global defaults) and `~/bin/` (launchers). Build/download caches live
under `~/.cache/wryayer/{pkg,build}`. Nothing is written elsewhere.

---

## The bwrap sandbox

At runtime `commands/run.rs` builds a `bwrap` command line. The app's own tree
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
(the D-Bus filter proxy, the Avahi stub bus) whose lifetimes are tied to the
sandbox. The
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
| cpuinfo | file bound over `/proc/cpuinfo` (`sample` preset or a user-edited file) |
| os-release | file bound over `/etc/os-release` **and** `/usr/lib/os-release` |
| terminal | walk the process tree to find the real terminal, set its env var (`TERM_PROGRAM`, `KITTY_WINDOW_ID`, …) |

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
The alias is a first-class entry in `list`/`tui`, gets its own `~/bin` launcher,
and its launcher calls `wryayer run <alias>` — which follows `alias_of` to the
real tree but reads the alias's own config. Removing an alias touches only the
alias dir + launcher; removing a target that still has aliases is refused
(unless `--cascade`).

---

## Snapshots

`wryayer snapshot` clones the live app dir into
`~/.wryayer/<app>/.snapshots/<timestamp>/` using hard links — instant and
near-free in disk space. Rollback restores the live tree from a chosen snapshot.

Snapshots survive updates because extraction **unlinks a file before
overwriting** it: a re-extracted file gets a fresh inode while the snapshot's
hard link keeps pointing at the old content. `update.rs` also lists
`.snapshots` (`SNAP_DIR`) in its `PRESERVE` set so the pre-extract wipe never
deletes them. Snapshots are excluded from `list` size totals, `dedup`, and the
export zip (via a `.snapshots` recursion guard).

---

## Update / reinstall internals

`update.rs::reinstall()`:

1. Re-resolves the app's full dependency tree.
2. **Re-resolves every merged-in child** (`alias_of == app`) and folds their
   dep trees into the set — otherwise the wipe-and-extract below would delete
   the child binaries. Any resolve failure bails *before* the wipe, so a
   transient error can never leave the tree missing a child.
3. Downloads/builds every package.
4. Wipes the tree except `PRESERVE = [.manifest.toml, config.ini, home,
   .snapshots]`, preserving user data and snapshots.
5. Re-extracts, rewrites the manifest, restores base symlinks, fixes
   permissions, re-runs the soname scan and `ldconfig`, regenerates runtime
   caches, and runs cross-app dedup.

`check_all_updates()` returns a `name → latest_version` map for every non-alias
app with a newer version available; the TUI runs it on a background thread at
startup and after each reload to drive the update dots, and `Shift+U` updates
every out-of-date app at once.

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
`HOME` to a temp directory. Run single-threaded to avoid races on `HOME`:

```fish
cargo test -- --test-threads=1
# or
RUST_TEST_THREADS=1 cargo test
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
| `commands/run.rs` | Arg stripping (5 cases), `no_other_instance`, `has_systemd_run`, `wrap_with_ram_limit` (program, `--user`/`--scope`/`--quiet`, `MemoryMax`, `MemorySwapMax=0`, `--` separator, inner args, env transfer) |
| `commands/install.rs` | `ensure_base_layout` (creates all symlinks, idempotent, preserves real dirs) |
| `commands/snapshot.rs` | `create` / `labels` / `latest` round-trip, inode sharing, `.snapshots` recursion guard, `rollback` (restores modifications, errors on missing label, preserves snapshots dir) |
| `commands/remove.rs` + alias model | `alias_of` serde round-trip, `skip_serializing_if`, legacy manifests parse, `list_all_apps` surfaces aliases, removing an alias leaves the target intact, removing a target with dependents is blocked with blockers named |
| `tui/mod.rs` (`option_picker_tests.rs`) | `setting_options` / `setting_title` / `setting_description` / `option_description` / `setting_current` / `apply_setting` / `cycle_setting` — full forward/backward/wrap cycles for all non-empty rows |
| `tui/mod.rs` | `parse_progress` parsing, konami FSM |

External-tool-dependent code (`bwrap_cmd`, `reinstall`, distro backends) is
covered by integration tests that require a live environment with `bwrap` and
either `pacman` (Arch) or `apt`/`dpkg` (Debian/Ubuntu) present.
