# wryayer — programming guide

A practical, task-oriented guide for changing the code. It complements the two
other docs:

- **[README.md](README.md)** — what wryayer does, for users.
- **[README-CODE.md](README-CODE.md)** — architecture and internals (read this
  for the source-tree map, the sandbox bind list, and the spoofing internals).

This file answers *"I want to change X — where do I go and what do I touch?"*

---

## 1. Toolchain and build

- **Rust** ≥ 1.88 (edition 2021). `rustup` recommended.
- **A C compiler** (`cc`/`gcc`/`clang`) with a **static libc** available. `build.rs`
  compiles two small C helpers; if no compiler is found the build still succeeds
  and the affected features degrade gracefully (see §6).
- **Runtime tools** the sandbox shells out to: `bwrap` (bubblewrap, required),
  and optionally `xdg-dbus-proxy`, `systemd-run`, `avahi`.
- **GTK4** (≥ 4.10) only if you build the `gui` feature.

```sh
cargo build                      # default = tui feature
cargo build --features gui       # add the GTK front-end
cargo build --no-default-features# plain CLI, no TUI
cargo build --release            # optimized; what the installer ships
cargo clippy --all-features      # keep this clean — CI-style gate
cargo test --all-features        # run everything (see §7)
```

Feature flags (`Cargo.toml`):

| Feature | Default | Pulls in | Gates |
|---|---|---|---|
| `tui` | yes | ratatui, crossterm | `src/tui/`, the `tui` subcommand |
| `gui` | no | gtk4 | `src/gui/`, the `gui` subcommand |

Cross-feature code is guarded with `#[cfg(feature = "…")]`. **Always build all
three combinations** (`default`, `--features gui`, `--no-default-features`) before
committing — it's easy to break one.

---

## 2. The mental model

wryayer installs each app into `~/.wryayer/<app>/`: a self-contained filesystem
tree (the package plus its dependencies, extracted). At run time that tree is
bind-mounted as `/` inside a `bwrap` sandbox. Everything else — identity
spoofing, resource limits, device masking — is layered on by binding small
generated files over specific paths, or by injecting environment / `LD_PRELOAD`.

Two data structures anchor almost everything:

- **`AppConfig`** (`src/config.rs`) — the per-app (and global-default) settings,
  serialized to `~/.wryayer/<app>/config.ini` (and `~/.wryayer/defaults.ini`).
- **`Manifest`** (`src/manifest.rs`) — `.manifest.toml`: app name, package,
  launchers, `alias_of`, wine-game block. See README-CODE.md for the alias/merge
  model.

Add a knob → it almost always starts in `AppConfig` and ends in `run.rs`.

---

## 3. Recipe: add a new per-app config setting

This is the most common change. Say you want a boolean `foo`.

1. **`src/config.rs`**
   - add `pub foo: bool` to `AppConfig`, a default in `impl Default`, and (if it
     should propagate to merge aliases) a line in the alias-copy block.
   - parse it in `parse_ini` (`("foo", v) => …`) and emit it in `format_ini`.
   - add a round-trip assertion in `tests/config_tests.rs` (there's a test that
     constructs a fully-populated `AppConfig` — add your field there too, or the
     build breaks).

2. **`src/commands/run.rs`** — read `config.foo` in `bwrap_cmd()` and add the
   corresponding `--bind` / `--setenv` / etc. (see §5).

3. **CLI** (`src/main.rs` + `src/commands/config.rs`) — add a `ConfigSetting`
   variant and thread it through `commands::config::run(...)`. That function
   takes one `Option<&str>` per field; for settings that need validation or open
   a sub-flow, add a dedicated function instead (see `open_with`/`share_add` as
   models) and call it directly from the `main.rs` match arm.

4. **TUI** (`src/tui/`) — see §4. Rows are index-based; read that section
   carefully before inserting one.

5. **GUI** (`src/gui/config.rs`, behind `#[cfg(feature = "gui")]`) — add a widget
   in `build_form` and read it back in the returned `gather` closure.

6. Update **README.md** (Config reference table) and **README-CODE.md** if the
   mechanism is non-obvious.

---

## 4. The TUI (`src/tui/`)

- **`mod.rs`** holds `App` (all state), the event loop, and every key handler.
- **`ui.rs`** renders each screen with `ratatui`.

Key concepts:

- **`Screen` enum** — the current overlay/mode. Each variant has a numeric *tag*
  in `handle_key`; the tag routes to an `on_<screen>` handler and a
  `draw_<screen>` function. To add a screen: add the variant, a tag, a dispatch
  arm, a handler, and a draw arm.
- **Config rows are index-numbered** (`CFG_*` consts). ⚠️ The per-app Config
  screen and the global Settings tab **share indices** but render via different
  paths, and some indices are deliberately overloaded (e.g. a wine-game row and a
  behaviour row share an index because the two contexts never mix). `apply_setting`
  matches on **raw numeric literals**. If you insert a row, you must update the
  consts, `app_cfg_save_idx` / `app_cfg_total_rows`, and every `setting_*`
  function consistently — there is a cross-function consistency test in
  `tests/option_picker_tests.rs` that will catch drift. Prefer adding a row that
  opens a dedicated sub-screen (like *Shared dirs* / *Bound apps*) over widening
  the generic picker machinery.
- **Text input** uses a shared caret helper; navigation lists wrap around.

Headless render test pattern (used during development, not always committed):

```rust
let mut app = App::new().unwrap();
app.screen = Screen::Config { /* … */ };
let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
term.draw(|f| crate::tui::ui::draw(f, &mut app)).unwrap();
let text: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
assert!(text.contains("…"));
```

---

## 5. The sandbox launcher (`src/commands/run.rs`)

`bwrap_cmd()` assembles the `bwrap` command line and returns it plus optional
child handles (D-Bus proxy, Avahi stub, portal listener) whose lifetimes are
tied to the sandbox. `launch_bwrap()` spawns it, starts any background updater
threads (meminfo, `/proc/stat`), waits, then tears everything down.

Rules of thumb when adding a spoof or bind:

- **Order matters.** `--bind app_root /` comes first; later binds override
  earlier ones. `--proc /proc` must precede any `--ro-bind … /proc/<file>`.
- **Generated files** go under `~/.wryayer/<app>/.spoof/` and are removed on exit.
- **You cannot mkdir inside a read-only bind.** `/sys` is bound read-only, so to
  *add* entries under it (e.g. fake `cpuN` dirs) you must `--tmpfs` the parent and
  re-bind the real children back on top. See `spoof_sys_cpu`.
- **Some values change over time.** For those, write an initial file and start a
  background thread in `launch_bwrap` that rewrites it and is stopped/joined on
  exit (models: `meminfo_updater_loop`, `proc_stat_updater_loop`). Use the shared
  `stop` `AtomicBool`.
- Adding a return value to `bwrap_cmd` means updating **both** call sites
  (the normal path and the debug/`--print` path).

---

## 6. The C shims (`csrc/`, `build.rs`)

Two helpers are compiled by `build.rs` into `OUT_DIR` and embedded with
`include_bytes!`:

- **`cpuid_spoof.c`** → `libcpuidspoof.so`, injected via `LD_PRELOAD` to intercept
  `CPUID` and `sched_get/setaffinity`. See README-CODE.md → *CPU spoofing* for how
  CPUID faulting works.
- **`portal_client.c`** → `wryayer-portal`, a **static** helper for cross-container
  app binding.

Gotchas:

- `build.rs` is **best-effort**: if `cc` is missing or a static libc isn't
  available, it writes an empty blob and prints a `cargo:warning`. The runtime
  checks `if !BLOB.is_empty()` and disables the feature — never assume the blob
  is present.
- The LD_PRELOAD shim is built **without** `-fvisibility=hidden`, so its
  interposers (`sigaction`, `sched_getaffinity`, …) win symbol resolution.
- CPUID faulting is **Intel-only**. On AMD/unsupported CPUs the shim instead
  interposes libcpuid's public raw-data API (`cpuid_get_raw_data` /
  `cpuid_get_all_raw_data`) and rewrites the identity/topology leaves of the
  returned dump, so CPU-X still sees the spoofed vendor, brand, family/model and
  core count. Tools that don't use libcpuid keep the file/`/sys`/affinity layers.
- Avoid glibc symbols newer than your baseline (e.g. `strtoul` pulled a
  `GLIBC_2.38` dependency — it was replaced with a hand-rolled parser) so the shim
  loads against older glibc inside app trees.
- `build.rs` has `rerun-if-changed` on each C file; edit the `.c`, rebuild, and
  the embedded blob refreshes. To test a shim change against a real app:
  `cargo build && ./target/debug/wryayer run <app> …`.

---

## 7. Testing

```sh
cargo test --all-features                     # unit + integration
cargo test --all-features -- --test-threads=1 # serial: avoids races on shared
                                              # ~/.wryayer state (some tests touch it)
```

- **Unit tests** live inline (`#[cfg(test)] mod tests`) — e.g. `cpu.rs` has
  round-trip tests for presets and custom CPUs.
- **Integration tests** live in `tests/`. Notable: `config_tests.rs`
  (INI round-trip — keep the full-`AppConfig` literal in sync with new fields),
  `option_picker_tests.rs` (TUI row cross-function consistency).
- Tests must be **hermetic and offline** — no network, no reliance on host state
  you didn't set up. Prefer `tempfile` for filesystem fixtures.
- To sanity-check a **runtime/sandbox** change without a GUI, drive a small
  installed app (e.g. a shell) with a temporary config:
  `./target/debug/wryayer config <app> <setting> …` then
  `./target/debug/wryayer run <app> -- -c '…'`, and **restore the config after**.

---

## 8. Conventions

- Match the surrounding style: comment density, naming, and idiom of the file
  you're editing. Explain *why*, not *what*.
- Errors use `anyhow::Result` with `.context(...)`; user-facing failures print a
  clear message rather than panicking.
- Keep `cargo clippy --all-features` clean.
- Runtime side effects that write outside the repo belong under `~/.wryayer/`.
- Commit per logical change with a clear message. Some subsystems (the sandbox
  launcher) are touched by several features at once — group those into a single
  "wire X into the launcher" commit rather than splitting a hunk mid-function.

---

## 9. Where things live (quick index)

| I want to change… | Go to |
|---|---|
| a config field | `config.rs` → `run.rs` → `main.rs`/`commands/config.rs` → `tui/` → `gui/config.rs` |
| how an app is sandboxed | `commands/run.rs` (`bwrap_cmd`, `launch_bwrap`) |
| CPU / topology spoofing | `cpu.rs` (data) + `csrc/cpuid_spoof.c` (CPUID/affinity) + `run.rs` (`/proc`, `/sys`) |
| cross-container app binding | `csrc/portal_client.c` + `commands/portal.rs` + `run.rs` |
| a TUI screen or row | `tui/mod.rs` (state/keys) + `tui/ui.rs` (render) |
| a GUI form field or button | `gui/config.rs` / `gui/mod.rs` |
| install / extract / deps | `package/` + `commands/install.rs` |
| snapshots / dedup / export | the same-named files in `commands/` |

---

## 10. File-by-file reference

Every source file, what it's responsible for, and when you'd open it. See
README-CODE.md for the diagram version.

**Entry points & wiring**

| File | Responsibility |
|---|---|
| `main.rs` | The `clap` CLI: every subcommand and flag is defined here and dispatched to a `commands::*` function. Add a CLI command/flag here. |
| `lib.rs` | Module wiring; re-exports used by the integration tests in `tests/`. |
| `build.rs` | Compiles the two C helpers in `csrc/` and embeds them; best-effort (empty blob + warning if no compiler). |

**Core data & shared helpers**

| File | Responsibility |
|---|---|
| `config.rs` | `AppConfig` (every per-app / global setting), INI parse (`parse_ini`) and format (`format_ini`), `defaults.ini`, and the alias-merge copy. **Start here for a new setting.** |
| `manifest.rs` | `.manifest.toml` read/write, `app_dir`/`wryayer_root` path helpers, `list_all_apps`, `tree_order`, the alias/merge model. |
| `cpu.rs` | Built-in CPU profiles + the `CustomCpu` type; renders `/proc/cpuinfo`, and provides `cpuid_spoof_for` / `topology_for` for the launcher and shim. |
| `distro.rs` | Detects the host distro and selects the package backend (pacman/apt/dnf). |
| `launcher.rs` | Creates/removes the `~/bin/<app>` shell wrapper. |
| `avahi_stub.rs` | Config/data for the in-sandbox Avahi stub bus. |

**`commands/` — one file per subcommand**

| File | Responsibility |
|---|---|
| `run.rs` | **The sandbox launcher.** `bwrap_cmd` assembles the bwrap command line (all binds, spoofs, env, portal, CPU); `launch_bwrap` spawns it, runs updater threads, waits, tears down. The biggest and most important runtime file. |
| `install.rs` | resolve → download → extract → write manifest → dedup. |
| `install_game.rs` | Wine-container import (game folder → `.exe` → prefix). |
| `update.rs` | Re-resolve + re-extract; version checks (`--check`). |
| `remove.rs` | Delete tree + launcher; alias-aware (`--cascade`). |
| `snapshot.rs` | Hard-linked snapshots, list, rollback, delete, prune. |
| `export.rs` / `import.rs` | Zip an app tree / recreate one from a zip. |
| `dedup.rs` | Cross-app hard-link identical files; disk-usage accounting (`format_bytes`, `all_du`). |
| `repair.rs` | Resolve + install packages for missing sonames. |
| `list.rs` | The `wryayer list` table + size totals. |
| `clean.rs` | Wipe the shared download/build cache. |
| `config.rs` | The `wryayer config` CLI surface (reads/writes `AppConfig`). |
| `portal.rs` | Host-side listener for cross-container app binding (`wryayer portal-listener`). |
| `mod.rs` | `pub mod` wiring for the above. |

**`package/` — resolving and unpacking packages**

| File | Responsibility |
|---|---|
| `deps.rs` | BFS dependency resolver; virtual-package and soname fallback; pacman-output parsers. |
| `download.rs` | Official repo download + AUR git clone/`makepkg` build. |
| `extract.rs` | Unpack `.pkg.tar.zst` / `.deb` / `.rpm` into an app tree. |
| `soname_check.rs` | Scan ELF `NEEDED` entries; find owning packages for missing libs. |
| `mod.rs` | Module wiring. |

**`tui/` — terminal UI (feature `tui`)**

| File | Responsibility |
|---|---|
| `mod.rs` | `App` state, the event loop, the `Screen` enum, and **all** key handling. Config-row indices (`CFG_*`) and the `setting_*` helpers live here — read §4 before editing. |
| `ui.rs` | `ratatui` rendering for every screen/overlay. |
| `konami.rs` | Easter-egg state machine. |

**`gui/` — GTK4 desktop UI (feature `gui`)**

| File | Responsibility |
|---|---|
| `mod.rs` | Window, the six tabs, the installed/games list + toolbar buttons, snapshots dialog, space tab, small dialogs (`confirm`, `text_prompt`). |
| `config.rs` | Per-app + global settings forms and the custom-CPU configurator dialog. Mirrors the TUI's config surface. |
| `install.rs` | Search + multi-select install flow, already-installed dialog, game wizard. |
| `op.rs` | Runs a wryayer subcommand in a console dialog (`run_operation`, `run_jobs`). |

**`csrc/` — C helpers (compiled by `build.rs`)**

| File | Responsibility |
|---|---|
| `cpuid_spoof.c` | `LD_PRELOAD` shim: CPUID faulting + emulation, and `sched_get/setaffinity` interposition. See §6 and README-CODE.md. |
| `portal_client.c` | Static helper symlinked into sandboxes as each bound app / opener; forwards launch requests to the host portal. |

**`tests/`** — integration tests, one file per area (`config_tests.rs`,
`option_picker_tests.rs`, `snapshot_tests.rs`, …). See §7.
