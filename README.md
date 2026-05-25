# wryayer

> Isolated per-app package management — no root, no containers, no daemon.
> Supports **Arch Linux** (pacman + AUR), **Debian / Ubuntu** (apt), and **Fedora / RHEL** (dnf/rpm).

[![License: LGPL-3.0-or-later](https://img.shields.io/badge/License-LGPL%203.0-blue.svg)](LICENSE)
[![Platform: Arch / Debian / Fedora](https://img.shields.io/badge/platform-Arch%20%7C%20Debian%20%7C%20Fedora-blue)](https://github.com/KsmBl/wryayer)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange?logo=rust)](https://rustup.rs)

wryayer installs packages into fully-isolated per-app directory trees under `~/.wryayer/<app>/`. Each app and all its transitive dependencies live in their own private filesystem root and are launched inside a **bubblewrap** (`bwrap`) sandbox. No root access, no systemd units, no Flatpak runtimes — just ordinary files, hard links, and Linux namespaces.

On **Arch Linux** it resolves and downloads packages via `pacman` and the AUR. On **Debian / Ubuntu** it uses `apt-get download` and `dpkg-deb`. On **Fedora / RHEL** and derivatives it uses `dnf download` and `rpm2cpio`. The distro is detected automatically from `/etc/os-release`.

---

## Why this exists

Arch Linux has one of the richest package ecosystems on the planet, but its single-root package model means:

- Installing an old or alternate version of an app is painful or impossible without AUR hacks.
- A poorly-packaged AUR tool can clobber shared libraries used by other apps.
- There is no per-app permission model: once installed, an app can read your entire home directory.

wryayer solves all three by extracting packages into self-contained directory trees that are bind-mounted as `/` at runtime. Apps can't see your home directory unless you explicitly share a folder. Conflicting dependency versions coexist without interference. Removing an app is a single `rm -rf`.

**It is not a security sandbox.** The goal is isolation and disk-space efficiency, not hardened confinement. A determined app can still escape via `/proc`, shared IPC, or device access; `audio=off` and `network=off` raise the bar but are not guarantees.

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
              │  │ Settings (global cfg) │   snapshot  rollback       │
              │  └───────────────────────┘   snapshots dedup          │
              │                            snapshots dedup completions│
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
          │  │  Arch: pacman -Si, pacman -Sp, tar --zstd, vercmp│     │
          │  │         AUR RPC + git clone + makepkg            │     │
          │  │  Debian: apt-cache show, apt-get download,        │     │
          │  │          dpkg-deb -x, dpkg -S, dpkg --compare-   │     │
          │  │          versions                                 │     │
          │  └──────────────────────────────────────────────────┘     │
          └───────────────────────────────────────────────────────────┘

Filesystem layout:
─────────────────
~/.wryayer/
├── firefox/                 ← isolated root (bind-mounted as / at runtime)
│   ├── usr/
│   │   ├── bin/firefox
│   │   └── lib/             ← shared libs, hard-linked with other apps
│   │        libz.so.1       │  where content is identical (dedup)
│   │        libpng.so.16    │
│   ├── etc/                 ← app-specific /etc
│   ├── .manifest.toml       ← package list + install metadata
│   └── config.ini           ← per-app sandbox settings
├── fastfetch/               ← target of an `install --into` chain
│   ├── usr/bin/{fastfetch,hyfetch}
│   └── .manifest.toml
├── hyfetch/                 ← thin alias dir — no extracted files
│   ├── .manifest.toml       ← alias_of = "fastfetch"
│   └── config.ini           ← independent sandbox config
└── vlc/
     └── ...

~/bin/
├── firefox    ──►  exec wryayer run firefox "$@"
├── fastfetch  ──►  exec wryayer run fastfetch "$@"
├── hyfetch    ──►  exec wryayer run hyfetch "$@"   (bwrap roots on fastfetch/)
└── vlc

bwrap sandbox at runtime:
──────────────────────────
~/.wryayer/<app>/   ──► /                   (app root, rw)
/dev                ──► /dev                (devices, configurable)
/proc               ──► /proc
/sys                ──► /sys                (read-only)
/run                ──► /run
/tmp                ──► /tmp                (system | tmpfs | local dir | uuid dir)
/etc/resolv.conf    ──► /etc/...            (read-only host network/identity files)
/etc/hosts               ...
/etc/ssl/certs           ...
/usr/share/fonts    ──► /usr/share/fonts    (read-only; required by Chromium/Electron/NW.js)
/etc/fonts          ──► /etc/fonts          (fontconfig configuration)
/usr/share/fontconfig ──► /usr/share/fontconfig
<shared_dirs>       ──► <same>              (user-configured, read-write)
```

---

## Supported distributions

| Distribution | Support | Notes |
|---|---|---|
| **Arch Linux** | ✅ Fully supported | pacman + AUR backend; actively tested |
| **CachyOS** | ✅ Fully supported | Arch-based; primary test environment |
| **Manjaro** | ✅ Fully supported | Arch-based; detected via `ID_LIKE=manjaro` |
| **EndeavourOS / Garuda / other Arch derivatives** | ✅ Fully supported | Detected via `ID_LIKE=arch` or presence of `/usr/bin/pacman` |
| **Debian 12 / 13** | ✅ Fully supported | apt + dpkg backend; actively tested |
| **Ubuntu 22.04 / 24.04** | ✅ Expected to work | Detected via `ID_LIKE=ubuntu`; same apt/dpkg toolchain as Debian — not separately tested |
| **Linux Mint** | ✅ Expected to work | Ubuntu-based; detected via `ID_LIKE=ubuntu`; not separately tested |
| **Fedora 38+** | ✅ Fully supported | dnf + rpm2cpio backend; actively tested |
| **RHEL / AlmaLinux / Rocky** | ✅ Expected to work | Same dnf/rpm backend as Fedora; detected via `ID_LIKE=rhel` |
| **Void Linux** | ❌ Not supported | Uses xbps — no supported backend |
| **openSUSE** | ❌ Not supported | Uses zypper — no supported backend |

Distro detection reads `/etc/os-release`. Distributions not listed above may work if they are closely derived from Arch or Debian and carry a matching `ID_LIKE` value, but are untested.

---

## Prerequisites

wryayer auto-detects your distro from `/etc/os-release` and uses the appropriate package backend. Install the tools for your distro before building.

### Arch Linux

| Requirement | How to install | Notes |
|---|---|---|
| **bubblewrap** | `sudo pacman -S bubblewrap` | Required at runtime |
| **Rust toolchain** | `curl https://sh.rustup.rs -sSf \| sh` | For building |
| **git** | `sudo pacman -S git` | AUR package builds |
| **base-devel** | `sudo pacman -S base-devel` | AUR builds (`makepkg`) |
| **yay** (optional) | AUR | Cache reused when present; fallback is `makepkg` |
| `vercmp` | Bundled with `pacman` | Version comparison |
| `ldconfig` | Bundled with `glibc` | Library cache rebuild after install |
| `glib-compile-schemas` | `sudo pacman -S glib2` | Optional — GLib apps only |

### Debian / Ubuntu

| Requirement | How to install | Notes |
|---|---|---|
| **bubblewrap** | `sudo apt install bubblewrap` | Required at runtime |
| **Rust toolchain** | `curl https://sh.rustup.rs -sSf \| sh` | For building |
| **binutils** | `sudo apt install binutils` | Provides `readelf` — used by soname scanner |
| **dpkg** | Pre-installed | Package extraction (`dpkg-deb`) |
| **apt** | Pre-installed | Dep resolution and download |
| `ldconfig` | `sudo apt install libc-bin` | Library cache rebuild after install |

> **AUR packages are Arch-only.** On Debian/Ubuntu, only packages from `apt` repos are available. Attempting to install an AUR-only package will print a warning and skip that dep.

---

## Building from source

```fish
# Clone
git clone https://github.com/KsmBl/wryayer.git
cd wryayer

# Build release binary
cargo build --release

# Install to ~/bin/ (already on PATH if you followed setup)
cp target/release/wryayer ~/bin/
```

For development builds (faster compile, debug symbols):

```fish
cargo build
cp target/debug/wryayer ~/bin/
```

---

## Shell completions

Generate and install completions once; they are auto-loaded by fish:

```fish
# fish
wryayer completions fish > ~/.config/fish/completions/wryayer.fish

# bash
wryayer completions bash >> ~/.bashrc

# zsh
wryayer completions zsh >> ~/.zshrc
```

Re-run after updating the binary to pick up new subcommands.

---

## Usage

### Install an app

```fish
# From the official repos or AUR — wryayer detects automatically
wryayer install firefox
wryayer install neovim

# Override the app directory name and/or the ~/bin/ launcher name
wryayer install python --app-name py312 --bin-name python3.12

# Multiple launchers from one package (e.g. a toolkit shipping several CLIs)
wryayer install imagemagick --bin-names convert,identify,mogrify

# Install additively into an existing app's directory — useful for plugins
# and multi-tool bundles. The new package's files land in the target's
# tree (sharing deps already extracted there), but the new package gets
# its own thin manifest dir at ~/.wryayer/<pkg>/ that carries an
# `alias_of` pointer back to the target. Each alias is a first-class
# entry in `wryayer list` and `wryayer tui`, gets its own ~/bin/<name>
# launcher, and can have its own sandbox config.
wryayer install neovim
wryayer install ripgrep --into neovim
wryayer install fd      --into neovim

# Pick a different name for the alias dir (defaults to the package name)
wryayer install hyfetch --into fastfetch --app-name hf

# Some packages install their binary under a name that differs from the
# package name (e.g. vivaldi installs as vivaldi-stable, google-chrome as
# google-chrome-stable). If the install fails with "binary not found", the
# error lists what IS in the bin dirs — re-run with --bin-names:
wryayer install vivaldi --bin-names vivaldi-stable
```

The resulting layout:

```
~/.wryayer/
├── neovim/
│   ├── usr/bin/nvim, rg, fd  ← all the real binaries live here
│   └── .manifest.toml         ← lists neovim, ripgrep, fd packages
├── ripgrep/
│   └── .manifest.toml         ← alias_of = "neovim", launchers = [rg]
└── fd/
    └── .manifest.toml         ← alias_of = "neovim", launchers = [fd]
```

Aliases run inside the target's tree but read **their own** `config.ini`, so
you can give a plugin a different sandbox profile (e.g. `network=off`) than
the host app. Removing an alias deletes only the alias dir + its launcher;
the target's tree is left intact. Removing a target while aliases still point
at it is refused with an explicit error listing the blocking aliases.

### Run an app

```fish
# Via the generated launcher (preferred)
firefox

# Via wryayer run
wryayer run firefox
wryayer run firefox -- --new-window        # pass -- to separate wryayer flags from app flags
wryayer run firefox -- ~/Documents/doc.pdf

# Multi-binary apps installed with --bin-names: pick which binary to invoke.
# Must be one of the launchers registered for the app.
wryayer run neovim --bin nvim

# Aliases (installed via --into) are run via their own alias name, not the target.
# After: wryayer install ripgrep --into neovim
wryayer run ripgrep -- --json "TODO" .   # or just: rg --json "TODO" .
```

### List installed apps

```fish
wryayer list
```

Output:

```
name       version      installed            size       launchers
------------------------------------------------------------------------
firefox    130.0.1-1    2026-05-10T14:23:00  847.3 MiB  firefox
vlc        3.0.21-1     2026-05-11T09:01:00  312.7 MiB  vlc
------------------------------------------------------------------------
apparent: 1.1 GiB   on disk: 960 MiB   saves: 200 MiB
```

### Remove an app

```fish
wryayer remove firefox

# Remove an app and all aliases that point at it in one shot
wryayer remove firefox --cascade
```

If the app has any aliases pointing at it (created via `install --into`),
removal is refused until those aliases are removed first — otherwise their
launcher scripts would silently target a missing directory. Use `--cascade`
to remove the target and all its aliases in one command. Removing the
alias itself is always safe and never touches the target tree.

### Update apps

```fish
wryayer update            # check and update all apps
wryayer update firefox    # update one app
wryayer update --check    # report available updates without installing
```

### Export and import

```fish
# Pack an app's entire directory tree into a portable zip
wryayer export firefox
wryayer export firefox --output /mnt/backup/firefox-2026.zip

# Import a previously-exported zip (re-creates the app as if freshly installed)
wryayer import firefox-2026.zip
```

The export progress bar in the TUI is real — wryayer pre-counts entries, then emits `PROGRESS n/total` markers during the zip write so the gauge and ETA reflect actual work done.

### Snapshot and rollback

Snapshots are **instant and near-free in disk space** — they create a hard-linked clone of the live app dir under `~/.wryayer/<app>/.snapshots/<timestamp>/`. Rollbacks atomically restore the live tree from a chosen snapshot.

```fish
# Snapshot the current state of an app
wryayer snapshot firefox

# List snapshots for an app, newest first
wryayer snapshots firefox

# Roll back to the most recent snapshot
wryayer rollback firefox

# Roll back to a specific labelled snapshot
wryayer rollback firefox 20260516-141022
```

Snapshots survive updates because wryayer unlinks any existing file before overwriting it during extraction — a re-extracted file always gets a fresh inode, while the snapshot's hard link continues pointing at the old content. The cross-app dedup pass at the end of every install re-establishes shared-library hard links.

Snapshots are excluded from `wryayer list` size totals, `wryayer dedup`, and the export zip.

### Deduplicate shared files

After installing multiple apps that share libraries, identical files are automatically hard-linked. Run manually at any time:

```fish
wryayer dedup           # silent
wryayer dedup --verbose # print every file linked
```

### Scan for missing shared libraries

If an app crashes with a missing `.so` error, this command finds and installs the missing package:

```fish
wryayer repair firefox
```

### Interactive TUI

```fish
wryayer tui
```

Key bindings:

| Key | Action |
|---|---|
| `Tab` / `Shift+Tab` | Switch tabs (Installed / Install / Import / Space / **Settings**) |
| `↑` / `↓` or `j` / `k` | Navigate lists |
| `r` | Run selected app |
| `d` / `Delete` | Remove selected app (double-confirm) |
| `e` | Export selected app to a zip |
| `p` | Snapshot selected app (hard-linked clone) |
| `o` | Roll selected app back to its latest snapshot |
| `u` | Update selected app |
| `c` | Check for updates |
| `s` | Open per-app config |
| `n` | Rename app (set display name) |
| `q` / `Esc` | Quit / close overlay |
| `t` | Toggle debug log during install/remove operations |
| `?` | Show key-bindings reference |
| `Shift+Q` | Force-quit from anywhere |

The **Settings** tab lets you edit global defaults applied to every newly installed app. Settings behave identically to per-app config but are stored in `~/.wryayer/defaults.ini`. Press `Enter` or `←`/`→` to change a value; press `Enter` on **Save & Close** to persist. Per-app overrides always take precedence over global defaults.

---

## Per-app configuration

```fish
# Show current config
wryayer config firefox

# Change settings
wryayer config firefox network off        # block internet access
wryayer config firefox audio off          # mute audio output + mic
wryayer config firefox tempmode ramdisk   # private in-memory /tmp
wryayer config firefox ramlimit 2048      # limit to 2 GiB RAM
wryayer config firefox ramlimit none      # remove RAM limit

# Shared directories (bind-mounted read-write into the sandbox)
wryayer config firefox share add ~/Documents
wryayer config firefox share add ~/Downloads
wryayer config firefox share remove ~/Documents
wryayer config firefox share list
```

### Config reference

| Setting | Values | Default | Description |
|---|---|---|---|
| `tempmode` | `system` `ramdisk` `local` `uuid` | `system` | How `/tmp` is provided inside the sandbox |
| `tempdelete` | `never` `on_start` `on_close` | `on_start` | When to clean up local temp dirs (only with `tempmode local`) |
| `network` | `on` `off` | `on` | Allow outgoing network access |
| `camera` | `on` `off` | `on` | Allow `/dev/video*` camera access |
| `microphone` | `on` `off` | `on` | Mask ALSA capture devices (see caveat below) |
| `audio` | `on` `off` | `on` | Mask ALSA + PipeWire/PulseAudio sockets |
| `share add <path>` | Any existing directory | — | Bind-mount `<path>` read-write inside the sandbox |
| `keyboard-layout` | `off` `us` `de` `colemak` `dvorak` | `off` (inherit host) | Inject `XKB_DEFAULT_LAYOUT` into the sandbox. `us` = QWERTY, `de` = QWERTZ, `colemak` and `dvorak` are ergonomic alternatives. `off` or `system` inherits the host compositor layout. |
| `ramlimit <MiB\|none>` | Integer (MiB) or `none` | `none` | Hard cap on RAM **and** swap combined, enforced via `systemd-run --scope -p MemoryMax=NM -p MemorySwapMax=0` (requires systemd). Both limits are necessary — without `MemorySwapMax=0` the kernel silently offloads pages to swap (including zram), letting the app exceed the cap. |

### Identity spoofing

Override what the sandbox reports about the host machine. Useful for preventing apps from embedding your real hostname, username, OS identity, or machine fingerprint in logs, telemetry, or profiles.

```fish
# Spoof /etc/hostname and $HOSTNAME
wryayer config firefox spoof-hostname myworkstation
wryayer config firefox spoof-hostname sample   # → "workstation"
wryayer config firefox spoof-hostname system   # disable

# Spoof $USER and $LOGNAME
wryayer config firefox spoof-username sample   # → "user"
wryayer config firefox spoof-username myname

# Spoof /etc/machine-id
wryayer config firefox spoof-machine-id system    # use real ID (default)
wryayer config firefox spoof-machine-id random    # fresh UUID every launch
wryayer config firefox spoof-machine-id sample    # → cafebabe0011223344556677deadbeef
wryayer config firefox spoof-machine-id a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4

# Spoof /proc/cpuinfo
wryayer config firefox spoof-cpuinfo sample        # built-in generic Intel i7
wryayer config firefox spoof-cpuinfo ~/fakecpu.txt # custom file

# Spoof /etc/os-release (hide real distro identity from the app)
wryayer config firefox spoof-os ubuntu      # present as Ubuntu 24.04 LTS
wryayer config firefox spoof-os arch        # present as Arch Linux
wryayer config firefox spoof-os windows     # present as Windows 11
wryayer config firefox spoof-os arduinoide  # present as ArduinoIDE
wryayer config firefox spoof-os fedora      # custom: any name works via "input" in TUI
wryayer config firefox spoof-os system      # disable

# Spoof terminal — fix fastfetch showing "bwrap" instead of your real terminal
wryayer config fastfetch spoof-terminal on   # detect kitty/foot/alacritty/… and set TERM_PROGRAM
wryayer config fastfetch spoof-terminal off  # disable (default)

# Set keyboard layout inside the sandbox
wryayer config firefox keyboard-layout de       # QWERTZ (German)
wryayer config firefox keyboard-layout us       # QWERTY (US English)
wryayer config firefox keyboard-layout colemak  # Colemak ergonomic
wryayer config firefox keyboard-layout dvorak   # Dvorak simplified
wryayer config firefox keyboard-layout off      # inherit from host compositor (default)

# Disable any spoofing
wryayer config firefox spoof-hostname system
```

All settings are editable in the TUI config screen (`s` on an installed app). Each row uses a picker; press `?` on any row or option to see a description of what the setting does.

Press `?` on the **installed** tab for a full key-bindings reference.

| CLI subcommand | Values | Effect |
|---|---|---|
| `spoof-hostname <value\|sample\|system\|off>` | Any string | Writes `/etc/hostname`, sets `$HOSTNAME` |
| `spoof-username <value\|sample\|system\|off>` | Any string | Sets `$USER` and `$LOGNAME` |
| `spoof-machine-id <system\|random\|sample\|hex\|off>` | See below | Writes `/etc/machine-id` |
| `spoof-cpuinfo <sample\|path\|system\|off>` | Path or `sample` | Binds the file over `/proc/cpuinfo` |
| `spoof-os <ubuntu\|arch\|windows\|arduinoide\|name\|system\|off>` | Preset or any OS name | Writes `/etc/os-release` and `/usr/lib/os-release` |
| `spoof-terminal <on\|off>` | `on` or `off` | Detects real terminal via process tree and sets `TERM_PROGRAM` inside sandbox |
| `keyboard-layout <layout\|off>` | `us` `de` `colemak` `dvorak` or `off` | Sets `XKB_DEFAULT_LAYOUT` inside the sandbox (`off` or `system` = inherit from host) |

**Sample values:**

| Setting | Sample value |
|---|---|
| hostname | `workstation` |
| username | `user` |
| machine-id | `cafebabe0011223344556677deadbeef` |
| cpuinfo | Built-in generic Intel Core i7-8550U on x86_64 |
| os-release presets | `ubuntu` → Ubuntu 24.04 LTS · `arch` → Arch Linux · `windows` → Windows 11 · `arduinoide` → ArduinoIDE · any other value used as a custom OS name |

**machine-id modes:**

| Value | Behaviour |
|---|---|
| `system` / `off` | No spoofing — real `/etc/machine-id` is used |
| `random` | Generates a fresh 32-char hex UUID on every launch |
| `sample` | Fixed placeholder `cafebabe0011223344556677deadbeef` |
| 32-char hex | Your own fixed machine-id |

The config is stored as a human-readable INI file at `~/.wryayer/<app>/config.ini`.

---

## Caveats

**AUR is Arch-only.** On Debian/Ubuntu the AUR code path is never reached; deps are resolved via `apt-cache` only. Fedora and other distros are not currently supported.

**glibc version pinning.** glibc is resolved and extracted as a normal dependency. The `ld-linux-x86-64.so.2` loader that executes inside the sandbox is therefore the one from the app's own extracted glibc, not the host's. If the app's packages were built against a glibc version that differs significantly from the host kernel's syscall ABI, it may crash with `version GLIBC_X.XX not found` or `Illegal instruction`.

**Microphone isolation is incomplete.** Setting `microphone=off` masks ALSA capture devices (`/dev/snd/pcmC*D*c`). Apps using PipeWire or PulseAudio can still access the microphone socket in `XDG_RUNTIME_DIR`. To fully block mic access, also set `audio=off`.

**No D-Bus session bus isolation.** The sandbox inherits the host `DBUS_SESSION_BUS_ADDRESS`. Apps that use D-Bus can still talk to other session services (Notifications, portal services, etc.).

**Hard links require same filesystem.** Deduplication only works when all `~/.wryayer/<app>/` directories are on the same filesystem. If you mount `~/.wryayer` on a separate partition from another app, `dedup` will silently skip cross-device hard-links.

**AUR builds run makepkg as your user.** This is the same trust model as using yay directly. Build scripts execute arbitrary code. Only install from AUR packages you trust.

**Wayland socket not isolated.** The Wayland display socket (`$XDG_RUNTIME_DIR/wayland-0`) is accessible inside the sandbox via `/run`. Apps have full Wayland access.

**No SETUID/SETCAP binaries.** bwrap drops most capabilities. Apps that rely on setuid helpers (e.g., some network tools) will fail unless you add the helper to your system installation.

---

## Known issues

- **soname resolution occasionally misses packages.** If `wryayer repair` can't find a `.so`, it usually means the owning package uses an unusual installation path. Workaround: install the package system-wide and copy the `.so` into `~/.wryayer/<app>/usr/lib/`.
- **AUR packages with custom build steps** (non-standard `PKGBUILD` layouts) may produce a `.pkg.tar.zst` that doesn't extract cleanly. Check the build log with `[t]` in the TUI.
- **GLib apps may miss GSettings schemas** if a dependency ships schemas that aren't compiled after extraction. wryayer runs `glib-compile-schemas` on the main app's schema dir, but not on dependency dirs. Workaround: run `glib-compile-schemas ~/.wryayer/<app>/usr/share/glib-2.0/schemas/` manually.
- **The TUI progress bar is indeterminate** during install and update operations because pacman doesn't emit structured progress on stderr. The actual log is one `[t]` keypress away.
- **Disk usage figures in `wryayer list`** are per-app apparent sizes; they don't subtract hard-linked savings. The footer line of `wryayer list` and the Space tab in the TUI both show the combined total with savings noted.

---

## Planned features

- [x] **Multi-binary apps** — install an app that ships more than one launcher binary (`--bin-names a,b,c`)
- [x] **Rollback support** — `wryayer snapshot` + `wryayer rollback` (hard-linked, instant)
- [x] **Install into existing app** — `wryayer install <pkg> --into <existing>` for plugins and bundles; alias gets its own first-class entry under `~/.wryayer/<pkg>/` with `alias_of` pointer
- [x] **TUI install target picker** — choosing Install on a search result now prompts whether to start a new app or merge into an existing one
- [ ] **Wayland isolation** — bind a private Wayland socket so apps can't impersonate each other
- [ ] **D-Bus portal forwarding** — route file-chooser and notification portals through `xdg-desktop-portal` without exposing the full session bus
- [ ] **Package signing verification** — validate `.pkg.tar.zst` signatures before extraction
- [ ] **Delta updates** — only re-download changed packages instead of the full dep tree
- [ ] **Export/import via SSH or SFTP** — `wryayer export --remote user@host:/path`
- [x] **TUI package search from AUR** — Install tab searches both official repos and the AUR
- [x] **Identity spoofing** — spoof hostname, username, machine-id, and cpuinfo per app
- [x] **Keyboard layout override** — inject `XKB_DEFAULT_LAYOUT` per app via `config keyboard-layout`; presets for QWERTY (us), QWERTZ (de), Colemak, and Dvorak
- [x] **Global default settings** — Settings tab in TUI and `~/.wryayer/defaults.ini` set defaults inherited by all new apps
- [ ] **Per-app env var overrides** — let users set `LANG`, `QT_SCALE_FACTOR`, etc. in `config.ini`
- [ ] **Dependency graph viewer** — TUI screen showing the full package tree for an installed app
- [ ] **Auto-snapshot on update** — capture a snapshot automatically before each update so failures can be undone with one keystroke

---

## Developing and testing

### Build

```fish
cargo build
```

### Run tests

Tests that touch the filesystem isolate themselves by temporarily redirecting `HOME` to a temp directory. Run with a single thread to avoid races on the `HOME` environment variable:

```fish
cargo test -- --test-threads=1
```

Or set a thread-safe count per test binary:

```fish
RUST_TEST_THREADS=1 cargo test
```

### Test coverage

The test suite targets **≥ 90 % branch coverage** on all pure and filesystem-dependent logic. Coverage is achieved through **equivalence class partitioning** — one representative value per class rather than exhaustive enumeration — combined with explicit boundary and error-path tests.

| Module / test file | What is covered |
|---|---|
| `config.rs` (`config_tests.rs`) | `parse_ini` (all keys, all enum variants, error paths, `ram_limit` disable aliases / integers / absent, `keyboard_layout` all values), `format_ini` (`[resources]` + `[keyboard]` sections presence/absence), `parse_bool` (3 EC), round-trip (including `ram_limit` and `keyboard_layout`) |
| `config.rs` (`global_config_tests.rs`) | `read_global_config` fallback when file absent, `write_global_config` + `read_global_config` round-trip, `keyboard_layout` all values through format+parse, `off`/`system`/empty disable aliases |
| `manifest.rs` | `write_manifest`/`read_manifest` round-trip, `list_all_apps` (empty, sorted, skips bad dirs), atomicity |
| `launcher.rs` | `create_launcher` (content, permissions), `remove_launcher` (missing, non-wryayer, valid) |
| `commands/dedup.rs` | `format_bytes` (4 EC + 7 boundaries), `du_walk` (SKIP_DIRS, hard-link accounting) |
| `package/deps.rs` | `strip_version_constraint` (7 operators), `is_soname_dep` (5 EC), `parse_pacman_field`, `parse_pacman_depends` (5 EC) |
| `commands/run.rs` | Arg stripping (5 cases), `no_other_instance` (missing file, bad content, live PID, dead PID), `has_systemd_run` (filesystem consistency), `wrap_with_ram_limit` (outer program, `--user`/`--scope`/`--quiet`, `MemoryMax`, `MemorySwapMax=0`, `--` separator, inner args preserved, env transfer) |
| `commands/install.rs` | `ensure_base_layout` (creates all symlinks, idempotent, preserves real dirs) |
| `commands/snapshot.rs` | `create` / `labels` / `latest` round-trip, inode sharing, `.snapshots` recursion guard, `rollback` (restores modifications, errors on missing label, preserves snapshots dir) |
| `commands/remove.rs` + alias model | `alias_of` serde round-trip, `skip_serializing_if` for `None`, legacy manifests without the field still parse, `list_all_apps` surfaces aliases as own entries, removing an alias leaves the target tree + manifest untouched, removing a target with dependent aliases is blocked with all blockers named, standalone removal unaffected |
| `tui/mod.rs` (`option_picker_tests.rs`) | `setting_options` (shape per row incl. keyboard layout row 13 and RAM limit row 14), `setting_title`, `setting_description`, `option_description`, `setting_current`, `apply_setting`, `cycle_setting` — full forward/backward/wrap cycles for all non-empty rows |
| `tui/mod.rs` | `parse_progress` (`PROGRESS n/total` parsing + garbage rejection), konami FSM (full sequence, wrong-key reset, case-insensitive BA) |

External-tool-dependent code (`bwrap_cmd`, `reinstall`, distro backends) is covered by integration tests that require a live environment with `bwrap` and either `pacman` (Arch) or `apt` / `dpkg` (Debian/Ubuntu) present.

---

## License

Copyright © 2026 KsmBl and contributors.

wryayer is free software: you can redistribute it and/or modify it under the terms of the **GNU Lesser General Public License** as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version.

This program is distributed in the hope that it will be useful, but **WITHOUT ANY WARRANTY**; without even the implied warranty of **MERCHANTABILITY** or **FITNESS FOR A PARTICULAR PURPOSE**. See the [GNU Lesser General Public License](https://www.gnu.org/licenses/lgpl-3.0.html) for more details.

A copy of the license is included in this repository as [`LICENSE`](LICENSE).

> **Note for package distributors:** The LGPL requires that users be able to relink a modified version of the library. Because wryayer is a standalone binary application (not a shared library), this condition is satisfied by making the source code available, which this repository does.
