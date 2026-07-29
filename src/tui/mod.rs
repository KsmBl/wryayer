pub mod konami;
mod ui;

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::widgets::ListState;
use ratatui::Terminal;

use crate::commands::dedup::all_du;
use crate::config::{read_config, read_global_config, write_config, write_global_config, AppConfig, AvahiMode, Layout, LocalDelete, PasswordSource, TempMode, Theme};
use crate::manifest::{list_all_apps, tree_order, Manifest};

// ── Op messages ───────────────────────────────────────────────────────────────

pub enum Msg {
    Line(String),
    Done(bool),
}

// ── File browser entry ────────────────────────────────────────────────────────

pub struct FbEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_zip: bool,
}

// ── Screen overlays ───────────────────────────────────────────────────────────

pub enum Screen {
    Main,
    Confirm {
        title: String,
        body: Vec<String>,
        action: PendingAction,
        danger: bool,
    },
    Operation {
        title: String,
        log: Vec<String>,
        done: bool,
        success: bool,
        rx: Receiver<Msg>,
        total_bytes: Option<u64>,
        /// (done, total) emitted by the subprocess via `PROGRESS n/total` lines.
        progress: Option<(u64, u64)>,
        started: Instant,
        reload: bool,
        show_log: bool,
        /// Set when the subprocess emits `PROMPT_LAUNCHER_CHOICE:<pkg>:<bins>`.
        /// Triggers the NoLauncherChoice popup once the operation finishes.
        launcher_choice: Option<(String, Vec<String>)>,
        /// The --into target from the original install, if this was a merge install.
        into_target: Option<String>,
        /// Set when the subprocess emits `PROMPT_OUTDATED_PACKAGES:<pkg>`.
        /// Triggers the OutdatedPackages popup once the operation finishes.
        outdated_pkg: Option<String>,
        /// The args originally passed to launch_op — stored so the OutdatedPackages
        /// popup can relaunch the same operation with --sync-db appended.
        original_args: Vec<String>,
    },
    Config {
        app_name: String,
        config: AppConfig,
        selected: usize,
    },
    SharedDirs {
        app_name: String,
        dirs: Vec<String>,
        selected: usize,
    },
    /// Multi-select list of other installed apps to bind into this app's
    /// sandbox as host-delegated launchers (config.bound_apps). Each entry is
    /// (app name, ticked). Space toggles; Enter/Esc saves and returns to Config.
    BoundApps {
        app_name: String,
        apps: Vec<(String, bool)>,
        selected: usize,
    },
    FileBrowser {
        current_dir: PathBuf,
        entries: Vec<FbEntry>,
        fb_state: ListState,
        mode: BrowserMode,
    },
    /// Wine-game wizard: pick the main .exe inside the selected folder.
    GameExePick {
        game_dir: PathBuf,
        exes: Vec<(String, u64)>,
        selected: usize,
    },
    /// Wine-game wizard: type an app name (defaults to sanitized folder name).
    /// The name doubles as the container directory under ~/.wryayer/.
    GameNameInput {
        game_dir: PathBuf,
        exe: String,
        value: String,
    },
    /// Wine-game wizard: confirm install + ask whether to delete the source folder.
    GameConfirm {
        game_dir: PathBuf,
        exe: String,
        app_name: String,
        delete_source: bool,
        selected: usize, // 0 = install, 1 = toggle delete, 2 = cancel
    },
    /// Choose between a fresh install and merging into an existing app.
    /// Row 0 = fresh install; rows 1..=targets.len() = merge into targets[row-1].
    InstallTarget {
        pkg: String,
        targets: Vec<String>,
        selected: usize,
    },
    /// Popup launched from the Config screen with all valid choices for a
    /// single setting. `setting_idx` is the row index in the Config screen
    /// (0..=5); `selected` is the highlighted option inside the popup.
    OptionPicker {
        app_name: String,
        config: AppConfig,
        setting_idx: usize,
        selected: usize,
    },
    /// Help popup shown when the user presses `?` on a Config row.
    /// Dismisses on any key and returns to Config with `back_selected` active.
    SettingHelp {
        app_name: String,
        config: AppConfig,
        back_selected: usize,
    },
    /// Per-option help popup opened from OptionPicker when the user presses `?`
    /// while hovering over a specific choice.  Dismisses back to OptionPicker.
    OptionHelp {
        app_name: String,
        config: AppConfig,
        setting_idx: usize,
        picker_selected: usize,
    },
    /// Free-text input overlay for spoof string settings and wine_game fields.
    TextInput {
        app_name: String,
        config: AppConfig,
        back_selected: usize,
        field_idx: usize,
        value: String,
    },
    /// Full key-bindings reference popup. Dismisses on any key.
    KeyHelp,
    /// Inline text input for renaming an app's display name.
    RenameApp {
        app_name: String,
        value: String,
    },
    /// Inline text input for picking an app name when installing a duplicate package.
    DuplicateInstall {
        pkg: String,
        value: String,
        /// None = fresh container; Some(target) = merge into target app.
        into: Option<String>,
    },
    /// Choice between uninstalling an existing app or installing a second copy.
    AlreadyInstalled {
        pkg: String,
        selected: usize, // 0 = install second copy, 1 = uninstall
    },
    /// Shown after an install fails because no binary was found in the package.
    /// User picks "keep without launcher" or "clean up (already done)".
    NoLauncherChoice {
        pkg: String,
        available_bins: Vec<String>,
        selected: usize, // 0 = keep, 1 = clean (already done)
        /// The --into target if this was a merge install, so the retry can pass it.
        into_target: Option<String>,
    },
    /// Shown when a download returns 404 (package databases are out of date).
    /// User picks "Update & retry" or "Cancel".
    OutdatedPackages {
        pkg: String,
        /// The original install args; retry appends --sync-db to these.
        install_args: Vec<String>,
        selected: usize, // 0 = update & retry, 1 = cancel
    },
    /// Ask whether to create a ~/bin/<pkg> shortcut before starting the install.
    AskShortcut {
        pkg: String,
        title: String,
        /// Install args ready to pass to spawn_wryayer (without --keep-without-launcher).
        args: Vec<String>,
        selected: usize, // 0 = yes (create shortcut), 1 = no (skip)
    },
    /// Ask whether to install into a VeraCrypt container. Shown right after the
    /// shortcut prompt, so both install-time choices are made up front.
    AskEncrypt {
        pkg: String,
        title: String,
        /// Install args accumulated so far, including the shortcut answer.
        args: Vec<String>,
        selected: usize, // indexes kind.choices()
        /// Whether this is an install being routed into a container, or an
        /// already-installed app being converted. The two offer different
        /// choices and pass different flags.
        kind: EncryptAsk,
    },
    /// Reveal the container passwords held in the master store.
    ///
    /// `entries` is None until the store has been opened — while locked the
    /// screen is a masked master-password prompt instead.
    RevealPasswords {
        entries: Option<Vec<(String, String)>>,
        value: String,
        selected: usize,
        error: Option<String>,
    },
    /// Create or change the master password, from the Settings tab.
    MasterPassword {
        stages: Vec<MasterStage>,
        idx: usize,
        value: String,
        /// The existing password, once verified — needed to re-key the store.
        current: String,
        /// First entry of the new password, awaiting confirmation.
        new_first: String,
        error: Option<String>,
    },
    /// Collect the passwords an encrypted install needs, one masked field at a
    /// time. Each is validated as it is entered, so the install never starts
    /// with a secret that will turn out to be wrong.
    EncryptSecrets {
        title: String,
        args: Vec<String>,
        /// When false the collected secrets are not handed to the child at all
        /// — the point was the side effect of caching sudo credentials, so a
        /// container operation inside the child never has to prompt on a
        /// terminal the TUI is busy repainting.
        pass_to_child: bool,
        /// Remaining fields to ask for, in order.
        stages: Vec<SecretStage>,
        idx: usize,
        /// The field being typed.
        value: String,
        /// First entry of a type-it-twice pair, awaiting confirmation.
        first_entry: String,
        sudo: String,
        master: String,
        container: String,
        /// Validation failure to show above the input.
        error: Option<String>,
    },
    /// Snapshot manager: pick a snapshot to roll back to or delete.
    SnapshotManager {
        app_name: String,
        snaps: Vec<String>,
        selected: usize,
    },
    /// Field-by-field configurator for a user-defined ("custom") CPU. Saving
    /// stores the result as `custom:<...>` in `spoof_cpuinfo`.
    CpuConfig {
        /// Empty for global defaults, otherwise the per-app config being edited.
        app_name: String,
        config: AppConfig,
        draft: Box<CpuDraft>,
        selected: usize,
        /// When Some, the selected field is being edited: (buffer, caret char index).
        editing: Option<(String, usize)>,
        /// When true, a help popup for the selected row is shown over the form.
        help: bool,
    },
}

/// Editable string form of a [`crate::cpu::CustomCpu`]. Every field is held as
/// text so the configurator can edit it in place; numbers are validated on save.
#[derive(Clone)]
pub struct CpuDraft {
    pub vendor: String,     // "GenuineIntel" or "AuthenticAMD" (cycled, not typed)
    pub model_name: String,
    pub family: String,
    pub model: String,
    pub stepping: String,
    pub cores: String,
    pub threads: String,
    pub mhz: String,
    pub cache_kb: String,
    pub host: String,
}

/// Field rows in the CPU configurator, in display order. The Save button is an
/// extra row after these (index `CPU_FIELDS.len()`).
pub const CPU_FIELDS: &[&str] = &[
    "Vendor", "Model name", "CPU family", "Model", "Stepping",
    "Cores", "Threads", "CPU MHz", "Cache (KB)", "Host",
];

/// Row index of the free-text Host field (mainboard string).
pub const CPU_HOST_ROW: usize = 9;

/// Row index of the Save button in the configurator.
pub const CPU_SAVE_ROW: usize = CPU_FIELDS.len();

/// A short one-line hint shown under the selected configurator row.
pub fn cpu_field_hint(row: usize) -> &'static str {
    match row {
        0 => "CPU vendor — ←/→ or Space to switch GenuineIntel / AuthenticAMD.",
        1 => "Human-readable brand string, e.g. 'AMD Ryzen 9 7950X 16-Core Processor'.",
        2 => "CPU family number (Intel Core=6, AMD Zen3/4=25). Names the microarchitecture.",
        3 => "Model number — pins the exact chip within the family.",
        4 => "Stepping — silicon revision (0–15). 1 is a safe default.",
        5 => "Number of physical cores to report.",
        6 => "Total logical CPUs (threads). =Cores for no SMT, 2×Cores for SMT.",
        7 => "Reported clock speed in MHz (3200 = 3.2 GHz).",
        8 => "Cache size in KB (16384 = 16 MB).",
        9 => "Mainboard shown as fastfetch 'Host:'. Blank = auto-pick a board.",
        _ => "Save this CPU and apply it to the sandbox.",
    }
}

/// Full `?`-help text for a configurator row.
pub fn cpu_field_help(row: usize) -> &'static str {
    match row {
        0 => "Vendor\n\nThe CPU vendor ID string, reported as /proc/cpuinfo 'vendor_id' and CPUID leaf 0.\n\nUse ←/→ or Space to switch between 'GenuineIntel' (Intel) and 'AuthenticAMD' (AMD). The vendor also selects the matching CPU feature-flag set.",
        1 => "Model name\n\nThe human-readable brand string shown by lscpu, CPU-X, and /proc/cpuinfo 'model name'.\n\nExamples:\n• 13th Gen Intel(R) Core(TM) i7-13700K\n• AMD Ryzen 9 7950X 16-Core Processor",
        2 => "CPU family\n\nA number identifying the processor generation / microarchitecture (CPUID family). Together with Model it names a specific chip design, and it is encoded into CPUID leaf 1 so detection libraries (libcpuid / CPU-X) see the same family as /proc/cpuinfo.\n\nCommon values:\n• Intel Core (all modern) = 6\n• AMD Zen / Zen+ = 23\n• AMD Zen 2 = 23\n• AMD Zen 3 / Zen 4 = 25",
        3 => "Model\n\nA number that, together with CPU family, identifies the exact CPU within that family (CPUID model). Encoded into CPUID leaf 1.\n\nExample: family 6 + model 151 = Intel Alder Lake; family 25 + model 97 = AMD Zen 4 (Ryzen 7000).",
        4 => "Stepping\n\nThe silicon revision of the chip (CPUID stepping, 0–15). Minor hardware revisions bump this value.\n\nMostly cosmetic for spoofing — leave it at 1 unless you are matching a specific chip.",
        5 => "Cores\n\nThe number of physical CPU cores to report, shown as 'cpu cores' in /proc/cpuinfo. Must be at least 1.",
        6 => "Threads\n\nThe total number of logical CPUs (hardware threads). One processor block per thread is written to /proc/cpuinfo.\n\nSet equal to Cores for a chip with no SMT/Hyper-Threading, or 2× Cores for one with SMT. Values below Cores are clamped up to Cores.",
        7 => "CPU MHz\n\nThe reported clock speed in megahertz, shown as 'cpu MHz' in /proc/cpuinfo (e.g. 3200 = 3.2 GHz). It also drives the synthetic 'bogomips' value.",
        8 => "Cache (KB)\n\nThe CPU cache size in kilobytes, shown as 'cache size' in /proc/cpuinfo (e.g. 16384 = 16 MB).",
        9 => "Host (mainboard)\n\nThe motherboard / system identity presented over DMI (SMBIOS), i.e. what fastfetch shows as 'Host:' and hostnamectl as the hardware model. Type the board you want to appear, e.g.:\n• ASUS ROG STRIX X670E-E GAMING\n• Supermicro H12SSL-i\n\nThe OEM vendor is inferred from the text when recognised. Leave blank to auto-pick a believable board that matches the CPU (a server board for EPYC/Xeon, an enthusiast desktop board otherwise).",
        _ => "Save\n\nStore this CPU as a custom profile (custom:…) and apply it. It overrides /proc/cpuinfo and the CPUID instruction inside the sandbox, so both file-parsing tools and detection libraries (CPU-X / libcpuid) see the fake CPU.",
    }
}

impl CpuDraft {
    /// Seed the configurator from the current config value: an existing
    /// `custom:<...>`, a built-in `preset:<key>`, or a generic starter.
    pub fn from_config(cfg: &AppConfig) -> Self {
        let base = cfg.spoof_cpuinfo.as_deref()
            .and_then(crate::cpu::CustomCpu::parse)
            .or_else(|| cfg.spoof_cpuinfo.as_deref().and_then(crate::cpu::CustomCpu::from_preset))
            .unwrap_or_else(crate::cpu::CustomCpu::starter);
        CpuDraft {
            vendor: base.vendor_id,
            model_name: base.model_name,
            family: base.family.to_string(),
            model: base.model.to_string(),
            stepping: base.stepping.to_string(),
            cores: base.cores.to_string(),
            threads: base.threads.to_string(),
            mhz: base.mhz.to_string(),
            cache_kb: base.cache_kb.to_string(),
            host: base.host,
        }
    }

    /// Current text value of field `idx` (0-based, matching `CPU_FIELDS`).
    pub fn field(&self, idx: usize) -> &str {
        match idx {
            0 => &self.vendor,
            1 => &self.model_name,
            2 => &self.family,
            3 => &self.model,
            4 => &self.stepping,
            5 => &self.cores,
            6 => &self.threads,
            7 => &self.mhz,
            8 => &self.cache_kb,
            9 => &self.host,
            _ => "",
        }
    }

    fn set_field(&mut self, idx: usize, v: String) {
        match idx {
            // Row 0 (vendor) is a switch, not a text field — never set here.
            1 => self.model_name = v,
            2 => self.family = v,
            3 => self.model = v,
            4 => self.stepping = v,
            5 => self.cores = v,
            6 => self.threads = v,
            7 => self.mhz = v,
            8 => self.cache_kb = v,
            9 => self.host = v,
            _ => {}
        }
    }

    /// Materialize into a `custom:<...>` config value, coercing invalid or empty
    /// numeric fields to safe defaults.
    pub fn to_spec(&self) -> String {
        let num = |s: &str, d: u32| s.trim().parse::<u32>().unwrap_or(d);
        let cores = num(&self.cores, 1).max(1);
        let threads = num(&self.threads, cores).max(cores);
        let name = if self.model_name.trim().is_empty() { "Custom CPU".to_string() } else { self.model_name.clone() };
        crate::cpu::CustomCpu {
            vendor_id: self.vendor.clone(),
            family: num(&self.family, 6),
            model: num(&self.model, 0),
            stepping: num(&self.stepping, 1),
            cores,
            threads,
            mhz: num(&self.mhz, 3000),
            cache_kb: num(&self.cache_kb, 8192),
            model_name: name,
            host: self.host.trim().to_string(),
        }.serialize()
    }
}


pub enum PendingAction {
    Remove(String),
    ConfirmedRemove(String),
    /// Remove a target app together with all aliases that point at it.
    RemoveCascade(String, Vec<String>),
    ConfirmedRemoveCascade(String),
    Update(String),
    UpdateAll,
    Install { pkg: String, app_name: Option<String>, into: Option<String> },
    Export(String),
    Snapshot(String),
    Rollback(String, String),
    DeleteSnapshot(String, String),
    /// Move an app back out of its VeraCrypt container.
    ConfirmedDecrypt(String),
}

// ── Tabs ──────────────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy)]
pub enum Tab {
    Installed,
    Install,
    Import,
    Games,
    Space,
    Settings,
}

/// Where the file browser hands its picked path. Lets one browser handle
/// .zip imports, shared-dir picking, and wine-game folder picking.
#[derive(Clone)]
pub enum BrowserMode {
    /// Pick a .zip file for `wryayer import`.
    ImportZip,
    /// Pick a directory and add it to `app_name`'s shared_dirs.
    PickShareDir(String),
    /// Pick a directory to import as a wine game.
    PickGameDir,
}

// ── App state ─────────────────────────────────────────────────────────────────

/// A package-search hit: (package_name, optional_repo).
type SearchHit = (String, Option<String>);
/// A generation-tagged batch of search hits sent from the search thread, so a
/// stale batch from an earlier query can be discarded.
type SearchBatch = (u64, Vec<SearchHit>);

pub struct App {
    pub quit: bool,
    pub tab: Tab,
    // Installed tab
    pub installed: Vec<Manifest>,
    pub inst_state: ListState,
    pub update_available: HashMap<String, String>,
    /// Async update check, streamed in from a background thread on start and
    /// after reloads so the list dots appear without blocking the UI.
    pub update_tx: Sender<HashMap<String, String>>,
    pub update_rx: Receiver<HashMap<String, String>>,
    // Install tab — async search
    pub search_input: String,
    /// (package_name, optional_repo) pairs returned by pkg_search.
    pub search_results: Vec<SearchHit>,
    pub search_searching: bool,
    pub search_gen: u64,
    pub search_tx: Sender<SearchBatch>,
    pub search_rx: Receiver<SearchBatch>,
    pub avail_state: ListState,
    pub search_list_focused: bool,
    // Import tab
    pub import_input: String,
    // Settings tab (global defaults)
    pub global_config: AppConfig,
    pub global_selected: usize,
    // Disk usage (computed once on load, refreshed on reload)
    pub app_sizes: HashMap<String, u64>,
    pub du_apparent: u64,
    pub du_actual: u64,
    // Running-instance counts per app, keyed by filesystem-root name.
    // Refreshed on a throttle from the event loop (scanning /proc is not free).
    pub running_instances: HashMap<String, usize>,
    pub last_instance_scan: Instant,
    // Overlay
    pub screen: Screen,
    pub status: String,
    pub log_scroll: usize,
    /// Whether the log view sticks to the newest lines. True until the user
    /// scrolls up; a running operation can emit hundreds of lines a second, so
    /// a fixed scroll offset would leave the view stranded on stale output.
    pub log_follow: bool,
    pub needs_clear: bool,
    /// Caret position (in characters) for whichever text-input overlay is
    /// active. Only one input is on screen at a time, so a single shared cursor
    /// is enough. Set to the value's length when an input opens.
    pub input_cursor: usize,
    /// If Some, the event loop will suspend the TUI, exec `wryayer run <app>`
    /// with inherited stdio so the user actually interacts with the app,
    /// then resume. Set by pressing `r`/Enter on an installed app.
    /// Apps stored in a VeraCrypt container, mapped to how that container
    /// currently stands. Refreshed on the same throttle as running instances —
    /// see `refresh_running_instances`.
    pub encrypted_apps: HashMap<String, EncState>,
    pub run_request: Option<String>,
    /// If Some, the event loop will suspend the TUI, open an editor on the
    /// given path, save config to "custom" after, then resume.
    /// Tuple: (app_name, path_to_edit)
    pub editor_request: Option<(String, PathBuf)>,
    // ── Easter egg ────────────────────────────────────────────────────────────
    pub konami_mode: bool,
    pub konami_state: usize,
    // ── Detail panel ─────────────────────────────────────────────────────────
    pub detail_focused: bool,
    pub detail_scroll: usize,
    // ── Install multi-select ──────────────────────────────────────────────────
    pub selected_pkgs: HashSet<String>,
    pub install_queue: VecDeque<String>,
    // Games tab selection
    pub games_state: ListState,
    /// Stores in-progress wine-game (exe, prefix) edits while the Config screen
    /// is open for a wine game. Set when [s] opens Config; cleared when Config
    /// is exited (Save commits it to the manifest; Esc discards).
    /// Lives on App so it survives transitions to OptionPicker / SharedDirs /
    /// SettingHelp / TextInput without having to be threaded through every
    /// Screen variant.
    pub editing_wine_game: Option<(String, String)>,
}

impl App {
    fn new() -> Result<Self> {
        let installed = tree_order(list_all_apps()?);
        let mut inst_state = ListState::default();
        if !installed.is_empty() {
            inst_state.select(Some(0));
        }
        let (app_sizes, du_apparent, du_actual) = all_du().unwrap_or_default();
        let (search_tx, search_rx) = mpsc::channel();
        let (update_tx, update_rx) = mpsc::channel();
        spawn_update_check(update_tx.clone());
        Ok(Self {
            quit: false,
            tab: Tab::Installed,
            installed,
            inst_state,
            update_available: HashMap::new(),
            update_tx,
            update_rx,
            search_input: String::new(),
            search_results: Vec::new(),
            search_searching: false,
            search_gen: 0,
            search_tx,
            search_rx,
            avail_state: ListState::default(),
            search_list_focused: false,
            import_input: String::new(),
            global_config: read_global_config(),
            global_selected: 0,
            app_sizes,
            du_apparent,
            du_actual,
            running_instances: scan_running_instances(),
            last_instance_scan: Instant::now(),
            screen: Screen::Main,
            status: String::new(),
            log_scroll: 0,
            log_follow: true,
            needs_clear: false,
            input_cursor: 0,
            encrypted_apps: HashMap::new(),
            run_request: None,
            editor_request: None,
            konami_mode: false,
            konami_state: 0,
            detail_focused: false,
            detail_scroll: 0,
            selected_pkgs: HashSet::new(),
            install_queue: VecDeque::new(),
            games_state: ListState::default(),
            editing_wine_game: None,
        })
    }

    fn reload_installed(&mut self) {
        if let Ok(list) = list_all_apps() {
            self.installed = tree_order(list);
            let sel = self.inst_state.selected().unwrap_or(0);
            if self.installed.is_empty() {
                self.inst_state.select(None);
            } else {
                self.inst_state.select(Some(sel.min(self.installed.len() - 1)));
            }
            // Clamp games_state to the current games list.
            let games_count = self.installed.iter().filter(|m| m.app.wine_game.is_some()).count();
            if games_count == 0 {
                self.games_state.select(None);
            } else {
                let gs = self.games_state.selected().unwrap_or(0);
                self.games_state.select(Some(gs.min(games_count - 1)));
            }
        }
        if let Ok((sizes, apparent, actual)) = all_du() {
            self.app_sizes = sizes;
            self.du_apparent = apparent;
            self.du_actual = actual;
        }
        self.running_instances = scan_running_instances();
        self.last_instance_scan = Instant::now();
        // Package versions may have changed (install/update/remove); re-check so
        // stale update dots clear and new ones appear.
        self.update_available.clear();
        spawn_update_check(self.update_tx.clone());
    }

    /// Re-scan /proc for running sandboxes at most once per second so the
    /// instance counts in the list stay live without hammering /proc on every
    /// 50 ms redraw.
    fn refresh_running_instances(&mut self) {
        if self.last_instance_scan.elapsed() >= Duration::from_secs(1) {
            self.running_instances = scan_running_instances();
            // Piggy-backed on the same throttle: listing mounted volumes forks
            // `veracrypt --list`, which must never run per-frame from the
            // renderer. Cached here and read by draw_installed.
            self.encrypted_apps =
                crate::commands::encrypt::scan(self.installed.iter().map(|m| m.app.name.as_str()));
            self.last_instance_scan = Instant::now();
        }
    }

    /// Which encryption rows `app_name`'s config screen should offer.
    ///
    /// Reads `alias_of` from the already-loaded manifest list rather than off
    /// disk: this runs on every frame the config popup is open.
    pub fn encryption_rows_for(&self, app_name: &str) -> EncryptionRows {
        let is_alias = self
            .installed
            .iter()
            .find(|m| m.app.name == app_name)
            .is_some_and(|m| m.app.alias_of.is_some());
        EncryptionRows::for_app(app_name, is_alias)
    }

    pub fn selected_installed(&self) -> Option<&Manifest> {
        self.inst_state.selected().and_then(|i| self.installed.get(i))
    }

    /// Filtered list of wine-game manifests, in `installed` order.
    pub fn games(&self) -> Vec<&Manifest> {
        self.installed.iter().filter(|m| m.app.wine_game.is_some()).collect()
    }

    pub fn selected_game(&self) -> Option<&Manifest> {
        let games = self.games();
        self.games_state.selected().and_then(|i| games.get(i).copied())
    }

    pub fn selected_available(&self) -> Option<&str> {
        self.avail_state
            .selected()
            .and_then(|i| self.search_results.get(i))
            .map(|(name, _)| name.as_str())
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Count running sandboxes per app by scanning /proc for the bwrap monitor
/// process of each launch.  Every wryayer sandbox runs `bwrap --bind <app_root>
/// / …`, and only the outer monitor keeps that argv (the inner process is
/// exec'd into the app), so one match == one running instance.
///
/// The result is keyed by `app.name`.  Programs installed with `--into <parent>`
/// share the parent's sandbox root, so the `--bind <root> /` triple only names
/// the parent; counting by root alone would attribute a running child to its
/// parent and show the same total on both rows.  We instead disambiguate by the
/// binary the sandbox is actually running (`bwrap … -- <binary> …`), which is
/// each program's own `main_binary`, and fall back to the root's own app when a
/// launch used a binary that matches no manifest (e.g. a `run --bin` override).
fn scan_running_instances() -> HashMap<String, usize> {
    let mut map: HashMap<String, usize> = HashMap::new();
    let Ok(root) = crate::manifest::wryayer_root() else { return map };
    let root_prefix = format!("{}/", root.to_string_lossy());

    // (fs_root dir name, main-binary basename) -> app.name, plus the non-alias
    // owner of each root as the fallback attribution.
    let manifests = crate::manifest::list_all_apps().unwrap_or_default();
    let mut by_bin: HashMap<(String, String), String> = HashMap::new();
    let mut root_owner: HashMap<String, String> = HashMap::new();
    for m in &manifests {
        let fs_root = m.app.alias_of.clone().unwrap_or_else(|| m.app.name.clone());
        let bin = std::path::Path::new(&m.app.main_binary)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(m.app.main_binary.as_str())
            .to_string();
        by_bin.insert((fs_root.clone(), bin), m.app.name.clone());
        if m.app.alias_of.is_none() {
            root_owner.insert(fs_root, m.app.name.clone());
        }
    }

    let Ok(entries) = std::fs::read_dir("/proc") else { return map };
    for entry in entries.flatten() {
        let fname = entry.file_name();
        // Only numeric PID directories.
        if !fname.to_str().map(|s| s.bytes().all(|b| b.is_ascii_digit())).unwrap_or(false) {
            continue;
        }
        // Count only the actual bwrap monitor.  When ram_limit is set the launch
        // is `systemd-run … bwrap --bind <root> / …`, so systemd-run's cmdline
        // carries the same triple; filtering by the real executable (bwrap is
        // exec'd through a symlink, but /proc/<pid>/exe still resolves to it)
        // avoids double-counting that wrapper.
        let is_bwrap = std::fs::read_link(entry.path().join("exe"))
            .ok()
            .as_deref()
            .and_then(std::path::Path::file_name)
            .map(|n| n == "bwrap")
            .unwrap_or(false);
        if !is_bwrap {
            continue;
        }
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else { continue };
        let args: Vec<&str> = raw
            .split(|&b| b == 0)
            .filter_map(|s| std::str::from_utf8(s).ok())
            .filter(|s| !s.is_empty())
            .collect();

        // The binary bwrap execs sits right after the `--` command separator.
        let run_bin = args
            .iter()
            .position(|&a| a == "--")
            .and_then(|i| args.get(i + 1))
            .and_then(|p| std::path::Path::new(p).file_name())
            .and_then(|n| n.to_str());

        // Look for the `--bind <app_root> /` triple that mounts the sandbox root.
        for w in args.windows(3) {
            if w[0] == "--bind" && w[2] == "/" && w[1].starts_with(&root_prefix) {
                if let Some(rest) = w[1].strip_prefix(&root_prefix) {
                    let fs_root = rest.split('/').next().unwrap_or("");
                    if !fs_root.is_empty() {
                        // Attribute to the program whose main_binary is running in
                        // this shared root; fall back to the root's own app.
                        let key = run_bin
                            .and_then(|b| by_bin.get(&(fs_root.to_string(), b.to_string())).cloned())
                            .or_else(|| root_owner.get(fs_root).cloned())
                            .unwrap_or_else(|| fs_root.to_string());
                        *map.entry(key).or_insert(0) += 1;
                    }
                }
                break;
            }
        }
    }
    map
}

/// Kick off an update check on a background thread; the result map is sent back
/// over `tx` and drained into `App::update_available` by the event loop.
fn spawn_update_check(tx: Sender<HashMap<String, String>>) {
    thread::spawn(move || {
        let _ = tx.send(crate::commands::update::check_all_updates());
    });
}

pub fn run() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let res = event_loop(&mut terminal);
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}

fn event_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let mut app = App::new()?;

    loop {
        // Drain op channel
        if let Screen::Operation { rx, log, done, success, progress, launcher_choice, outdated_pkg, .. } = &mut app.screen {
            loop {
                match rx.try_recv() {
                    Ok(Msg::Line(l)) => {
                        if let Some(rest) = l.strip_prefix("PROMPT_LAUNCHER_CHOICE:") {
                            if let Some((pkg, bins_str)) = rest.split_once(':') {
                                let bins: Vec<String> = if bins_str.is_empty() {
                                    vec![]
                                } else {
                                    bins_str.split(',').map(str::to_string).collect()
                                };
                                *launcher_choice = Some((pkg.to_string(), bins));
                            }
                        } else if let Some(rest) = l.strip_prefix("PROMPT_OUTDATED_PACKAGES:") {
                            *outdated_pkg = Some(rest.to_string());
                        } else if let Some(p) = parse_progress(&l) {
                            *progress = Some(p);
                        } else {
                            log.push(l);
                        }
                    }
                    Ok(Msg::Done(ok)) => { *done = true; *success = ok; }
                    Err(_) => break,
                }
            }
        }

        // Auto-transition to NoLauncherChoice popup when op finishes with the marker set.
        if let Screen::Operation { done: true, success: false, launcher_choice: Some(_), .. } = &app.screen {
            let screen = std::mem::replace(&mut app.screen, Screen::Main);
            if let Screen::Operation { launcher_choice: Some((pkg, bins)), into_target, .. } = screen {
                app.screen = Screen::NoLauncherChoice { pkg, available_bins: bins, selected: 0, into_target };
                app.needs_clear = true;
            }
        }

        // Auto-transition to OutdatedPackages popup when op finishes with the marker set.
        if let Screen::Operation { done: true, success: false, outdated_pkg: Some(_), .. } = &app.screen {
            let screen = std::mem::replace(&mut app.screen, Screen::Main);
            if let Screen::Operation { outdated_pkg: Some(pkg), original_args, .. } = screen {
                app.screen = Screen::OutdatedPackages { pkg, install_args: original_args, selected: 0 };
                app.needs_clear = true;
            }
        }

        // Drain async update-check results
        while let Ok(map) = app.update_rx.try_recv() {
            app.update_available = map;
        }

        // Drain async search results
        loop {
            match app.search_rx.try_recv() {
                Ok((gen, results)) if gen == app.search_gen => {
                    app.search_results = results;
                    app.search_searching = false;
                    app.status = if app.search_results.is_empty() {
                        "No results.".into()
                    } else {
                        format!("{} results", app.search_results.len())
                    };
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }

        app.refresh_running_instances();
        terminal.draw(|f| ui::draw(f, &mut app))?;

        if app.quit {
            return Ok(());
        }

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }

        if let Event::Key(key) = event::read()? {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                return Ok(());
            }
            // Shift+Q force-quits from anywhere, even during running operations
            if key.code == KeyCode::Char('Q') {
                return Ok(());
            }
            handle_key(&mut app, key.code)?;
            if let Some(name) = app.run_request.take() {
                run_app_inline(terminal, &name, &mut app)?;
            }
            if let Some((app_name, cpuinfo_path)) = app.editor_request.take() {
                open_editor_inline(terminal, &app_name, cpuinfo_path, &mut app)?;
            }
            if app.needs_clear {
                app.needs_clear = false;
                terminal.clear()?;
            }
        }
    }
}

/// Suspend the TUI, exec `wryayer run <app>` with inherited stdio so the user
/// sees and interacts with the app on the real terminal, then resume the TUI.
fn run_app_inline(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app_name: &str,
    app: &mut App,
) -> Result<()> {
    // Tear down the TUI's terminal state
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    let exe = std::env::current_exe().unwrap_or_else(|_| "wryayer".into());
    let status = Command::new(&exe)
        .args(["run", app_name])
        .status();

    // Resume the TUI's terminal state
    enable_raw_mode()?;
    terminal.backend_mut().execute(EnterAlternateScreen)?;
    terminal.hide_cursor()?;
    terminal.clear()?;
    app.needs_clear = false;

    match status {
        Ok(s) if !s.success() => {
            app.status = format!("{app_name} exited with status {}", s.code().unwrap_or(-1));
        }
        Err(e) => {
            app.status = format!("failed to launch {app_name}: {e}");
        }
        _ => app.status.clear(),
    }
    Ok(())
}

/// How an encrypted app's container currently stands.
///
/// The GUI needs exactly the same facts on the same refresh cadence, so both
/// the type and the scan that fills it live in `commands::encrypt`.
pub use crate::commands::encrypt::AppEncryption as EncState;

/// Suspend the TUI, let the user pick an editor and edit the cpuinfo file,
/// save "custom" into the app config, then resume the TUI.
fn open_editor_inline(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app_name: &str,
    cpuinfo_path: PathBuf,
    app: &mut App,
) -> Result<()> {
    use std::io::Write;

    // Tear down the TUI's terminal state so we get a normal shell.
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    let editors = find_editors();
    let chosen = if editors.is_empty() {
        eprintln!("No editor found. Install one of: nvim, vim, vi, nano.");
        std::thread::sleep(std::time::Duration::from_secs(2));
        None
    } else if editors.len() == 1 {
        Some(editors[0])
    } else {
        println!("Select editor:");
        for (i, ed) in editors.iter().enumerate() {
            println!("  {}) {ed}", i + 1);
        }
        print!("Choice [1]: ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            Some(editors[0])
        } else {
            trimmed.parse::<usize>().ok()
                .and_then(|n| if n >= 1 && n <= editors.len() { Some(editors[n - 1]) } else { None })
                .or(Some(editors[0]))
        }
    };

    let edited = if let Some(editor) = chosen {
        // Ensure the .spoof/ dir exists.
        if let Some(parent) = cpuinfo_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Pre-populate on first use with the real /proc/cpuinfo so the user
        // has a starting point instead of a blank file.
        if !cpuinfo_path.exists() {
            let seed = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
            let _ = std::fs::write(&cpuinfo_path, seed);
        }
        Command::new(editor)
            .arg(&cpuinfo_path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    } else {
        false
    };

    // Restore the TUI.
    enable_raw_mode()?;
    terminal.backend_mut().execute(EnterAlternateScreen)?;
    terminal.hide_cursor()?;
    terminal.clear()?;
    app.needs_clear = false;

    // If the editor ran and the file exists, update config and return to the right screen.
    if app_name.is_empty() {
        // Global settings: update in-memory global config, return to Settings tab.
        if edited && cpuinfo_path.exists() {
            app.global_config.spoof_cpuinfo = Some("custom".to_string());
            let _ = crate::config::write_global_config(&app.global_config);
            app.status = "CPU info saved.".to_string();
        } else if chosen.is_none() {
            app.status = "No editor available — install nvim, vim, vi, or nano.".to_string();
        }
        app.global_selected = CFG_SPOOF_CPUINFO;
        app.tab = Tab::Settings;
        app.screen = Screen::Main;
    } else if edited && cpuinfo_path.exists() {
        if let Ok(mut cfg) = crate::config::read_config(app_name) {
            cfg.spoof_cpuinfo = Some("custom".to_string());
            let _ = crate::config::write_config(app_name, &cfg);
            app.screen = Screen::Config {
                app_name: app_name.to_string(),
                config: cfg,
                selected: CFG_SPOOF_CPUINFO,
            };
            app.status = "CPU info saved.".to_string();
        }
    } else {
        if let Ok(cfg) = crate::config::read_config(app_name) {
            app.screen = Screen::Config {
                app_name: app_name.to_string(),
                config: cfg,
                selected: CFG_SPOOF_CPUINFO,
            };
        }
        if chosen.is_none() {
            app.status = "No editor available — install nvim, vim, vi, or nano.".to_string();
        }
    }

    Ok(())
}

/// Return the editors from {nvim, vim, vi, nano} that are on PATH, in that order.
fn find_editors() -> Vec<&'static str> {
    ["nvim", "vim", "vi", "nano"]
        .iter()
        .copied()
        .filter(|ed| {
            std::env::var_os("PATH")
                .map(|p| std::env::split_paths(&p).any(|dir| dir.join(ed).exists()))
                .unwrap_or(false)
        })
        .collect()
}

// ── Key dispatch ──────────────────────────────────────────────────────────────

fn handle_key(app: &mut App, code: KeyCode) -> Result<()> {
    // Konami code detection — only listens on the main screen so it doesn't
    // interfere with text input or list navigation in modal screens.
    if matches!(app.screen, Screen::Main) {
        konami_step(app, code);
    }

    // Single exhaustive dispatch on the current screen. The scrutinee borrow of
    // `app.screen` ends at the match arm, leaving each handler free to take
    // `&mut app` (handlers replace `app.screen` on transitions). One arm per
    // screen — no second parallel table, and a new variant is a compile error
    // here until it's routed.
    match &app.screen {
        Screen::Main => on_main(app, code)?,
        Screen::Confirm { .. } => on_confirm(app, code)?,
        Screen::Operation { done: false, .. } => on_op_running(app, code),
        Screen::Operation { done: true, .. } => on_op_done(app, code)?,
        Screen::Config { .. } => on_config(app, code),
        Screen::FileBrowser { .. } => on_file_browser(app, code),
        Screen::SharedDirs { .. } => on_shared_dirs(app, code),
        Screen::InstallTarget { .. } => on_install_target(app, code),
        Screen::OptionPicker { .. } => on_option_picker(app, code),
        Screen::SettingHelp { .. } => on_setting_help(app, code),
        Screen::OptionHelp { .. } => on_option_help(app, code),
        Screen::TextInput { .. } => on_text_input(app, code),
        Screen::KeyHelp => on_key_help(app),
        Screen::RenameApp { .. } => on_rename_app(app, code),
        Screen::DuplicateInstall { .. } => on_duplicate_install(app, code),
        Screen::AlreadyInstalled { .. } => on_already_installed(app, code),
        Screen::NoLauncherChoice { .. } => on_no_launcher_choice(app, code),
        Screen::OutdatedPackages { .. } => on_outdated_packages(app, code),
        Screen::AskShortcut { .. } => on_ask_shortcut(app, code),
        Screen::AskEncrypt { .. } => on_ask_encrypt(app, code),
        Screen::EncryptSecrets { .. } => on_encrypt_secrets(app, code),
        Screen::MasterPassword { .. } => on_master_password(app, code),
        Screen::RevealPasswords { .. } => on_reveal_passwords(app, code),
        Screen::GameExePick { .. } => on_game_exe_pick(app, code),
        Screen::GameNameInput { .. } => on_game_name_input(app, code),
        Screen::GameConfirm { .. } => on_game_confirm(app, code),
        Screen::SnapshotManager { .. } => on_snapshot_manager(app, code),
        Screen::CpuConfig { .. } => on_cpu_config(app, code),
        Screen::BoundApps { .. } => on_bound_apps(app, code),
    }
    Ok(())
}

/// Standard vertical list navigation for the picker popups: ↑/k and ↓/j wrap at
/// the ends, Home/End jump to the edges. Moves `*selected` within `0..len` and
/// returns true when it consumed the key, so a handler can `if list_nav(..) {
/// return; }` before matching its own action keys. A no-op on an empty list.
fn list_nav(selected: &mut usize, len: usize, code: KeyCode) -> bool {
    if len == 0 {
        return false;
    }
    match code {
        KeyCode::Up | KeyCode::Char('k') => {
            *selected = if *selected == 0 { len - 1 } else { *selected - 1 };
            true
        }
        KeyCode::Down | KeyCode::Char('j') => {
            *selected = (*selected + 1) % len;
            true
        }
        KeyCode::Home => {
            *selected = 0;
            true
        }
        KeyCode::End => {
            *selected = len - 1;
            true
        }
        _ => false,
    }
}

/// Snapshot manager: ↑/↓ to choose, Enter to roll back, Esc to close.
fn on_snapshot_manager(app: &mut App, code: KeyCode) {
    let Screen::SnapshotManager { app_name, snaps, selected } = &mut app.screen else { return };
    if list_nav(selected, snaps.len(), code) {
        return;
    }
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.screen = Screen::Main;
            app.needs_clear = true;
        }
        KeyCode::Enter => {
            if let Some(snap) = snaps.get(*selected).cloned() {
                let name = app_name.clone();
                app.screen = Screen::Confirm {
                    title: format!("Rollback '{name}'?"),
                    body: vec![
                        format!("Restore from snapshot: {snap}"),
                        String::new(),
                        "Press y to roll back, n or Esc to cancel.".into(),
                    ],
                    action: PendingAction::Rollback(name, snap),
                    danger: true,
                };
                app.needs_clear = true;
            }
        }
        KeyCode::Char('d') | KeyCode::Delete => {
            if let Some(snap) = snaps.get(*selected).cloned() {
                let name = app_name.clone();
                app.screen = Screen::Confirm {
                    title: "Delete snapshot?".into(),
                    body: vec![
                        format!("Delete snapshot {snap} of '{name}'?"),
                        String::new(),
                        "Press y to delete, n or Esc to cancel.".into(),
                    ],
                    action: PendingAction::DeleteSnapshot(name, snap),
                    danger: true,
                };
                app.needs_clear = true;
            }
        }
        _ => {}
    }
}

/// CPU configurator: edit each field of a user-defined CPU, then save it via the
/// Save button. ↑/↓ move, Enter edits a field / presses Save, `?` shows per-row
/// help, Esc cancels.
fn on_cpu_config(app: &mut App, code: KeyCode) {
    let Screen::CpuConfig { app_name, config, draft, selected, editing, help } = &mut app.screen else { return };
    let nrows = CPU_FIELDS.len() + 1; // fields + Save button

    // ── Help popup: any key dismisses ────────────────────────────────────────
    if *help {
        *help = false;
        return;
    }

    // ── Field edit mode ──────────────────────────────────────────────────────
    if let Some((buf, cur)) = editing {
        match code {
            KeyCode::Esc => { *editing = None; }
            KeyCode::Enter => {
                let v = buf.clone();
                draft.set_field(*selected, v);
                *editing = None;
            }
            // Model name (row 1) and Host (row 9) are free text; the numeric
            // rows in between accept digits only.
            _ => {
                let numeric = *selected >= 2 && *selected != CPU_HOST_ROW;
                edit_input(buf, cur, code, 64, |c| !numeric || c.is_ascii_digit());
            }
        }
        return;
    }

    // ── Navigation mode ──────────────────────────────────────────────────────
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            // Discard: return to the config popup / settings tab unchanged.
            let name = app_name.clone();
            let cfg = config.clone();
            if name.is_empty() {
                app.global_config = cfg;
                app.global_selected = CFG_SPOOF_CPUINFO;
                app.tab = Tab::Settings;
                app.screen = Screen::Main;
            } else {
                app.screen = Screen::Config { app_name: name, config: cfg, selected: CFG_SPOOF_CPUINFO };
            }
            app.needs_clear = true;
        }
        KeyCode::Char('?') => { *help = true; }
        KeyCode::Up | KeyCode::Char('k') => {
            *selected = if *selected == 0 { nrows - 1 } else { *selected - 1 };
        }
        KeyCode::Down | KeyCode::Char('j') => {
            *selected = (*selected + 1) % nrows;
        }
        // Vendor row: switch between the two vendors we have flag sets for.
        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if *selected == 0 => {
            draft.vendor = if draft.vendor == "AuthenticAMD" {
                "GenuineIntel".to_string()
            } else {
                "AuthenticAMD".to_string()
            };
        }
        KeyCode::Enter => {
            if *selected == 0 {
                draft.vendor = if draft.vendor == "AuthenticAMD" {
                    "GenuineIntel".to_string()
                } else {
                    "AuthenticAMD".to_string()
                };
            } else if *selected == CPU_SAVE_ROW {
                let spec = draft.to_spec();
                let name = app_name.clone();
                let mut cfg = config.clone();
                cfg.spoof_cpuinfo = Some(spec);
                if name.is_empty() {
                    app.global_config = cfg;
                    app.global_selected = CFG_SPOOF_CPUINFO;
                    app.tab = Tab::Settings;
                    app.screen = Screen::Main;
                } else {
                    app.screen = Screen::Config { app_name: name, config: cfg, selected: CFG_SPOOF_CPUINFO };
                }
                app.status = "custom CPU saved".to_string();
                app.needs_clear = true;
            } else {
                let v = draft.field(*selected).to_string();
                let n = v.chars().count();
                *editing = Some((v, n));
            }
        }
        _ => {}
    }
}

// ── Main screen ───────────────────────────────────────────────────────────────

fn on_main(app: &mut App, code: KeyCode) -> Result<()> {
    // Tab cycling — always active
    match code {
        KeyCode::Tab => {
            app.tab = match app.tab {
                Tab::Installed => Tab::Install,
                Tab::Install   => Tab::Import,
                Tab::Import    => Tab::Games,
                Tab::Games     => Tab::Space,
                Tab::Space     => Tab::Settings,
                Tab::Settings  => Tab::Installed,
            };
            app.status.clear();
            return Ok(());
        }
        KeyCode::BackTab => {
            app.tab = match app.tab {
                Tab::Installed => Tab::Settings,
                Tab::Install   => Tab::Installed,
                Tab::Import    => Tab::Install,
                Tab::Games     => Tab::Import,
                Tab::Space     => Tab::Games,
                Tab::Settings  => Tab::Space,
            };
            app.status.clear();
            return Ok(());
        }
        _ => {}
    }

    // Esc while the detail panel is focused: exit detail mode, don't quit.
    if code == KeyCode::Esc && matches!(app.tab, Tab::Installed) && app.detail_focused {
        app.detail_focused = false;
        return Ok(());
    }

    // 'q' / Esc quit only when NOT in a text-input context.
    // Install tab: input is active while the search list is not focused.
    // Import tab: always a text input.
    let in_text_input = matches!(app.tab, Tab::Import)
        || (matches!(app.tab, Tab::Install) && !app.search_list_focused);

    if !in_text_input && matches!(code, KeyCode::Char('q') | KeyCode::Esc) {
        app.quit = true;
        return Ok(());
    }

    match app.tab {
        Tab::Installed => on_installed(app, code),
        Tab::Install   => on_install(app, code),
        Tab::Import    => on_import(app, code),
        Tab::Games     => on_games(app, code),
        Tab::Space     => on_space_tab(app, code),
        Tab::Settings  => on_settings_tab(app, code),
    }
    Ok(())
}

// ── Installed tab ─────────────────────────────────────────────────────────────

fn on_installed(app: &mut App, code: KeyCode) {
    let len = app.installed.len();
    if len == 0 {
        return;
    }

    // Detail panel focus mode: scroll up/down, Left/h exits
    if app.detail_focused {
        match code {
            KeyCode::Left | KeyCode::Char('h') => { app.detail_focused = false; }
            KeyCode::Up | KeyCode::Char('k') => {
                app.detail_scroll = app.detail_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.detail_scroll += 1; // clamped in draw_detail
            }
            _ => {}
        }
        return;
    }

    match code {
        KeyCode::Right | KeyCode::Char('l')
            if app.selected_installed().is_some() => {
                app.detail_focused = true;
                app.detail_scroll = 0;
            }
        KeyCode::Up | KeyCode::Char('k') => {
            let i = app.inst_state.selected().unwrap_or(0);
            app.inst_state.select(Some(if i == 0 { len - 1 } else { i - 1 }));
            app.detail_scroll = 0;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let i = app.inst_state.selected().unwrap_or(0);
            app.inst_state.select(Some((i + 1) % len));
            app.detail_scroll = 0;
        }
        KeyCode::Char('r') | KeyCode::Enter => {
            if let Some(m) = app.selected_installed() {
                if m.app.main_binary.is_empty() {
                    app.status = format!(
                        "'{}' has no launcher — reinstall with: wryayer install --bin-names <name> {}",
                        m.app.name,
                        m.app.pkg_name.as_deref().unwrap_or(&m.app.name),
                    );
                } else {
                    // Hand off to the event loop, which will suspend the TUI and
                    // run the app attached to the real terminal. Going through the
                    // operation overlay (with piped stdout) would mangle interactive
                    // and ANSI-art output.
                    app.run_request = Some(m.app.name.clone());
                }
            }
        }
        KeyCode::Char('d') | KeyCode::Delete => {
            if let Some(m) = app.selected_installed() {
                let name = m.app.name.clone();
                if m.app.alias_of.is_none() {
                    let dependents: Vec<String> = app.installed
                        .iter()
                        .filter(|other| other.app.alias_of.as_deref() == Some(&name))
                        .map(|other| other.app.name.clone())
                        .collect();
                    if !dependents.is_empty() {
                        let n = dependents.len();
                        app.screen = Screen::Confirm {
                            title: format!("Remove '{name}' and {n} alias(es)?"),
                            body: vec![
                                format!("Also removes: {}", dependents.join(", ")),
                                String::new(),
                                "Press y to delete all, n or Esc to cancel.".into(),
                            ],
                            action: PendingAction::RemoveCascade(name, dependents),
                            danger: true,
                        };
                        return;
                    }
                }
                app.screen = Screen::Confirm {
                    title: format!("Remove '{name}'?"),
                    body: vec![
                        format!("Delete ~/.wryayer/{name}/ and all launchers?"),
                        String::new(),
                        "Press y to continue, n or Esc to cancel.".into(),
                    ],
                    action: PendingAction::Remove(name),
                    danger: true,
                };
            }
        }
        KeyCode::Char('e') => {
            if let Some(m) = app.selected_installed() {
                let name = m.app.name.clone();
                let zip = format!("{}-{}.zip", name, chrono::Local::now().format("%Y-%m-%d"));
                app.screen = Screen::Confirm {
                    title: format!("Export '{name}'?"),
                    body: vec![
                        format!("Output: ~/{zip}"),
                        String::new(),
                        "Press y to confirm, n or Esc to cancel.".into(),
                    ],
                    action: PendingAction::Export(name),
                    danger: false,
                };
            }
        }
        KeyCode::Char('p') => {
            if let Some(m) = app.selected_installed() {
                let name = m.app.name.clone();
                app.screen = Screen::Confirm {
                    title: format!("Snapshot '{name}'?"),
                    body: vec![
                        "Creates a hard-linked snapshot (instant, near-zero disk).".into(),
                        String::new(),
                        "Press y to confirm, n or Esc to cancel.".into(),
                    ],
                    action: PendingAction::Snapshot(name),
                    danger: false,
                };
            }
        }
        KeyCode::Char('o') => {
            if let Some(m) = app.selected_installed() {
                let name = m.app.name.clone();
                match crate::commands::snapshot::labels(&name) {
                    Ok(snaps) if !snaps.is_empty() => {
                        app.screen = Screen::SnapshotManager { app_name: name, snaps, selected: 0 };
                        app.needs_clear = true;
                    }
                    Ok(_) => app.status = format!("No snapshots for {name}"),
                    Err(e) => app.status = format!("snapshot lookup failed: {e:#}"),
                }
            }
        }
        KeyCode::Char('c') => {
            if let Some(m) = app.selected_installed() {
                let name = m.app.name.clone();
                launch_op(app, format!("Check updates — {name}"), vec!["update".into(), "--check".into(), name], None, false);
            }
        }
        KeyCode::Char('u') => {
            if let Some(m) = app.selected_installed() {
                let name = m.app.name.clone();
                let new_ver = app.update_available.get(&name).cloned();
                let body = match new_ver {
                    Some(ref v) => {
                        let cur = m.packages.iter().find(|p| p.name == name)
                            .map(|p| p.version.as_str()).unwrap_or("?");
                        vec![format!("  {cur}  →  {v}"), String::new(), "Press y to update, n or Esc to cancel.".into()]
                    }
                    None => vec![
                        "Run [c]heck first to verify an update is available.".into(),
                        String::new(),
                        "Press y to update anyway, n or Esc to cancel.".into(),
                    ],
                };
                app.screen = Screen::Confirm {
                    title: format!("Update '{name}'?"),
                    body,
                    action: PendingAction::Update(name),
                    danger: false,
                };
            }
        }
        KeyCode::Char('U') => {
            let mut names: Vec<&String> = app.update_available.keys().collect();
            names.sort();
            let body = if names.is_empty() {
                vec![
                    "No updates detected from the last check.".into(),
                    String::new(),
                    "Press y to update all apps anyway, n or Esc to cancel.".into(),
                ]
            } else {
                let mut b = vec![format!("{} app(s) with updates:", names.len())];
                for n in &names {
                    b.push(format!("  • {n}"));
                }
                b.push(String::new());
                b.push("Press y to update all, n or Esc to cancel.".into());
                b
            };
            app.screen = Screen::Confirm {
                title: "Update all apps?".into(),
                body,
                action: PendingAction::UpdateAll,
                danger: false,
            };
            app.needs_clear = true;
        }
        KeyCode::Char('s') => {
            if let Some(m) = app.selected_installed() {
                let name = m.app.name.clone();
                let config = read_config(&name).unwrap_or_default();
                app.editing_wine_game = m.app.wine_game.as_ref()
                    .map(|w| (w.exe.clone(), w.prefix.clone()));
                app.screen = Screen::Config { app_name: name, config, selected: 0 };
            }
        }
        KeyCode::Char('n') => {
            if let Some(m) = app.selected_installed() {
                let name = m.app.name.clone();
                let value = m.app.display_name.clone().unwrap_or_default();
                app.input_cursor = value.chars().count();
                app.screen = Screen::RenameApp { app_name: name, value };
                app.needs_clear = true;
            }
        }
        KeyCode::Char('?') => {
            app.screen = Screen::KeyHelp;
            app.needs_clear = true;
        }
        _ => {}
    }
}

fn on_key_help(app: &mut App) {
    app.screen = Screen::Main;
    app.needs_clear = true;
}

// ── App rename overlay ────────────────────────────────────────────────────────

fn on_rename_app(app: &mut App, code: KeyCode) {
    {
        let App { screen, input_cursor, .. } = app;
        if let Screen::RenameApp { value, .. } = screen {
            if edit_input(value, input_cursor, code, 256, |_| true) { return; }
        }
    }
    let Screen::RenameApp { app_name, value } = &mut app.screen else { return };
    match code {
        KeyCode::Esc => {
            app.screen = Screen::Main;
            app.needs_clear = true;
        }
        KeyCode::Enter => {
            let name = app_name.clone();
            let v = value.trim().to_string();
            if let Ok(mut m) = crate::manifest::read_manifest(&name) {
                m.app.display_name = if v.is_empty() { None } else { Some(v.clone()) };
                let _ = crate::manifest::write_manifest(&name, &m);
                app.reload_installed();
                app.status = if v.is_empty() {
                    format!("Display name cleared for '{name}'")
                } else {
                    format!("'{name}' renamed to '{v}'")
                };
            }
            app.screen = Screen::Main;
            app.needs_clear = true;
        }
        _ => {}
    }
}

// ── Duplicate install overlay ─────────────────────────────────────────────────

fn on_duplicate_install(app: &mut App, code: KeyCode) {
    {
        let App { screen, input_cursor, .. } = app;
        if let Screen::DuplicateInstall { value, .. } = screen {
            // 'q' is a valid app-name character while typing, so only treat it as
            // quit via the match below when it is not consumed as input.
            if !matches!(code, KeyCode::Char('q')) && edit_input(value, input_cursor, code, 256, |_| true) {
                return;
            }
        }
    }
    let Screen::DuplicateInstall { pkg, value, into } = &mut app.screen else { return };
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.screen = Screen::Main;
            app.needs_clear = true;
        }
        KeyCode::Enter => {
            let new_name = value.trim().to_string();
            if new_name.is_empty() {
                return;
            }
            if app.installed.iter().any(|m| m.app.name == new_name) {
                app.status = format!("'{new_name}' is already taken — pick a different name");
                return;
            }
            let pkg = pkg.clone();
            let into = into.clone();
            let body = match &into {
                None => vec![
                    format!("Creates ~/.wryayer/{new_name}/ alongside the existing '{pkg}'."),
                    String::new(),
                    "Press y to confirm, n or Esc to cancel.".into(),
                ],
                Some(target) => vec![
                    format!("Merges '{pkg}' into ~/.wryayer/{target}/ as alias '{new_name}'."),
                    String::new(),
                    "Press y to confirm, n or Esc to cancel.".into(),
                ],
            };
            install_confirm(
                app,
                format!("Install '{pkg}' as '{new_name}'?"),
                body,
                PendingAction::Install { pkg, app_name: Some(new_name), into },
            );
        }
        _ => {}
    }
}

// ── Already installed choice ──────────────────────────────────────────────────

fn on_already_installed(app: &mut App, code: KeyCode) {
    let Screen::AlreadyInstalled { pkg, selected } = &mut app.screen else { return };
    if list_nav(selected, 2, code) {
        return;
    }
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.screen = Screen::Main;
            app.needs_clear = true;
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let pkg = pkg.clone();
            match *selected {
                0 => {
                    // Install a second copy — show the same target picker as a
                    // first-time install so the user can choose fresh vs merge.
                    let targets: Vec<String> = app.installed
                        .iter()
                        .filter(|m| m.app.alias_of.is_none())
                        .map(|m| m.app.name.clone())
                        .collect();
                    app.screen = Screen::InstallTarget { pkg, targets, selected: 0 };
                    app.needs_clear = true;
                }
                _ => {
                    // Uninstall the existing app.
                    let dependents: Vec<String> = app.installed
                        .iter()
                        .filter(|other| other.app.alias_of.as_deref() == Some(&pkg))
                        .map(|other| other.app.name.clone())
                        .collect();
                    if !dependents.is_empty() {
                        let n = dependents.len();
                        app.screen = Screen::Confirm {
                            title: format!("Remove '{pkg}' and {n} alias(es)?"),
                            body: vec![
                                format!("Also removes: {}", dependents.join(", ")),
                                String::new(),
                                "Press y to delete all, n or Esc to cancel.".into(),
                            ],
                            action: PendingAction::RemoveCascade(pkg, dependents),
                            danger: true,
                        };
                    } else {
                        app.screen = Screen::Confirm {
                            title: format!("Remove '{pkg}'?"),
                            body: vec![
                                format!("Delete ~/.wryayer/{pkg}/ and all launchers?"),
                                String::new(),
                                "Press y to continue, n or Esc to cancel.".into(),
                            ],
                            action: PendingAction::Remove(pkg),
                            danger: true,
                        };
                    }
                }
            }
        }
        _ => {}
    }
}

// ── No-launcher choice popup ──────────────────────────────────────────────────

fn on_no_launcher_choice(app: &mut App, code: KeyCode) {
    let Screen::NoLauncherChoice { pkg, selected, into_target, .. } = &mut app.screen else { return };
    if list_nav(selected, 2, code) {
        return;
    }
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.screen = Screen::Main;
            app.needs_clear = true;
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let pkg = pkg.clone();
            let into_target = into_target.clone();
            match *selected {
                0 => {
                    // Re-run install with --keep-without-launcher, preserving --into if set.
                    let mut args = vec!["install".into(), pkg.clone(), "--keep-without-launcher".into()];
                    if let Some(t) = &into_target {
                        args.extend(["--into".into(), t.clone()]);
                    }
                    launch_op(
                        app,
                        format!("Install — {pkg} (no launcher)"),
                        args,
                        None,
                        true,
                    );
                }
                _ => {
                    // Clean up — install already cleaned up on error; just go back.
                    app.screen = Screen::Main;
                    app.needs_clear = true;
                }
            }
        }
        _ => {}
    }
}

fn on_outdated_packages(app: &mut App, code: KeyCode) {
    let Screen::OutdatedPackages { selected, install_args, pkg, .. } = &mut app.screen else { return };
    if list_nav(selected, 2, code) {
        return;
    }
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.screen = Screen::Main;
            app.needs_clear = true;
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let s = *selected;
            let pkg = pkg.clone();
            let mut args = install_args.clone();
            match s {
                0 => {
                    // Update sources and retry: append --sync-db so the install
                    // command runs 'sudo pacman -Sy' before downloading.
                    args.push("--sync-db".into());
                    launch_op(app, format!("Install — {pkg} (update sources)"), args, None, true);
                }
                _ => {
                    app.screen = Screen::Main;
                    app.needs_clear = true;
                }
            }
        }
        _ => {}
    }
}

// ── Install tab ───────────────────────────────────────────────────────────────

fn on_install(app: &mut App, code: KeyCode) {
    if !app.search_list_focused {
        match code {
            KeyCode::Char(c) => {
                app.search_input.push(c);
                trigger_search(app);
            }
            KeyCode::Backspace => {
                app.search_input.pop();
                trigger_search(app);
            }
            KeyCode::Down
                if !app.search_results.is_empty() => {
                    app.search_list_focused = true;
                    app.avail_state.select(Some(0));
                }
            KeyCode::Enter if !app.selected_pkgs.is_empty() => {
                enqueue_marked(app);
            }
            _ => {}
        }
    } else {
        let len = app.search_results.len();
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                let i = app.avail_state.selected().unwrap_or(0);
                if i == 0 {
                    app.search_list_focused = false;
                    app.avail_state.select(None);
                } else {
                    app.avail_state.select(Some(i - 1));
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let i = app.avail_state.selected().unwrap_or(0);
                if len > 0 {
                    app.avail_state.select(Some((i + 1).min(len - 1)));
                }
            }
            KeyCode::Char(' ') => {
                if let Some(i) = app.avail_state.selected() {
                    if let Some((pkg, _)) = app.search_results.get(i) {
                        let is_installed = app.installed.iter().any(|m| m.app.name == *pkg);
                        if !is_installed {
                            let pkg = pkg.clone();
                            if app.selected_pkgs.contains(&pkg) {
                                app.selected_pkgs.remove(&pkg);
                            } else {
                                app.selected_pkgs.insert(pkg);
                            }
                        }
                    }
                }
            }
            KeyCode::Enter => {
                if !app.selected_pkgs.is_empty() {
                    enqueue_marked(app);
                } else if let Some(pkg) = app.selected_available() {
                    let pkg = pkg.to_string();
                    if app.installed.iter().any(|m| m.app.name == pkg) {
                        // Already installed — let the user choose: install again or uninstall.
                        app.screen = Screen::AlreadyInstalled { pkg, selected: 0 };
                        app.needs_clear = true;
                    } else {
                        let targets: Vec<String> = app.installed
                            .iter()
                            .filter(|m| m.app.alias_of.is_none())
                            .map(|m| m.app.name.clone())
                            .collect();
                        if targets.is_empty() {
                            install_confirm(
                                app,
                                format!("Install '{pkg}'?"),
                                vec![
                                    format!("Installs {pkg} into ~/.wryayer/{pkg}/"),
                                    String::new(),
                                    "Press y to confirm, n or Esc to cancel.".into(),
                                ],
                                PendingAction::Install { pkg, app_name: None, into: None },
                            );
                        } else {
                            app.screen = Screen::InstallTarget { pkg, targets, selected: 0 };
                        }
                    }
                }
            }
            KeyCode::Esc => {
                app.search_list_focused = false;
                app.avail_state.select(None);
            }
            _ => {}
        }
    }
}

fn trigger_search(app: &mut App) {
    let query = app.search_input.trim().to_string();
    if query.is_empty() {
        app.search_gen += 1;
        app.search_results.clear();
        app.search_searching = false;
        app.status.clear();
        return;
    }
    app.search_gen += 1;
    app.search_searching = true;
    app.status = format!("Searching '{query}'…");
    let gen = app.search_gen;
    let tx = app.search_tx.clone();
    thread::spawn(move || {
        let results = crate::distro::pkg_search(&query);
        let _ = tx.send((gen, results));
    });
}

// ── Install target picker ─────────────────────────────────────────────────────

fn on_install_target(app: &mut App, code: KeyCode) {
    let Screen::InstallTarget { pkg, targets, selected } = &mut app.screen else { return };
    let rows = targets.len() + 1; // row 0 = fresh; rest = merge targets
    if list_nav(selected, rows, code) {
        return;
    }

    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.install_queue.clear(); // cancel any pending queue
            app.screen = Screen::Main;
            app.needs_clear = true;
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let pkg = pkg.clone();
            let into = if *selected == 0 {
                None
            } else {
                Some(targets[*selected - 1].clone())
            };
            // If the default name (pkg) is already taken as an installed app,
            // the user must pick a unique alias name before we can confirm.
            let name_taken = app.installed.iter().any(|m| m.app.name == pkg);
            if name_taken {
                app.input_cursor = 0;
                app.screen = Screen::DuplicateInstall {
                    pkg,
                    value: String::new(),
                    into,
                };
                app.needs_clear = true;
                return;
            }
            let (title, body) = match &into {
                None => (
                    format!("Install '{pkg}'?"),
                    vec![
                        format!("Installs {pkg} into ~/.wryayer/{pkg}/"),
                        String::new(),
                        "Press y to confirm, n or Esc to cancel.".into(),
                    ],
                ),
                Some(target) => (
                    format!("Merge '{pkg}' into '{target}'?"),
                    vec![
                        format!("Adds {pkg} (and missing deps) to ~/.wryayer/{target}/"),
                        format!("A launcher ~/bin/{pkg} will be created."),
                        String::new(),
                        "Press y to confirm, n or Esc to cancel.".into(),
                    ],
                ),
            };
            install_confirm(app, title, body, PendingAction::Install { pkg, app_name: None, into });
        }
        _ => {}
    }
}

// ── Import tab ────────────────────────────────────────────────────────────────

fn on_import(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Esc => {
            // Clear input or switch away
            if app.import_input.is_empty() {
                app.tab = Tab::Installed;
            } else {
                app.import_input.clear();
            }
        }
        KeyCode::Char('q') => {
            // In import tab 'q' is intercepted here — typing 'q' into the path
            // To quit from import tab, use Shift+Q
            app.import_input.push('q');
        }
        KeyCode::Char(c) => app.import_input.push(c),
        KeyCode::Backspace => { app.import_input.pop(); }
        KeyCode::Tab | KeyCode::BackTab => {} // handled by on_main already
        KeyCode::F(1) | KeyCode::F(2) => {
            open_file_browser(app, BrowserMode::ImportZip);
        }
        KeyCode::Enter => {
            let raw = app.import_input.trim().to_string();
            if raw.is_empty() {
                return;
            }
            let path = shellexpand::tilde(&raw).into_owned();
            let zip_bytes = std::fs::metadata(&path).ok().map(|m| m.len());
            launch_op(app, format!("Import — {path}"), vec!["import".into(), path], zip_bytes, true);
            app.import_input.clear();
        }
        _ => {}
    }
}

// ── Confirm dialog ────────────────────────────────────────────────────────────

fn on_confirm(app: &mut App, code: KeyCode) -> Result<()> {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            let screen = std::mem::replace(&mut app.screen, Screen::Main);
            if let Screen::Confirm { action, .. } = screen {
                execute_action(app, action);
            }
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.install_queue.clear(); // cancel any pending queue
            app.screen = Screen::Main;
        }
        _ => {}
    }
    Ok(())
}

/// Show the "Install '<pkg>'?" confirmation, or — when the user turned
/// confirm_install off in the global settings — skip it and start the install
/// straight away.
fn install_confirm(app: &mut App, title: String, body: Vec<String>, action: PendingAction) {
    if app.global_config.confirm_install {
        app.screen = Screen::Confirm { title, body, action, danger: false };
    } else {
        execute_action(app, action);
    }
    app.needs_clear = true;
}

/// Ask whether to create a ~/bin shortcut, or — when ask_shortcut is off —
/// launch the install immediately using the create_shortcut default.
fn ask_shortcut_or_launch(app: &mut App, pkg: String, title: String, args: Vec<String>) {
    if app.global_config.ask_shortcut {
        app.screen = Screen::AskShortcut {
            pkg,
            title,
            args,
            selected: if app.global_config.create_shortcut { 0 } else { 1 },
        };
        app.needs_clear = true;
    } else {
        let mut args = args;
        if !app.global_config.create_shortcut {
            args.push("--keep-without-launcher".into());
        }
        ask_encrypt(app, pkg, title, args);
    }
}

/// The install-time choices the encryption prompt offers, as
/// `(label, description, extra install args)`.
pub const ENCRYPT_CHOICES: &[(&str, &str, &[&str])] = &[
    ("No", "install into a plain directory", &[]),
    (
        "Encrypt",
        "type the container password at every launch",
        &["--encrypt"],
    ),
    (
        "Encrypt + master password",
        "keep its password in the master store",
        &["--encrypt", "--encrypt-master"],
    ),
    (
        "Encrypt + generated password",
        "generate a password into the master store",
        &["--encrypt", "--encrypt-master", "--encrypt-generate"],
    ),
];

/// Where an app is asked to go into a container: at install time, or after the
/// fact from its settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncryptAsk {
    /// An install that hasn't happened yet — `--encrypt*` flags on `install`.
    Install,
    /// An app already on disk — flags on `encrypt`.
    Convert,
}

/// Converting an installed app. Index 0 backs out, mirroring
/// [`ENCRYPT_CHOICES`]; the rest are flags for `wryayer encrypt <app>`.
pub const CONVERT_CHOICES: &[(&str, &str, &[&str])] = &[
    ("Leave it as it is", "no container, nothing moves", &[]),
    ("Encrypt", "type the container password at every launch", &[]),
    (
        "Encrypt + master password",
        "kept in the master store, unlocked once per boot",
        &["--master"],
    ),
    (
        "Encrypt + generated password",
        "generate a password into the master store",
        &["--master", "--generate"],
    ),
];

impl EncryptAsk {
    pub fn choices(self) -> &'static [(&'static str, &'static str, &'static [&'static str])] {
        match self {
            Self::Install => ENCRYPT_CHOICES,
            Self::Convert => CONVERT_CHOICES,
        }
    }
}

/// The `--into` target in an install argument list, resolved to the app that
/// actually owns the filesystem tree (following one alias hop).
fn merge_target_root(args: &[String]) -> Option<String> {
    let target = args.windows(2).find(|w| w[0] == "--into").map(|w| w[1].clone())?;
    Some(
        crate::manifest::read_manifest(&target)
            .ok()
            .and_then(|m| m.app.alias_of)
            .unwrap_or(target),
    )
}

/// Ask whether to install into a VeraCrypt container, or go straight to the
/// install when there is nothing to decide.
fn ask_encrypt(app: &mut App, pkg: String, title: String, args: Vec<String>) {
    if !crate::veracrypt::available() {
        launch_op(app, title, args, None, true);
        return;
    }
    // Merging into an app that already lives in a container: the files are
    // written straight into that container, so there is no choice to offer —
    // asking "encrypt?" here would imply a second container that never exists.
    if let Some(root) = merge_target_root(&args) {
        if crate::veracrypt::is_encrypted(&root) {
            begin_merge_into_encrypted(app, title, args, &root);
            return;
        }
    }
    app.screen = Screen::AskEncrypt { pkg, title, args, selected: 0, kind: EncryptAsk::Install };
    app.needs_clear = true;
}

/// Ask how to encrypt an app that is already installed, from its settings.
fn ask_encrypt_app(app: &mut App, app_name: String) {
    app.screen = Screen::AskEncrypt {
        title: format!("Encrypt — {app_name}"),
        args: vec!["encrypt".into(), app_name.clone()],
        pkg: app_name,
        selected: 1,
        kind: EncryptAsk::Convert,
    };
    app.needs_clear = true;
}

/// Confirm before moving an app back out of its container.
///
/// Confirmed rather than done on the spot because it copies the whole tree and
/// throws the container away: not destructive to the app, but not something to
/// start by brushing past a row either.
fn ask_decrypt_app(app: &mut App, app_name: String) {
    app.screen = Screen::Confirm {
        title: format!("Remove encryption from '{app_name}'?"),
        body: vec![
            "Its files are copied out of the container, and the container".into(),
            "is deleted along with any password stored for it.".into(),
            String::new(),
            "The app keeps working — it is just no longer encrypted at rest.".into(),
            String::new(),
            "Press y to confirm, n or Esc to cancel.".into(),
        ],
        action: PendingAction::ConfirmedDecrypt(app_name),
        danger: false,
    };
    app.needs_clear = true;
}

/// The passwords still needed to *open* an existing container.
///
/// Shared by every operation that has to mount one it did not create: merge
/// installs, and removing encryption. Anything already satisfied is skipped, so
/// a master-backed container with sudo still cached asks for nothing.
fn open_container_stages(app_name: &str) -> Vec<SecretStage> {
    let mut stages = Vec::new();
    if !crate::veracrypt::sudo_is_primed() {
        stages.push(SecretStage::Sudo);
    }
    let known = crate::secrets::open_cached()
        .ok()
        .flatten()
        .is_some_and(|s| s.get(app_name).is_some());
    if !known {
        stages.push(SecretStage::ContainerExisting);
    }
    stages
}

/// Start an install into an already-encrypted app: no encryption question, just
/// whatever is needed to open that app's container.
fn begin_merge_into_encrypted(app: &mut App, title: String, args: Vec<String>, root: &str) {
    // The master store may already know this container's password, in which
    // case the install can open it without asking anything.
    let stages = open_container_stages(root);
    if stages.is_empty() {
        launch_encrypted_op(app, title, args, "", "", "");
        return;
    }
    app.screen = Screen::EncryptSecrets {
        title,
        args,
        pass_to_child: true,
        stages,
        idx: 0,
        value: String::new(),
        first_entry: String::new(),
        sudo: String::new(),
        master: String::new(),
        container: String::new(),
        error: None,
    };
    app.input_cursor = 0;
    app.needs_clear = true;
}

/// One field of the set/change-master-password flow.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum MasterStage {
    /// Only asked when a store already exists — proves the user may re-key it.
    Current,
    New,
    Confirm,
}

impl MasterStage {
    pub fn prompt(self) -> &'static str {
        match self {
            MasterStage::Current => "Current master password",
            MasterStage::New => "New master password",
            MasterStage::Confirm => "Repeat the new master password",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            MasterStage::Current => "Proves you may re-key the store.",
            MasterStage::New => "You'll type this once per boot to unlock stored passwords.",
            MasterStage::Confirm => "Must match what you just typed.",
        }
    }
}

/// Open the master-password flow: create the store, or change its password.
fn open_master_password(app: &mut App) {
    let stages = if crate::secrets::exists() {
        vec![MasterStage::Current, MasterStage::New, MasterStage::Confirm]
    } else {
        vec![MasterStage::New, MasterStage::Confirm]
    };
    app.screen = Screen::MasterPassword {
        stages,
        idx: 0,
        value: String::new(),
        current: String::new(),
        new_first: String::new(),
        error: None,
    };
    app.input_cursor = 0;
    app.needs_clear = true;
}

/// Open the stored-password viewer, skipping the prompt when the store is
/// already unlocked for this boot.
fn open_reveal_passwords(app: &mut App) {
    let entries = crate::secrets::open_cached()
        .ok()
        .flatten()
        .map(|store| {
            store
                .apps()
                .into_iter()
                .map(|a| {
                    let pw = store.get(&a).unwrap_or_default().to_string();
                    (a, pw)
                })
                .collect::<Vec<_>>()
        });
    app.screen = Screen::RevealPasswords { entries, value: String::new(), selected: 0, error: None };
    app.input_cursor = 0;
    app.needs_clear = true;
}

fn on_reveal_passwords(app: &mut App, code: KeyCode) {
    let Screen::RevealPasswords { entries, value, selected, error } = &mut app.screen else {
        return;
    };

    // Already unlocked: this is just a list to scroll and close.
    if let Some(list) = entries {
        match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                app.screen = Screen::Main;
                app.needs_clear = true;
            }
            KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') if *selected + 1 < list.len() => {
                *selected += 1;
            }
            _ => {}
        }
        return;
    }

    // Still locked: collect the master password.
    match code {
        KeyCode::Esc => {
            app.screen = Screen::Main;
            app.needs_clear = true;
        }
        KeyCode::Char(c) => {
            value.push(c);
            app.input_cursor = value.chars().count();
        }
        KeyCode::Backspace => {
            value.pop();
            app.input_cursor = value.chars().count();
        }
        KeyCode::Enter => {
            let entered = std::mem::take(value);
            match crate::secrets::open(&entered) {
                Ok(store) => {
                    *entries = Some(
                        store
                            .apps()
                            .into_iter()
                            .map(|a| {
                                let pw = store.get(&a).unwrap_or_default().to_string();
                                (a, pw)
                            })
                            .collect(),
                    );
                    *error = None;
                }
                Err(e) => *error = Some(format!("{e:#}")),
            }
            app.input_cursor = 0;
        }
        _ => {}
    }
}

fn on_master_password(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Esc => {
            app.screen = Screen::Main;
            app.needs_clear = true;
            return;
        }
        KeyCode::Char(c) => {
            if let Screen::MasterPassword { value, .. } = &mut app.screen {
                value.push(c);
                app.input_cursor = value.chars().count();
            }
            return;
        }
        KeyCode::Backspace => {
            if let Screen::MasterPassword { value, .. } = &mut app.screen {
                value.pop();
                app.input_cursor = value.chars().count();
            }
            return;
        }
        KeyCode::Enter => {}
        _ => return,
    }

    let Screen::MasterPassword { stages, idx, value, current, new_first, error } = &mut app.screen
    else {
        return;
    };
    let stage = stages[*idx];
    let entered = std::mem::take(value);
    *error = None;

    match stage {
        MasterStage::Current => {
            // Verify before going further, so a wrong password is caught here
            // rather than after the user has typed a new one twice.
            if let Err(e) = crate::secrets::open(&entered) {
                *error = Some(format!("{e:#}"));
                return;
            }
            *current = entered;
        }
        MasterStage::New => {
            if entered.is_empty() {
                *error = Some("Password must not be empty.".into());
                return;
            }
            *new_first = entered;
        }
        MasterStage::Confirm => {
            if entered != *new_first {
                *error = Some("Passwords did not match — try the new password again.".into());
                *idx -= 1;
                new_first.clear();
                app.input_cursor = 0;
                return;
            }
            let had_store = crate::secrets::exists();
            let result = if had_store {
                crate::secrets::change_master(current, &entered)
            } else {
                crate::secrets::init(&entered)
            };
            match result {
                Ok(()) => {
                    app.screen = Screen::Main;
                    app.needs_clear = true;
                    app.status = if had_store {
                        "Master password changed.".into()
                    } else {
                        "Master password store created — you'll be asked for it once per boot.".into()
                    };
                }
                Err(e) => {
                    *error = Some(format!("{e:#}"));
                    *idx -= 1;
                    new_first.clear();
                    app.input_cursor = 0;
                }
            }
            return;
        }
    }

    *idx += 1;
    app.input_cursor = 0;
}

/// One password an encrypted install needs before it can start.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SecretStage {
    /// Cached with `sudo -v` so VeraCrypt can mount without a terminal prompt.
    Sudo,
    MasterNew,
    MasterNewConfirm,
    MasterExisting,
    ContainerNew,
    ContainerConfirm,
    /// Opens the container of an app being merged into — it already exists, so
    /// it is entered once with no confirmation.
    ContainerExisting,
}

impl SecretStage {
    pub fn prompt(self) -> &'static str {
        match self {
            SecretStage::Sudo => "Your sudo password",
            SecretStage::MasterNew => "New master password",
            SecretStage::MasterNewConfirm => "Repeat the master password",
            SecretStage::MasterExisting => "Master password",
            SecretStage::ContainerNew => "New container password",
            SecretStage::ContainerConfirm => "Repeat the container password",
            SecretStage::ContainerExisting => "Container password",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            SecretStage::Sudo => "VeraCrypt needs root to mount the container.",
            SecretStage::MasterNew => "You'll type this once per boot to unlock stored passwords.",
            SecretStage::MasterNewConfirm => "Must match what you just typed.",
            SecretStage::MasterExisting => "Unlocks the master password store for this boot.",
            SecretStage::ContainerNew => "Opens this app's container. Not stored anywhere.",
            SecretStage::ContainerConfirm => "Must match what you just typed.",
            SecretStage::ContainerExisting => {
                "Opens the container this app is being installed into."
            }
        }
    }
}

/// Which passwords are still needed for this install, in the order to ask.
///
/// Everything already satisfied is skipped: an authenticated sudo, a master
/// store already unlocked this boot, or a generated container password all drop
/// their prompts, so the common repeat case asks for nothing at all.
fn build_secret_stages(use_master: bool, generate: bool) -> Vec<SecretStage> {
    let mut stages = Vec::new();
    if !crate::veracrypt::sudo_is_primed() {
        stages.push(SecretStage::Sudo);
    }
    if use_master {
        if !crate::secrets::exists() {
            stages.push(SecretStage::MasterNew);
            stages.push(SecretStage::MasterNewConfirm);
        } else if !crate::secrets::is_unlocked() {
            stages.push(SecretStage::MasterExisting);
        }
    }
    if !generate {
        stages.push(SecretStage::ContainerNew);
        stages.push(SecretStage::ContainerConfirm);
    }
    stages
}

/// Start an operation that creates a container — an encrypted install, or
/// moving an installed app into one. Collects whatever passwords are still
/// needed, then runs it as a normal operation with its log in the TUI.
fn begin_encrypted_op(app: &mut App, title: String, args: Vec<String>, use_master: bool, generate: bool) {
    let stages = build_secret_stages(use_master, generate);
    if stages.is_empty() {
        launch_encrypted_op(app, title, args, "", "", "");
        return;
    }
    app.screen = Screen::EncryptSecrets {
        title,
        args,
        pass_to_child: true,
        stages,
        idx: 0,
        value: String::new(),
        first_entry: String::new(),
        sudo: String::new(),
        master: String::new(),
        container: String::new(),
        error: None,
    };
    app.input_cursor = 0;
    app.needs_clear = true;
}

/// Run `args` as a normal operation, first caching sudo credentials if the
/// operation will need root and doesn't have them.
///
/// Without this a container unmount inside the child makes `sudo` prompt on the
/// shared terminal, where the TUI's animated progress bar immediately paints
/// over it — leaving an invisible prompt the operation silently hangs on.
fn launch_op_with_sudo(app: &mut App, title: String, args: Vec<String>) {
    if crate::veracrypt::sudo_is_primed() {
        launch_op(app, title, args, None, true);
        return;
    }
    app.screen = Screen::EncryptSecrets {
        title,
        args,
        pass_to_child: false,
        stages: vec![SecretStage::Sudo],
        idx: 0,
        value: String::new(),
        first_entry: String::new(),
        sudo: String::new(),
        master: String::new(),
        container: String::new(),
        error: None,
    };
    app.input_cursor = 0;
    app.needs_clear = true;
}

/// Launch the operation, handing the collected passwords to the child on
/// stdin. `install`, `encrypt` and `decrypt` all read them the same way.
fn launch_encrypted_op(
    app: &mut App,
    title: String,
    mut args: Vec<String>,
    sudo: &str,
    master: &str,
    container: &str,
) {
    args.push("--encrypt-secrets-stdin".into());
    // Only non-empty secrets are sent; the child prompts for anything missing,
    // and an empty line would be taken as an empty password.
    let mut payload = String::new();
    for (key, value) in [("sudo", sudo), ("master", master), ("container", container)] {
        if !value.is_empty() {
            payload.push_str(&format!("{key}={value}\n"));
        }
    }
    launch_op_with_stdin(app, title, args, None, true, Some(payload));
}

fn on_encrypt_secrets(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Esc => {
            app.install_queue.clear();
            app.screen = Screen::Main;
            app.needs_clear = true;
            return;
        }
        KeyCode::Char(c) => {
            if let Screen::EncryptSecrets { value, .. } = &mut app.screen {
                value.push(c);
                app.input_cursor = value.chars().count();
            }
            return;
        }
        KeyCode::Backspace => {
            if let Screen::EncryptSecrets { value, .. } = &mut app.screen {
                value.pop();
                app.input_cursor = value.chars().count();
            }
            return;
        }
        KeyCode::Enter => {}
        _ => return,
    }

    // Enter: validate the current field and advance.
    let Screen::EncryptSecrets {
        stages, idx, value, first_entry, sudo, master, container, error, ..
    } = &mut app.screen
    else {
        return;
    };
    let stage = stages[*idx];
    let entered = std::mem::take(value);
    *error = None;

    match stage {
        SecretStage::Sudo => {
            // Validate immediately: a wrong sudo password would otherwise only
            // surface when VeraCrypt fails, long into the install.
            if let Err(e) = crate::veracrypt::prime_sudo(&entered) {
                *error = Some(format!("{e:#}"));
                return;
            }
            *sudo = entered;
        }
        SecretStage::MasterNew | SecretStage::ContainerNew => {
            if entered.is_empty() {
                *error = Some("Password must not be empty.".into());
                return;
            }
            *first_entry = entered;
        }
        SecretStage::MasterNewConfirm | SecretStage::ContainerConfirm => {
            if entered != *first_entry {
                *error = Some("Passwords did not match — starting that pair again.".into());
                // Step back to the first half of the pair.
                *idx -= 1;
                first_entry.clear();
                app.input_cursor = 0;
                return;
            }
            if stage == SecretStage::MasterNewConfirm {
                *master = std::mem::take(first_entry);
            } else {
                *container = std::mem::take(first_entry);
            }
        }
        SecretStage::ContainerExisting => {
            if entered.is_empty() {
                *error = Some("Password must not be empty.".into());
                return;
            }
            // Not verified here: checking it would mean mounting the container,
            // which needs root and is exactly what the install is about to do.
            // A wrong password surfaces as a clear mount failure in the log.
            *container = entered;
        }
        SecretStage::MasterExisting => {
            // Check it opens the store now, rather than after the install.
            if let Err(e) = crate::secrets::open(&entered) {
                *error = Some(format!("{e:#}"));
                return;
            }
            *master = entered;
        }
    }

    *idx += 1;
    app.input_cursor = 0;
    if *idx < stages.len() {
        return;
    }

    // All collected — hand off to the normal operation runner.
    let screen = std::mem::replace(&mut app.screen, Screen::Main);
    if let Screen::EncryptSecrets { title, args, pass_to_child, sudo, master, container, .. } = screen
    {
        if pass_to_child {
            launch_encrypted_op(app, title, args, &sudo, &master, &container);
        } else {
            // sudo is primed now; the operation needs nothing else.
            launch_op(app, title, args, None, true);
        }
    }
}

fn execute_action(app: &mut App, action: PendingAction) {
    match action {
        PendingAction::Remove(name) => {
            // Double-confirm: first press shows this second dialog
            app.screen = Screen::Confirm {
                title: format!("PERMANENTLY delete '{name}'?"),
                body: vec![
                    "This cannot be undone.".into(),
                    String::new(),
                    "Press y again to permanently delete, n or Esc to cancel.".into(),
                ],
                action: PendingAction::ConfirmedRemove(name),
                danger: true,
            };
        }
        PendingAction::ConfirmedRemove(name) => {
            // Removing an encrypted app unmounts and deletes its container,
            // which needs root.
            let title = format!("Remove — {name}");
            let args = vec!["remove".into(), name.clone()];
            if crate::veracrypt::is_encrypted(&name) {
                launch_op_with_sudo(app, title, args);
            } else {
                launch_op(app, title, args, None, true);
            }
        }
        PendingAction::RemoveCascade(name, aliases) => {
            let alias_list = aliases.join(", ");
            app.screen = Screen::Confirm {
                title: format!("PERMANENTLY delete '{name}' and all aliases?"),
                body: vec![
                    format!("Will delete: {name}, {alias_list}"),
                    "This cannot be undone.".into(),
                    String::new(),
                    "Press y again to confirm, n or Esc to cancel.".into(),
                ],
                action: PendingAction::ConfirmedRemoveCascade(name),
                danger: true,
            };
        }
        PendingAction::ConfirmedRemoveCascade(name) => {
            let title = format!("Remove — {name}");
            let args = vec!["remove".into(), "--cascade".into(), name.clone()];
            if crate::veracrypt::is_encrypted(&name) {
                launch_op_with_sudo(app, title, args);
            } else {
                launch_op(app, title, args, None, true);
            }
        }
        PendingAction::Update(name) =>
            launch_op(app, format!("Update — {name}"), vec!["update".into(), name], None, true),
        PendingAction::UpdateAll =>
            launch_op(app, "Update all apps".into(), vec!["update".into()], None, true),
        PendingAction::Install { pkg, app_name: None, into: None } => {
            let title = format!("Install — {pkg}");
            let args = vec!["install".into(), pkg.clone()];
            ask_shortcut_or_launch(app, pkg, title, args);
        }
        PendingAction::Install { pkg, app_name: Some(an), into: None } => {
            let title = format!("Install — {pkg} as {an}");
            let args = vec!["install".into(), pkg.clone(), "--app-name".into(), an];
            ask_shortcut_or_launch(app, pkg, title, args);
        }
        PendingAction::Install { pkg, app_name, into: Some(target) } => {
            let mut args = vec!["install".into(), pkg.clone(), "--into".into(), target.clone()];
            if let Some(an) = app_name { args.extend(["--app-name".into(), an]); }
            let title = format!("Install — {pkg} → {target}");
            ask_shortcut_or_launch(app, pkg, title, args);
        }
        PendingAction::Export(name) => {
            let total = dir_bytes(&format!(
                "{}/.wryayer/{name}",
                std::env::var("HOME").unwrap_or_default()
            ));
            launch_op(app, format!("Export — {name}"), vec!["export".into(), name], total, false);
        }
        PendingAction::Snapshot(name) =>
            launch_op(app, format!("Snapshot — {name}"), vec!["snapshot".into(), name], None, true),
        PendingAction::Rollback(name, snap) =>
            launch_op(app, format!("Rollback — {name}"), vec!["rollback".into(), name, snap], None, true),
        PendingAction::DeleteSnapshot(name, snap) => {
            match crate::commands::snapshot::delete(&name, &snap) {
                Ok(_) => {
                    app.status = format!("Deleted snapshot {snap}");
                    let snaps = crate::commands::snapshot::labels(&name).unwrap_or_default();
                    app.screen = if snaps.is_empty() {
                        Screen::Main
                    } else {
                        Screen::SnapshotManager { app_name: name, snaps, selected: 0 }
                    };
                }
                Err(e) => {
                    app.status = format!("delete failed: {e:#}");
                    app.screen = Screen::Main;
                }
            }
            app.needs_clear = true;
        }
        PendingAction::ConfirmedDecrypt(name) => {
            // Opening the container needs sudo, and its password unless the
            // master store already holds it.
            let title = format!("Remove encryption — {name}");
            let args = vec!["decrypt".into(), name.clone()];
            let stages = open_container_stages(&name);
            if stages.is_empty() {
                launch_encrypted_op(app, title, args, "", "", "");
            } else {
                app.screen = Screen::EncryptSecrets {
                    title,
                    args,
                    pass_to_child: true,
                    stages,
                    idx: 0,
                    value: String::new(),
                    first_entry: String::new(),
                    sudo: String::new(),
                    master: String::new(),
                    container: String::new(),
                    error: None,
                };
                app.input_cursor = 0;
                app.needs_clear = true;
            }
        }
    }
}

// ── Operation screens ─────────────────────────────────────────────────────────

fn on_op_running(app: &mut App, code: KeyCode) {
    if let Screen::Operation { show_log, log, .. } = &mut app.screen {
        match code {
            KeyCode::Char('t') => {
                *show_log = !*show_log;
                // Opening the log starts at the newest output and keeps up with
                // it, rather than freezing at whatever offset was current.
                if *show_log {
                    app.log_scroll = log.len().saturating_sub(1);
                    app.log_follow = true;
                }
            }
            KeyCode::Up | KeyCode::Char('k') if *show_log => {
                // Scrolling back is an explicit request to stop following.
                app.log_follow = false;
                app.log_scroll = app.log_scroll.min(log.len()).saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') if *show_log => {
                app.log_scroll += 1;
                // Reaching the end resumes following the live output.
                if app.log_scroll >= log.len().saturating_sub(1) {
                    app.log_follow = true;
                }
            }
            _ => {}
        }
    }
}

fn on_op_done(app: &mut App, code: KeyCode) -> Result<()> {
    if let Screen::Operation { show_log, log, .. } = &mut app.screen {
        if code == KeyCode::Char('t') {
            *show_log = !*show_log;
            if *show_log {
                app.log_scroll = log.len().saturating_sub(1);
                app.log_follow = true;
            }
            return Ok(());
        }
        if *show_log {
            match code {
                KeyCode::Up | KeyCode::Char('k') if app.log_scroll > 0 => {
                    app.log_follow = false;
                    app.log_scroll -= 1;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.log_scroll += 1;
                    if app.log_scroll >= log.len().saturating_sub(1) {
                        app.log_follow = true;
                    }
                }
                _ => {}
            }
        }
    }
    match code {
        KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => {
            let screen = std::mem::replace(&mut app.screen, Screen::Main);
            if let Screen::Operation { reload, success, .. } = screen {
                if reload && success {
                    app.reload_installed();
                }
            }
            app.needs_clear = true;
            if !app.install_queue.is_empty() {
                process_install_queue(app);
            }
        }
        _ => {}
    }
    Ok(())
}

// ── Config screen ─────────────────────────────────────────────────────────────

// Rows: 0=network 1=camera 2=microphone 3=audio 4=temp_mode 5=temp_delete 6=shared_dirs
//       7=spoof_hostname 8=spoof_username 9=spoof_machine_id 10=spoof_cpuinfo 11=spoof_os
//       12=spoof_terminal 13=ram_limit 14=avahi
// Per-app Config (no wine_game):  15=bound_apps 16=Save
// Per-app Config (wine_game):     15=bound_apps 16=game_exe 17=game_prefix 18=Save
// Global Settings:                15=create_shortcut 16=confirm_install 17=ask_shortcut
//                                 18=clean_cache 19=theme 20=layout 21=Save
// Named aliases for the boolean/temp rows so the section table and helpers can
// refer to every row symbolically (the values match the historical literals).
pub const CFG_NETWORK: usize = 0;
pub const CFG_CAMERA: usize = 1;
pub const CFG_MICROPHONE: usize = 2;
pub const CFG_AUDIO: usize = 3;
pub const CFG_TEMP_MODE: usize = 4;
pub const CFG_TEMP_DELETE: usize = 5;
pub const CFG_SHARES: usize = 6;
pub const CFG_SPOOF_HOSTNAME: usize = 7;
pub const CFG_SPOOF_USERNAME: usize = 8;
pub const CFG_SPOOF_MACHINE_ID: usize = 9;
pub const CFG_SPOOF_CPUINFO: usize = 10;
pub const CFG_SPOOF_OS: usize = 11;
pub const CFG_SPOOF_TERMINAL: usize = 12;
pub const CFG_RAM_LIMIT: usize = 13;
/// Avahi mode — a shared row shown in both per-app Config and global Settings.
pub const CFG_AVAHI: usize = 14;
/// Bound apps — a per-app-only row that opens the cross-container bind picker.
pub const CFG_BOUND: usize = 15;
/// Wine-game rows (only present when the Config screen carries `wine_game = Some`).
pub const CFG_GAME_EXE: usize = 16;
pub const CFG_GAME_PREFIX: usize = 17;
/// The following are only shown in the global Settings tab, not per-app Config.
/// Their indices sit past the per-app rows (which top out at 17 = wine save), so
/// the shared setting_* helpers never see them from a per-app screen.
pub const CFG_CREATE_SHORTCUT: usize = 15;
pub const CFG_CONFIRM_INSTALL: usize = 16;
pub const CFG_ASK_SHORTCUT: usize = 17;
pub const CFG_CLEAN_CACHE: usize = 18;
pub const CFG_THEME: usize = 19;
pub const CFG_LAYOUT: usize = 20;
pub const CFG_SAVE: usize = 21;
/// Spoofed system uptime. Its storage index sits past every other row (per-app
/// and global) so adding it needs no renumbering of the literal-indexed
/// setting_* arms; its display position is set by SANDBOX_SECTIONS.
pub const CFG_SPOOF_UPTIME: usize = 22;
/// USB / removable-media visibility. Like CFG_SPOOF_UPTIME its storage index
/// sits past every other row so the literal-indexed setting_* arms need no
/// renumbering; its display position is set by SANDBOX_SECTIONS.
pub const CFG_USB: usize = 23;
/// Where an encrypted app's container password comes from. Only shown for apps
/// that are actually stored in a VeraCrypt container; like the two rows above,
/// its storage index sits past every other row so nothing needs renumbering.
pub const CFG_PASSWORD_SOURCE: usize = 24;
/// Set or change the master password (global Settings only). An action row —
/// it opens a masked prompt rather than cycling through values.
pub const CFG_MASTER_PASSWORD: usize = 25;
/// Unmount an encrypted app's container when it exits (per-app).
pub const CFG_LOCK_ON_EXIT: usize = 26;
/// Reveal the container passwords held in the master store (global, action).
pub const CFG_MASTER_SHOW: usize = 27;
/// Forget the cached master key so it must be typed again (global, action).
pub const CFG_MASTER_FORGET: usize = 28;
/// Move a plain app into a new container (per-app, action).
pub const CFG_ENCRYPT_APP: usize = 29;
/// Move an encrypted app back out into a plain directory (per-app, action).
pub const CFG_DECRYPT_APP: usize = 30;
/// Explains why an alias has no encryption of its own (per-app, inert).
pub const CFG_ENCRYPT_ALIAS: usize = 31;
pub const CFG_LEN: usize = 32;

/// Which encryption rows a per-app config screen offers.
///
/// Three cases rather than a bool, because "already encrypted" and "could be
/// encrypted" want different rows, and some apps want neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncryptionRows {
    /// No section at all — nothing could be encrypted even in principle.
    Hidden,
    /// A plain app that could be moved into a container.
    Offer,
    /// Already in a container: its settings, plus the way back out.
    Manage,
    /// An alias, whose files are in another app's tree. It cannot be encrypted
    /// on its own, and saying so is the whole point: an absent section reads as
    /// a missing feature, and this is the case people hit first, because a
    /// merged-in tool is exactly the kind of small app you try it on.
    Alias,
}

impl EncryptionRows {
    /// Decide from the app itself. `is_alias` comes from the caller because it
    /// already has the manifest in hand and this runs on every frame.
    pub fn for_app(app_name: &str, is_alias: bool) -> Self {
        if crate::veracrypt::is_encrypted(app_name) {
            return Self::Manage;
        }
        if !crate::veracrypt::available() {
            return Self::Hidden;
        }
        // An alias owns no filesystem tree — its files live in the target's.
        // Encrypting it would seal an almost empty directory and leave the real
        // files exactly where they were.
        if is_alias {
            return Self::Alias;
        }
        Self::Offer
    }

    fn rows(self) -> Vec<usize> {
        match self {
            Self::Hidden => vec![],
            Self::Offer => vec![CFG_ENCRYPT_APP],
            Self::Manage => vec![CFG_PASSWORD_SOURCE, CFG_LOCK_ON_EXIT, CFG_DECRYPT_APP],
            Self::Alias => vec![CFG_ENCRYPT_ALIAS],
        }
    }
}

/// Index of the Save button in the per-app Config screen. Shifts down as
/// optional row groups (wine game, encryption) appear.
pub fn app_cfg_save_idx(has_wine_game: bool, encryption: EncryptionRows) -> usize {
    // The Save button is drawn right after the last selectable row, so its
    // position is simply the number of navigable rows. Deriving it from
    // config_nav_order keeps it correct as rows are added or removed.
    config_nav_order(false, has_wine_game, encryption).len()
}

/// The sandbox config rows grouped into labelled sections, in display order.
/// Row indices are the stored `CFG_*` values, so the visual grouping is
/// independent of how the settings are numbered internally. Both renderers and
/// ↑/↓ navigation walk this table, letting the on-screen order and section
/// separators differ from the storage layout.
pub const SANDBOX_SECTIONS: &[(&str, &[usize])] = &[
    ("Hardware settings", &[CFG_SPOOF_CPUINFO, CFG_RAM_LIMIT]),
    (
        "Privacy settings",
        &[CFG_NETWORK, CFG_CAMERA, CFG_MICROPHONE, CFG_AUDIO, CFG_USB, CFG_SHARES, CFG_AVAHI],
    ),
    (
        "Environment settings",
        &[
            CFG_SPOOF_HOSTNAME, CFG_SPOOF_USERNAME, CFG_SPOOF_MACHINE_ID,
            CFG_SPOOF_OS, CFG_SPOOF_TERMINAL, CFG_SPOOF_UPTIME,
            CFG_TEMP_MODE, CFG_TEMP_DELETE,
        ],
    ),
];

/// The config/settings screen as an ordered list of `(section title, row
/// indices)`. `is_global` chooses the trailing block: wryayer's own behaviour on
/// the Settings tab, or a per-app Config's bound-apps (+ optional wine-game)
/// rows.
pub fn config_sections(
    is_global: bool,
    has_wine_game: bool,
    encryption: EncryptionRows,
) -> Vec<(&'static str, Vec<usize>)> {
    let mut out: Vec<(&'static str, Vec<usize>)> =
        SANDBOX_SECTIONS.iter().map(|(t, idxs)| (*t, idxs.to_vec())).collect();
    // A plain app gets the one row that offers encryption; an encrypted one
    // gets its settings and the way back out. Apps that can be neither — an
    // alias, or any app when veracrypt isn't installed — get no section rather
    // than a row that can only report failure.
    if !is_global {
        let rows = encryption.rows();
        if !rows.is_empty() {
            out.push(("Encryption", rows));
        }
    }
    if is_global {
        out.push((
            "Application settings",
            vec![
                CFG_CREATE_SHORTCUT, CFG_CONFIRM_INSTALL, CFG_ASK_SHORTCUT,
                CFG_CLEAN_CACHE, CFG_THEME, CFG_LAYOUT,
            ],
        ));
        // The master password store is global by nature — one store covers every
        // encrypted app — so it belongs here rather than in a per-app config.
        // Only offered when VeraCrypt is present, since without it no app can be
        // encrypted and the store would have nothing to protect.
        if crate::veracrypt::available() {
            let mut rows = vec![CFG_MASTER_PASSWORD];
            // Only meaningful once a store exists to reveal or lock.
            if crate::secrets::exists() {
                rows.push(CFG_MASTER_SHOW);
                rows.push(CFG_MASTER_FORGET);
            }
            out.push(("Encryption", rows));
        }
    } else {
        let mut rows = vec![CFG_BOUND];
        if has_wine_game {
            rows.push(CFG_GAME_EXE);
            rows.push(CFG_GAME_PREFIX);
        }
        out.push(("App binding", rows));
    }
    out
}

/// The selectable row indices in display order (Save excluded), used to step
/// ↑/↓ through the screen in the same order it is drawn.
pub fn config_nav_order(
    is_global: bool,
    has_wine_game: bool,
    encryption: EncryptionRows,
) -> Vec<usize> {
    config_sections(is_global, has_wine_game, encryption)
        .into_iter()
        .flat_map(|(_, idxs)| idxs)
        .collect()
}

/// Step from `selected` to the previous (`-1`) or next (`+1`) row in display
/// order, wrapping around, with the Save button (`save_idx`) as the final stop.
pub fn config_nav_step(
    is_global: bool,
    has_wine_game: bool,
    encryption: EncryptionRows,
    save_idx: usize,
    selected: usize,
    dir: i32,
) -> usize {
    let mut order = config_nav_order(is_global, has_wine_game, encryption);
    order.push(save_idx);
    let pos = order.iter().position(|&i| i == selected).unwrap_or(0);
    let len = order.len();
    let next = if dir < 0 { (pos + len - 1) % len } else { (pos + 1) % len };
    order[next]
}

/// A fixed 32-char hex machine-id that apps can use as a plausible-looking ID.
pub const MACHINE_ID_SAMPLE: &str = "cafebabe0011223344556677deadbeef";
/// Generic hostname used by the "sample" option.
pub const HOSTNAME_SAMPLE: &str = "workstation";
/// Generic username used by the "sample" option.
pub const USERNAME_SAMPLE: &str = "user";

fn on_config(app: &mut App, code: KeyCode) {
    // Capture wine_game presence before mut-borrowing app.screen so the row
    // count is known without dropping the borrow.
    let has_wg = app.editing_wine_game.is_some();
    let is_enc = match &app.screen {
        Screen::Config { app_name, .. } => app.encryption_rows_for(app_name),
        _ => EncryptionRows::Hidden,
    };
    let save_idx = app_cfg_save_idx(has_wg, is_enc);

    let Screen::Config { app_name, config, selected } = &mut app.screen else { return };

    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            // Discard changes (including any in-progress wine-game edits)
            app.editing_wine_game = None;
            app.screen = Screen::Main;
            app.needs_clear = true;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            *selected = config_nav_step(false, has_wg, is_enc, save_idx, *selected, -1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            *selected = config_nav_step(false, has_wg, is_enc, save_idx, *selected, 1);
        }
        KeyCode::Right | KeyCode::Char(' ') => {
            if *selected == save_idx {
                let name = app_name.clone();
                let cfg = config.clone();
                save_config_and_wine_game(app, name, cfg);
                return;
            }
            if *selected == CFG_SHARES {
                let name = app_name.clone();
                let dirs = config.shared_dirs.clone();
                let sel = if dirs.is_empty() { 0 } else { dirs.len() - 1 };
                app.screen = Screen::SharedDirs { app_name: name, dirs, selected: sel };
                app.needs_clear = true;
                return;
            }
            if *selected == CFG_BOUND {
                let name = app_name.clone();
                open_bound_apps(app, name);
                return;
            }
            if *selected == CFG_ENCRYPT_ALIAS {
                return; // nothing to do here; `?` explains why
            }
            if *selected == CFG_ENCRYPT_APP || *selected == CFG_DECRYPT_APP {
                let name = app_name.clone();
                let encrypt = *selected == CFG_ENCRYPT_APP;
                // Both leave the config screen: they rewrite the very tree
                // config.ini lives in, so unsaved edits here could not survive
                // the move anyway.
                app.editing_wine_game = None;
                if encrypt { ask_encrypt_app(app, name) } else { ask_decrypt_app(app, name) }
                return;
            }
            if has_wg && (*selected == CFG_GAME_EXE || *selected == CFG_GAME_PREFIX) {
                open_game_field_input(app);
                return;
            }
            cycle_setting(config, *selected, 1);
        }
        KeyCode::Left => {
            // Inverse of Right — cycle backward. Special rows are no-ops.
            let sel = *selected;
            let is_game_row = has_wg && (sel == CFG_GAME_EXE || sel == CFG_GAME_PREFIX);
            // Action rows have nothing to cycle through — Left must not
            // silently start an encryption.
            let is_action =
                sel == CFG_ENCRYPT_APP || sel == CFG_DECRYPT_APP || sel == CFG_ENCRYPT_ALIAS;
            if sel != save_idx && sel != CFG_SHARES && sel != CFG_BOUND && !is_game_row && !is_action {
                cycle_setting(config, sel, -1);
            }
        }
        KeyCode::Enter => {
            if *selected == save_idx {
                let name = app_name.clone();
                let cfg = config.clone();
                save_config_and_wine_game(app, name, cfg);
                return;
            }
            if *selected == CFG_SHARES {
                let name = app_name.clone();
                let dirs = config.shared_dirs.clone();
                let sel = if dirs.is_empty() { 0 } else { dirs.len() - 1 };
                app.screen = Screen::SharedDirs { app_name: name, dirs, selected: sel };
                app.needs_clear = true;
                return;
            }
            if *selected == CFG_BOUND {
                let name = app_name.clone();
                open_bound_apps(app, name);
                return;
            }
            if *selected == CFG_ENCRYPT_ALIAS {
                return; // nothing to do here; `?` explains why
            }
            if *selected == CFG_ENCRYPT_APP || *selected == CFG_DECRYPT_APP {
                let name = app_name.clone();
                let encrypt = *selected == CFG_ENCRYPT_APP;
                // Both leave the config screen: they rewrite the very tree
                // config.ini lives in, so unsaved edits here could not survive
                // the move anyway.
                app.editing_wine_game = None;
                if encrypt { ask_encrypt_app(app, name) } else { ask_decrypt_app(app, name) }
                return;
            }
            if has_wg && (*selected == CFG_GAME_EXE || *selected == CFG_GAME_PREFIX) {
                open_game_field_input(app);
                return;
            }
            // Open the option picker for this row.
            let name = app_name.clone();
            let cfg = config.clone();
            let idx = *selected;
            let cur = setting_current(&cfg, idx);
            app.screen = Screen::OptionPicker {
                app_name: name,
                config: cfg,
                setting_idx: idx,
                selected: cur,
            };
            app.needs_clear = true;
        }
        KeyCode::Char('?') => {
            let name = app_name.clone();
            let cfg = config.clone();
            let sel = *selected;
            app.screen = Screen::SettingHelp { app_name: name, config: cfg, back_selected: sel };
            app.needs_clear = true;
        }
        _ => {}
    }
}

/// Persist config.ini and (if `editing_wine_game` is Some) the wine_game block
/// in the manifest, then return to Main. Consumes `editing_wine_game`.
fn save_config_and_wine_game(app: &mut App, name: String, cfg: AppConfig) {
    let _ = write_config(&name, &cfg);
    if let Some((exe, prefix)) = app.editing_wine_game.take() {
        if let Ok(mut m) = crate::manifest::read_manifest(&name) {
            if let Some(g) = m.app.wine_game.as_mut() {
                g.exe = exe;
                g.prefix = prefix;
                let _ = crate::manifest::write_manifest(&name, &m);
            }
        }
    }
    app.reload_installed();
    app.screen = Screen::Main;
    app.needs_clear = true;
}

/// Open a free-text input for the currently-selected game field (Exe or Prefix).
/// Assumes the active screen is Screen::Config and editing_wine_game is Some.
fn open_game_field_input(app: &mut App) {
    let Screen::Config { app_name, config, selected } = &app.screen else { return };
    let name = app_name.clone();
    let cfg = config.clone();
    let sel = *selected;
    let Some((exe, prefix)) = app.editing_wine_game.as_ref() else { return };
    let value = if sel == CFG_GAME_EXE { exe.clone() } else { prefix.clone() };
    app.input_cursor = value.chars().count();
    app.screen = Screen::TextInput {
        app_name: name,
        config: cfg,
        back_selected: sel,
        field_idx: sel,
        value,
    };
    app.needs_clear = true;
}

// ── Setting helpers (shared by Config screen + OptionPicker) ─────────────────

/// The list of valid choices for the config row at `idx`.
/// Rows 0..=3 are booleans; 4 = temp_mode; 5 = temp_delete.
/// Rows 7..=10 use pickers (system / sample / input, or system / random / sample / input for machine_id).
pub fn setting_options(idx: usize) -> Vec<&'static str> {
    match idx {
        0..=3 => vec!["on", "off"],
        4 => vec!["system", "ramdisk", "local", "uuid"],
        5 => vec!["never", "on_start", "on_close"],
        CFG_SPOOF_HOSTNAME | CFG_SPOOF_USERNAME => vec!["system", "sample", "input", "random"],
        CFG_SPOOF_OS => vec!["system", "Ubuntu", "Arch", "Windows 11", "ArduinoIDE", "input"],
        CFG_SPOOF_CPUINFO => {
            // system, the built-in CPU presets, the field configurator, then the
            // raw-text editor.
            let mut v = vec!["system"];
            v.extend(crate::cpu::CPU_PROFILES.iter().map(|p| p.label));
            v.push("custom");
            v.push("edit");
            v
        }
        CFG_SPOOF_MACHINE_ID => vec!["system", "random", "sample", "input"],
        CFG_SPOOF_TERMINAL => vec!["off", "detect"],
        CFG_SPOOF_UPTIME => vec!["system", "1 hour", "1 day", "1 week", "custom"],
        CFG_RAM_LIMIT => vec!["none", "512 MB", "1 GB", "2 GB", "4 GB", "8 GB", "custom"],
        CFG_AVAHI => vec!["stub", "host", "off"],
        CFG_USB => vec!["on", "off"],
        CFG_PASSWORD_SOURCE => vec!["prompt", "master"],
        CFG_LOCK_ON_EXIT => vec!["on", "off"],
        CFG_CREATE_SHORTCUT => vec!["yes", "no"],
        CFG_CONFIRM_INSTALL | CFG_ASK_SHORTCUT | CFG_CLEAN_CACHE => vec!["on", "off"],
        CFG_THEME => vec!["default", "amber", "matrix"],
        CFG_LAYOUT => vec!["default", "sidebar", "bottom"],
        _ => vec![],
    }
}

/// Human-readable title for the popup that edits the row at `idx`.
pub fn setting_title(idx: usize) -> &'static str {
    match idx {
        0 => "Network",
        1 => "Camera",
        2 => "Microphone",
        3 => "Audio",
        4 => "Temp mode",
        5 => "Temp delete",
        6 => "Shared dirs",
        7 => "Spoof hostname",
        8 => "Spoof username",
        9 => "Spoof machine ID",
        10 => "Spoof CPU info",
        11 => "Spoof OS release",
        12 => "Spoof terminal name",
        13 => "RAM limit",
        14 => "Avahi mode",
        15 => "Default shortcut",
        16 => "Confirm install",
        17 => "Ask shortcut",
        18 => "Clean cache",
        19 => "Colour theme",
        20 => "Layout",
        CFG_SPOOF_UPTIME => "Spoof uptime",
        CFG_USB => "USB / removable media",
        CFG_PASSWORD_SOURCE => "Container password",
        CFG_MASTER_PASSWORD => "Master password",
        CFG_MASTER_SHOW => "Stored passwords",
        CFG_MASTER_FORGET => "Forget master password",
        CFG_LOCK_ON_EXIT => "Lock on exit",
        CFG_ENCRYPT_APP => "Encrypt this app",
        CFG_DECRYPT_APP => "Remove encryption",
        CFG_ENCRYPT_ALIAS => "Encryption (alias)",
        _ => "Option",
    }
}

/// One-paragraph description of what each config row controls.
pub fn setting_description(idx: usize) -> &'static str {
    match idx {
        0 => "Allow outgoing internet access from the sandbox.\n\nDisable to run the app fully offline and prevent all network calls.",
        1 => "Allow the app to access webcam devices (/dev/video*).\n\nDisable to block camera access entirely.",
        2 => "Allow microphone access.\n\nNote: PipeWire/PulseAudio mic is only fully blocked when Audio is also disabled.",
        3 => "Allow audio playback and capture via PipeWire/PulseAudio.\n\nDisabling this also cuts off the mic path through the sound server.",
        4 => "Where the app's /tmp lives:\n\n• system   — share the host /tmp\n• ramdisk  — private in-memory tmpfs, wiped on close\n• local    — persistent ~/.wryayer/<app>/.tmp/\n• uuid     — fresh private dir on each launch",
        5 => "When to clean up the local temp dir (local/uuid modes):\n\n• never    — keep temp between launches\n• on_start — delete before each launch\n• on_close — delete after the app exits",
        6 => "Host directories bind-mounted read-write into the sandbox.\n\nUseful for sharing downloads, projects, or config files between the app and your system.",
        7 => "Override /etc/hostname and $HOSTNAME inside the sandbox.\n\n• system — use the real hostname\n• sample — sets it to 'workstation'\n• input  — type any custom name\n• random — fill with a generated name, kept fixed until you pick random again",
        8 => "Override $USER and $LOGNAME inside the sandbox.\n\n• system — use your real login name\n• sample — sets it to 'user'\n• input  — type any custom name\n• random — fill with a generated name, kept fixed until you pick random again",
        9 => "Override /etc/machine-id inside the sandbox.\n\n• system — real machine ID\n• random — fresh UUID every launch\n• sample — fixed placeholder\n• input  — type a 32-char hex value",
        10 => "Override /proc/cpuinfo inside the sandbox — pick a CPU to present.\n\n• system — expose the real CPU\n• <CPU>  — a built-in profile spanning budget → flagship → server, Intel and AMD\n• custom — open a configurator to build your own CPU field by field (spoofs cpuinfo + CPUID)\n• edit   — open a text editor to write a fully custom file (pre-filled with your real CPU data)",
        11 => "Override /etc/os-release inside the sandbox.\n\nChoose a preset (Ubuntu, Arch, Windows 11, ArduinoIDE) or 'input' to type any OS name.\n'system' exposes the real OS release.",
        12 => "Detect your real terminal emulator and pass its identity into the sandbox.\n\nWalks the process tree to find kitty, foot, alacritty, WezTerm, etc., then sets the matching env var (KITTY_WINDOW_ID, WEZTERM_PANE, …).\n\nFixes fastfetch / neofetch showing 'bwrap' instead of your real terminal.",
        13 => "Maximum RAM the app may use (RAM + swap both capped).\n\nEnforced via systemd-run MemoryMax + MemorySwapMax=0.\n'none' disables the limit. Requires systemd.\n\nPick a preset or 'custom' to type any size with a unit — e.g. 512MB, 1.5GB, 500000KB (KB/MB/GB, 1024-based).",
        14 => "How to answer apps that probe Avahi/zeroconf at startup (Electron/Chromium, KDE, CUPS-linked).\n\n• stub — private in-sandbox stub bus; no host change, no LAN broadcast (default)\n• host — start the host avahi-daemon if it's installed but stopped\n• off  — leave the harmless 'Daemon not running' warning as-is",
        15 => "Whether to pre-select 'Yes' or 'No' in the shortcut prompt shown before each install.\n\nThe prompt always appears — this only controls which answer is highlighted by default.",
        16 => "Whether to show the 'Install <pkg>?' confirmation before installing.\n\n• on  — ask for a y/n confirmation first (default)\n• off — start the install immediately, no prompt",
        17 => "Whether to ask about creating a ~/bin shortcut before installing.\n\n• on  — show the shortcut prompt (default)\n• off — skip it and use the 'Default shortcut' setting above without asking",
        18 => "Delete the shared download/build cache (~/.cache/wryayer) after each successful install.\n\n• on  — wipe the cache every install; leaves no record of installed packages outside ~/.wryayer (useful when that dir is an encrypted container)\n• off — keep the cache to speed up re-installs (default)",
        19 => "Colour palette for the TUI (independent of Layout). Applies immediately.\n\n• default — cool: cyan accent on a dark-blue selection\n• amber   — warm: amber accent on a dark-brown selection\n• matrix  — green-phosphor: the body text itself is green, not white",
        20 => "Structural layout for the TUI (independent of Colour theme). Applies immediately.\n\n• default — horizontal tab strip on top, single-line borders\n• sidebar — vertical tab bar down the left, double-line borders, prompt-style cursor\n• bottom  — horizontal tab strip along the bottom, rounded borders, chevron cursor",
        CFG_SPOOF_UPTIME => "Report a fake system uptime inside the sandbox.\n\nFools fastfetch's 'Uptime', the uptime/w commands, and any sysinfo(2)/CLOCK_BOOTTIME reader via a /proc/uptime overlay plus an LD_PRELOAD shim. Time still advances from the fake value.\n\n• system — show the real uptime\n• 1 hour / 1 day / 1 week — fixed presets\n• custom — type a duration (3d4h, 90m) or bare seconds",
        CFG_USB => "Make USB / removable drives visible inside the sandbox.\n\nBinds the mount roots the desktop (or udisks/udevil/pmount) uses — /run/media, /media and /mnt — so drives show up in the app's file dialogs. Drives plugged in AFTER the app starts appear live, because the host mounts propagate into the sandbox.\n\n• on  — expose removable media\n• off — hide it (default; better isolation)",
        CFG_MASTER_PASSWORD => "The one password that protects every stored container password.\n\nApps set to 'master' keep their container password in an encrypted store (~/.wryayer/.passwords.vault, Argon2id + AES-256-GCM). You type this master password once per boot; after that those apps unlock without prompting.\n\nPress Enter to create it, or to change it if it already exists. Changing it re-encrypts the store — the passwords inside are unaffected, so your containers keep working.",
        CFG_LOCK_ON_EXIT => "Unmount this app's container when the app exits.\n\n• on  — the files become unreadable again the moment you close the app (default). Each launch mounts the container, which needs sudo.\n• off — leave it mounted until you lock it by hand. No sudo prompt per launch, but the files stay readable for the rest of the session.",
        CFG_MASTER_SHOW => "Show the container passwords held in the master store.\n\nThe only way to read a password that was generated rather than typed — those are never printed when they're created. Needs the master password if the store isn't already unlocked this boot.",
        CFG_MASTER_FORGET => "Forget the master key cached for this boot.\n\nThe store itself is untouched; only the cached key in $XDG_RUNTIME_DIR is dropped, so the next app needing a stored password asks for the master password again. Same as 'wryayer master lock'.",
        CFG_PASSWORD_SOURCE => "Where this app's VeraCrypt container password comes from.\n\n• prompt — you type it before every launch, and the container is unmounted again when the app exits. Nothing is stored on disk.\n• master — it is read from the master password store, which you unlock once per boot. The container then stays mounted until you lock it.",
        CFG_ENCRYPT_APP => "Move this app into its own VeraCrypt container.\n\nThe whole tree — binaries, config, browser profile — is copied into an encrypted volume mounted over the app's normal directory. While it is locked nothing in there is readable, filenames included.\n\nThe container is sized from what the app currently occupies, plus headroom. Copying a large app takes a while; the plaintext original is kept until the copy is verified, so an interruption leaves the app exactly as it was.\n\nPress Enter to choose where the password comes from and start.",
        CFG_ENCRYPT_ALIAS => "This app is an alias: it was installed with --into, so its binaries live inside another app's tree and its own directory holds little more than a manifest.\n\nEncrypting it would seal that near-empty directory and leave the real files exactly where they are. Encrypt the app that owns the tree instead — open its settings from the Installed list, or run 'wryayer encrypt <target>'.\n\nOnce that app is encrypted, this alias is inside the container too.",
        CFG_DECRYPT_APP => "Move this app back out into a plain directory.\n\nThe container's contents are copied out, the container is deleted, and any password stored for it is forgotten. The app keeps working exactly as before — it is simply no longer encrypted at rest.\n\nPress Enter to confirm.",
        _ => "No description available.",
    }
}

/// Description of the specific choice `choice_idx` within the setting at `idx`.
pub fn option_description(setting_idx: usize, choice_idx: usize) -> &'static str {
    // The cpuinfo row's presets are data-driven, so describe them from the table.
    if setting_idx == CFG_SPOOF_CPUINFO {
        let n = crate::cpu::CPU_PROFILES.len();
        return match choice_idx {
            0 => "system — Expose the real /proc/cpuinfo to the app. No spoofing.",
            c if c >= 1 && c < 1 + n => crate::cpu::CPU_PROFILES[c - 1].desc,
            c if c == 1 + n => "custom — Open a field-by-field configurator to build your own CPU (vendor, model name, family/model/stepping, cores, threads, MHz, cache). Spoofs both /proc/cpuinfo and CPUID.",
            _ => "edit — Open a text editor to write a fully custom /proc/cpuinfo (pre-filled with your real CPU).",
        };
    }
    match (setting_idx, choice_idx) {
        // Network
        (0, 0) => "on — Allow outgoing network connections from the sandbox.",
        (0, 1) => "off — Block all network access; the app runs fully offline.",
        // Camera
        (1, 0) => "on — Allow the app to access webcam devices (/dev/video*).",
        (1, 1) => "off — Block access to all camera devices.",
        // Microphone
        (2, 0) => "on — Allow microphone access. For full isolation also turn Audio off.",
        (2, 1) => "off — Block microphone access via device permissions.",
        // Audio
        (3, 0) => "on — Allow audio playback and capture via PipeWire/PulseAudio.",
        (3, 1) => "off — Block PipeWire/PulseAudio sockets; also cuts off the sound-server mic path.",
        // Temp mode
        (4, 0) => "system — Use the host /tmp. Fast, but shared with the rest of the system.",
        (4, 1) => "ramdisk — Mount a private in-memory tmpfs as /tmp. Fast, fully isolated, and wiped when the app exits.",
        (4, 2) => "local — Use ~/.wryayer/<app>/.tmp/ as /tmp. Persists across launches; controlled by Temp delete.",
        (4, 3) => "uuid — Create a fresh private temp dir on each launch. Maximum isolation; nothing from prior runs is reused.",
        // Temp delete
        (5, 0) => "never — Keep the temp dir between launches. Useful for apps that cache heavy data in /tmp.",
        (5, 1) => "on_start — Delete and recreate the temp dir before each launch; always starts fresh.",
        (5, 2) => "on_close — Delete the temp dir after the app exits; cleans up automatically.",
        // Hostname
        (7, 0) => "system — Use the real hostname from the host. No spoofing.",
        (7, 1) => "sample — Set hostname to 'workstation'. Generic name that won't expose your machine.",
        (7, 2) => "input — Type a custom hostname. Saved to /etc/hostname and $HOSTNAME inside the sandbox.",
        (7, 3) => "random — Fill with a generated hostname (e.g. desktop-a3f9c1). Saved as a fixed value; it only changes when you pick random again.",
        // Username
        (8, 0) => "system — Use your real login name. No spoofing.",
        (8, 1) => "sample — Set username to 'user'. Generic name that won't expose your real login.",
        (8, 2) => "input — Type a custom username. Applied to $USER and $LOGNAME inside the sandbox.",
        (8, 3) => "random — Fill with a generated username (e.g. max47). Saved as a fixed value; it only changes when you pick random again.",
        // Machine ID
        (9, 0) => "system — Use the real /etc/machine-id from the host (no spoofing).",
        (9, 1) => "random — Generate a fresh 32-char hex UUID on every launch. Prevents cross-session fingerprinting.",
        (9, 2) => "sample — Use a fixed placeholder ID: cafebabe0011223344556677deadbeef. Same every run, but not your real ID.",
        (9, 3) => "input — Type your own 32-char hex machine-id. Useful for reproducing a specific identity.",
        // CPU info (row 10) is handled by the data-driven early return above.
        // OS release
        (11, 0) => "system — Expose the real /etc/os-release to the app. No spoofing.",
        (11, 1) => "Ubuntu — Presents as Ubuntu 24.04 LTS. Apps see NAME=Ubuntu, ID=ubuntu, VERSION_ID=24.04.",
        (11, 2) => "Arch — Presents as Arch Linux. Apps see NAME=\"Arch Linux\", ID=arch, BUILD_ID=rolling.",
        (11, 3) => "Windows 11 — Presents as Windows 11. Apps see NAME=\"Windows 11\", ID=windows, VERSION_ID=11.",
        (11, 4) => "ArduinoIDE — Presents as ArduinoIDE. Apps see NAME=ArduinoIDE, ID=arduinoide, VERSION_ID=2.3.",
        (11, 5) => "input — Type a custom OS name (e.g. 'fedora'). Used as ID= and NAME= in /etc/os-release inside the sandbox.",
        // Spoof terminal
        (12, 0) => "off — Do not override terminal identity. Tools like fastfetch may show 'bwrap' as the terminal.",
        (12, 1) => "detect — Walk the process tree to find the real terminal (kitty, foot, alacritty, WezTerm, …) and set the correct env var inside the sandbox. Fixes fastfetch showing 'bwrap'.",
        // Uptime
        (CFG_SPOOF_UPTIME, 0) => "system — Report the machine's real uptime.",
        (CFG_SPOOF_UPTIME, 1) => "1 hour — Report a fixed uptime of one hour.",
        (CFG_SPOOF_UPTIME, 2) => "1 day — Report a fixed uptime of one day.",
        (CFG_SPOOF_UPTIME, 3) => "1 week — Report a fixed uptime of one week.",
        (CFG_SPOOF_UPTIME, 4) => "custom — Type a duration (3d4h, 90m) or bare seconds. Fools fastfetch, uptime/w, and sysinfo(2)/CLOCK_BOOTTIME readers.",
        // USB / removable media
        (CFG_USB, 0) => "on — Bind /run/media, /media and /mnt so USB drives (incl. ones plugged in after launch) show up in the app.",
        (CFG_USB, 1) => "off — Hide removable media from the sandbox (default; better isolation).",
        // RAM limit
        (13, 0) => "none — No RAM limit. The app may use as much memory as the system allows.",
        (13, 1) => "512 MB — Hard cap at 512 MB (RAM + swap). Processes are OOM-killed if they exceed this.",
        (13, 2) => "1 GB — Cap the app at 1 GB of RAM.",
        (13, 3) => "2 GB — Cap the app at 2 GB of RAM. Good default for everyday apps.",
        (13, 4) => "4 GB — Cap the app at 4 GB of RAM.",
        (13, 5) => "8 GB — Cap the app at 8 GB of RAM.",
        (13, 6) => "custom — Type a size: <number> <KB|MB|GB>, e.g. 256 MB, 2 GB, 500000 KB. 1024-based.",
        // Avahi mode
        (14, 0) => "stub — Private in-sandbox stub bus answers avahi-client so apps don't error, with no host change and no LAN broadcast. Everything lives under ~/.wryayer/<app>/.",
        (14, 1) => "host — Start the host avahi-daemon if it's installed but stopped. A host-wide change that also advertises this machine on the local network.",
        (14, 2) => "off — Do nothing; apps that probe Avahi print a harmless 'Daemon not running' warning.",
        // Default shortcut
        (15, 0) => "yes — Pre-select 'Yes' in the shortcut prompt. The prompt still appears; press Enter to confirm quickly.",
        (15, 1) => "no — Pre-select 'No' in the shortcut prompt. Useful if you rarely want ~/bin shortcuts.",
        // Confirm install
        (16, 0) => "on — Show the 'Install <pkg>?' confirmation before every install.",
        (16, 1) => "off — Skip the confirmation and start installing right away.",
        // Ask shortcut
        (17, 0) => "on — Ask whether to create a ~/bin shortcut before each install.",
        (17, 1) => "off — Don't ask; silently apply the 'Default shortcut' setting.",
        // Clean cache
        (18, 0) => "on — Wipe ~/.cache/wryayer after every install. No record of installed packages is left outside ~/.wryayer.",
        (18, 1) => "off — Keep the download/build cache between installs to avoid re-downloading and re-building.",
        // Colour theme
        (19, 0) => "default — Cool palette: cyan accent, dark-blue selection, green/red status colours.",
        (19, 1) => "amber — Warm palette: amber accent, dark-brown selection, warm status colours.",
        (19, 2) => "matrix — Green-phosphor palette: green body text (not white) on a dark-green selection.",
        // Layout
        (20, 0) => "default — Horizontal tab strip across the top with single-line panel borders.",
        (20, 1) => "sidebar — Vertical tab bar down the left edge, double-line borders and a '> ' cursor, for a terminal feel.",
        (20, 2) => "bottom — Horizontal tab strip along the bottom edge, rounded panel borders and a '» ' cursor.",
        // Container password source
        (CFG_PASSWORD_SOURCE, 0) => "prompt — Ask for the container password before every launch, and unmount it again when the app exits. Nothing is stored on disk.",
        (CFG_PASSWORD_SOURCE, 1) => "master — Take the password from the master password store, unlocked once per boot.",
        // Lock on exit
        (CFG_LOCK_ON_EXIT, 0) => "on — Unmount the container as soon as the app exits, so its files stop being readable (default).",
        (CFG_LOCK_ON_EXIT, 1) => "off — Leave the container mounted after the app exits. Avoids a sudo prompt per launch; the files stay readable until locked.",
        _ => "No description available.",
    }
}

/// Which option index in `setting_options(idx)` matches the current value
/// stored in `config`.
pub fn setting_current(config: &AppConfig, idx: usize) -> usize {
    match idx {
        0 => if config.network { 0 } else { 1 },
        1 => if config.camera { 0 } else { 1 },
        2 => if config.microphone { 0 } else { 1 },
        3 => if config.audio { 0 } else { 1 },
        4 => match config.temp_mode {
            TempMode::System  => 0,
            TempMode::Ramdisk => 1,
            TempMode::Local   => 2,
            TempMode::Uuid    => 3,
        },
        5 => match config.temp_delete {
            LocalDelete::Never   => 0,
            LocalDelete::OnStart => 1,
            LocalDelete::OnClose => 2,
        },
        CFG_SPOOF_HOSTNAME => match config.spoof_hostname.as_deref() {
            None => 0,
            Some(v) if v == HOSTNAME_SAMPLE => 1,
            _ => 2,
        },
        CFG_SPOOF_USERNAME => match config.spoof_username.as_deref() {
            None => 0,
            Some(v) if v == USERNAME_SAMPLE => 1,
            _ => 2,
        },
        CFG_SPOOF_MACHINE_ID => match config.spoof_machine_id.as_deref() {
            None             => 0,
            Some("random")   => 1,
            Some(v) if v == MACHINE_ID_SAMPLE => 2,
            _                => 3,
        },
        CFG_SPOOF_CPUINFO => {
            let opts = setting_options(CFG_SPOOF_CPUINFO).len();
            let edit = opts - 1;          // raw-text editor
            let custom = opts - 2;        // field configurator
            match config.spoof_cpuinfo.as_deref() {
                None           => 0,
                Some(v) if v.starts_with("custom:") => custom,
                Some(v) => v
                    .strip_prefix("preset:")
                    .and_then(|k| crate::cpu::CPU_PROFILES.iter().position(|p| p.key == k))
                    .map(|pos| 1 + pos)
                    // Legacy "sample" / bare "custom" values show as "edit".
                    .unwrap_or(edit),
            }
        }
        CFG_SPOOF_OS => match config.spoof_os.as_deref() {
            None               => 0,
            Some("ubuntu")     => 1,
            Some("arch")       => 2,
            Some("windows")    => 3,
            Some("arduinoide") => 4,
            _                  => 5,
        },
        CFG_SPOOF_TERMINAL => usize::from(config.spoof_terminal),
        CFG_USB => if config.usb { 0 } else { 1 },
        CFG_PASSWORD_SOURCE => match config.password_source {
            PasswordSource::Prompt => 0,
            PasswordSource::Master => 1,
        },
        CFG_LOCK_ON_EXIT => usize::from(!config.lock_on_exit),
        // Seconds. Exact preset -> its index; any other value -> "custom".
        CFG_SPOOF_UPTIME => match config.spoof_uptime {
            None            => 0,
            Some(3600)      => 1, // 1 hour
            Some(86400)     => 2, // 1 day
            Some(604800)    => 3, // 1 week
            Some(_)         => 4, // custom
        },
        // Values are KiB. Exact preset -> its index; any other value -> "custom".
        CFG_RAM_LIMIT => match config.ram_limit {
            None             => 0,
            Some(524288)     => 1, // 512 MiB
            Some(1048576)    => 2, // 1 GiB
            Some(2097152)    => 3, // 2 GiB
            Some(4194304)    => 4, // 4 GiB
            Some(8388608)    => 5, // 8 GiB
            Some(_)          => 6, // custom
        },
        CFG_AVAHI => match config.avahi {
            AvahiMode::Stub => 0,
            AvahiMode::Host => 1,
            AvahiMode::Off  => 2,
        },
        CFG_CREATE_SHORTCUT => if config.create_shortcut { 0 } else { 1 },
        CFG_CONFIRM_INSTALL => if config.confirm_install { 0 } else { 1 },
        CFG_ASK_SHORTCUT => if config.ask_shortcut { 0 } else { 1 },
        CFG_CLEAN_CACHE => if config.clean_cache { 0 } else { 1 },
        CFG_THEME => match config.theme {
            Theme::Default => 0,
            Theme::Amber => 1,
            Theme::Matrix => 2,
        },
        CFG_LAYOUT => match config.layout {
            Layout::Default => 0,
            Layout::Sidebar => 1,
            Layout::Bottom => 2,
        },
        _ => 0,
    }
}

/// Write the option `choice` (an index into `setting_options(idx)`) into the
/// matching config field. Out-of-range pairs are silent no-ops.
pub fn apply_setting(config: &mut AppConfig, idx: usize, choice: usize) {
    match (idx, choice) {
        (0, 0) => config.network = true,
        (0, 1) => config.network = false,
        (1, 0) => config.camera = true,
        (1, 1) => config.camera = false,
        (2, 0) => config.microphone = true,
        (2, 1) => config.microphone = false,
        (3, 0) => config.audio = true,
        (3, 1) => config.audio = false,
        (4, 0) => config.temp_mode = TempMode::System,
        (4, 1) => config.temp_mode = TempMode::Ramdisk,
        (4, 2) => config.temp_mode = TempMode::Local,
        (4, 3) => config.temp_mode = TempMode::Uuid,
        (5, 0) => config.temp_delete = LocalDelete::Never,
        (5, 1) => config.temp_delete = LocalDelete::OnStart,
        (5, 2) => config.temp_delete = LocalDelete::OnClose,
        (7, 0) => config.spoof_hostname = None,
        (7, 1) => config.spoof_hostname = Some(HOSTNAME_SAMPLE.to_string()),
        // (7, 2) = "input" — handled by on_option_picker which opens TextInput
        (7, 3) => config.spoof_hostname = Some(crate::config::random_hostname()),
        (8, 0) => config.spoof_username = None,
        (8, 1) => config.spoof_username = Some(USERNAME_SAMPLE.to_string()),
        // (8, 2) = "input" — handled by on_option_picker which opens TextInput
        (8, 3) => config.spoof_username = Some(crate::config::random_username()),
        (9, 0) => config.spoof_machine_id = None,
        (9, 1) => config.spoof_machine_id = Some("random".to_string()),
        (9, 2) => config.spoof_machine_id = Some(MACHINE_ID_SAMPLE.to_string()),
        // (9, 3) = "input" — handled by on_option_picker which opens TextInput
        (10, 0) => config.spoof_cpuinfo = None,
        // (10, 1..=N) = built-in CPU presets; (10, N+1) = "custom", (10, N+2) = "edit"
        (10, c) if c >= 1 && c < 1 + crate::cpu::CPU_PROFILES.len() => {
            config.spoof_cpuinfo = Some(format!("preset:{}", crate::cpu::CPU_PROFILES[c - 1].key));
        }
        (11, 0) => config.spoof_os = None,
        (11, 1) => config.spoof_os = Some("ubuntu".to_string()),
        (11, 2) => config.spoof_os = Some("arch".to_string()),
        (11, 3) => config.spoof_os = Some("windows".to_string()),
        (11, 4) => config.spoof_os = Some("arduinoide".to_string()),
        // (11, 5) = "input" — handled by on_option_picker which opens TextInput
        (12, 0) => config.spoof_terminal = false,
        (12, 1) => config.spoof_terminal = true,
        (CFG_USB, 0) => config.usb = true,
        (CFG_USB, 1) => config.usb = false,
        (CFG_PASSWORD_SOURCE, 0) => config.password_source = PasswordSource::Prompt,
        (CFG_PASSWORD_SOURCE, 1) => config.password_source = PasswordSource::Master,
        (CFG_LOCK_ON_EXIT, 0) => config.lock_on_exit = true,
        (CFG_LOCK_ON_EXIT, 1) => config.lock_on_exit = false,
        // Uptime values are seconds. "custom" (_, 4) opens a text input instead.
        (CFG_SPOOF_UPTIME, 0) => config.spoof_uptime = None,
        (CFG_SPOOF_UPTIME, 1) => config.spoof_uptime = Some(3600),   // 1 hour
        (CFG_SPOOF_UPTIME, 2) => config.spoof_uptime = Some(86400),  // 1 day
        (CFG_SPOOF_UPTIME, 3) => config.spoof_uptime = Some(604800), // 1 week
        // (CFG_SPOOF_UPTIME, 4) = "custom" — handled by on_option_picker's TextInput
        // RAM-limit values are KiB. "custom" (13, 6) opens a text input instead.
        (13, 0) => config.ram_limit = None,
        (13, 1) => config.ram_limit = Some(524288),  // 512 MiB
        (13, 2) => config.ram_limit = Some(1048576), // 1 GiB
        (13, 3) => config.ram_limit = Some(2097152), // 2 GiB
        (13, 4) => config.ram_limit = Some(4194304), // 4 GiB
        (13, 5) => config.ram_limit = Some(8388608), // 8 GiB
        // (13, 6) = "custom" — handled by on_option_picker which opens TextInput
        (14, 0) => config.avahi = AvahiMode::Stub,
        (14, 1) => config.avahi = AvahiMode::Host,
        (14, 2) => config.avahi = AvahiMode::Off,
        (15, 0) => config.create_shortcut = true,
        (15, 1) => config.create_shortcut = false,
        (16, 0) => config.confirm_install = true,
        (16, 1) => config.confirm_install = false,
        (17, 0) => config.ask_shortcut = true,
        (17, 1) => config.ask_shortcut = false,
        (18, 0) => config.clean_cache = true,
        (18, 1) => config.clean_cache = false,
        (19, 0) => config.theme = Theme::Default,
        (19, 1) => config.theme = Theme::Amber,
        (19, 2) => config.theme = Theme::Matrix,
        (20, 0) => config.layout = Layout::Default,
        (20, 1) => config.layout = Layout::Sidebar,
        (20, 2) => config.layout = Layout::Bottom,
        _ => {}
    }
}

/// Cycle the setting at `idx` forward (`dir == 1`) or backward (`dir == -1`).
/// Wraps at the ends of the option list.
pub fn cycle_setting(config: &mut AppConfig, idx: usize, dir: i32) {
    let opts = setting_options(idx);
    let n = opts.len();
    if n == 0 { return; }
    let cur = setting_current(config, idx);
    // "input" / "edit" / "custom" open an editor and can't be applied by cycling,
    // so skip over them — ←/→ moves among the concrete choices and wraps past the
    // deferred ones instead of getting stuck on them.
    let step = if dir > 0 { 1 } else { n - 1 };
    let mut next = (cur + step) % n;
    for _ in 0..n {
        if !matches!(opts[next], "input" | "edit" | "custom") {
            break;
        }
        next = (next + step) % n;
    }
    apply_setting(config, idx, next);
}

// ── Option picker overlay ─────────────────────────────────────────────────────

fn on_option_picker(app: &mut App, code: KeyCode) {
    let Screen::OptionPicker { app_name, config, setting_idx, selected } = &mut app.screen else { return };
    let n = setting_options(*setting_idx).len();
    if n == 0 {
        // Pathological: pop back to config
        let name = app_name.clone();
        let cfg = config.clone();
        let idx = *setting_idx;
        app.screen = Screen::Config { app_name: name, config: cfg, selected: idx };
        app.needs_clear = true;
        return;
    }
    if list_nav(selected, n, code) {
        return;
    }

    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            // Discard the picker choice. Per-app pickers return to the
            // per-app Config popup; global-defaults pickers (empty app_name)
            // return to the Settings tab.
            let name = app_name.clone();
            let cfg = config.clone();
            let idx = *setting_idx;
            if name.is_empty() {
                app.global_config = cfg;
                app.global_selected = idx;
                app.tab = Tab::Settings;
                app.screen = Screen::Main;
            } else {
                app.screen = Screen::Config { app_name: name, config: cfg, selected: idx };
            }
            app.needs_clear = true;
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let name = app_name.clone();
            let mut cfg = config.clone();
            let idx = *setting_idx;
            let choice = *selected;
            let cpu_opts = setting_options(CFG_SPOOF_CPUINFO).len();
            // cpuinfo "custom" (second-to-last) → open the field configurator.
            if idx == CFG_SPOOF_CPUINFO && choice == cpu_opts - 2 {
                let draft = Box::new(CpuDraft::from_config(&cfg));
                app.screen = Screen::CpuConfig {
                    app_name: name,
                    config: cfg,
                    draft,
                    selected: 0,
                    editing: None,
                    help: false,
                };
                app.needs_clear = true;
                return;
            }
            // cpuinfo "edit" (the last option) → tear down TUI, open editor.
            if idx == CFG_SPOOF_CPUINFO && choice == cpu_opts - 1 {
                let name2 = name.clone();
                let cpuinfo_file = crate::manifest::app_dir(&name)
                    .map(|d| d.join(".spoof").join("cpuinfo"));
                app.screen = Screen::Main;
                app.needs_clear = true;
                match cpuinfo_file {
                    Ok(path) => { app.editor_request = Some((name2, path)); }
                    Err(_) => { app.status = "error: cannot find app directory".to_string(); }
                }
                return;
            }

            // "input" option opens the free-text overlay for hostname/username/machine-id/os/resolution.
            let is_input_choice = match idx {
                CFG_SPOOF_HOSTNAME | CFG_SPOOF_USERNAME => choice == 2,
                CFG_SPOOF_OS => choice == 5,
                CFG_SPOOF_MACHINE_ID => choice == 3,
                CFG_SPOOF_UPTIME => choice == 4, // "custom"
                CFG_RAM_LIMIT => choice == 6, // "custom"
                _ => false,
            };
            if is_input_choice {
                let current = match idx {
                    CFG_SPOOF_HOSTNAME    => cfg.spoof_hostname.clone().unwrap_or_default(),
                    CFG_SPOOF_USERNAME    => cfg.spoof_username.clone().unwrap_or_default(),
                    CFG_SPOOF_MACHINE_ID  => cfg.spoof_machine_id.clone().unwrap_or_default(),
                    CFG_SPOOF_OS          => cfg.spoof_os.clone().unwrap_or_default(),
                    // Pre-fill the uptime input with the current value as e.g. "3d4h".
                    CFG_SPOOF_UPTIME      => cfg.spoof_uptime.map(crate::config::format_uptime).unwrap_or_default(),
                    // Pre-fill the RAM input with the current limit as e.g. "2 GB".
                    CFG_RAM_LIMIT         => cfg.ram_limit.map(crate::config::format_ram_limit).unwrap_or_default(),
                    _ => String::new(),
                };
                // Clear pre-fill when current value is one of the fixed presets.
                let is_preset = match idx {
                    CFG_SPOOF_HOSTNAME    => current == HOSTNAME_SAMPLE,
                    CFG_SPOOF_USERNAME    => current == USERNAME_SAMPLE,
                    CFG_SPOOF_MACHINE_ID  => current == "random" || current == MACHINE_ID_SAMPLE,
                    CFG_SPOOF_OS          => matches!(current.as_str(), "ubuntu" | "arch" | "windows" | "arduinoide"),
                    // format_uptime renders the presets as these compact strings.
                    CFG_SPOOF_UPTIME      => matches!(current.as_str(), "1h" | "1d" | "1w"),
                    _ => false,
                };
                let value = if is_preset || current.is_empty() { String::new() } else { current };
                app.input_cursor = value.chars().count();
                app.screen = Screen::TextInput {
                    app_name: name,
                    config: cfg,
                    back_selected: idx,
                    field_idx: idx,
                    value,
                };
                app.needs_clear = true;
                return;
            }
            apply_setting(&mut cfg, idx, choice);
            if name.is_empty() {
                // Global settings mode: update in-memory config and return to Settings tab
                app.global_config = cfg;
                app.global_selected = idx;
                app.tab = Tab::Settings;
                app.screen = Screen::Main;
            } else {
                app.screen = Screen::Config { app_name: name, config: cfg, selected: idx };
            }
            app.needs_clear = true;
        }
        KeyCode::Char('?') => {
            let name = app_name.clone();
            let cfg = config.clone();
            let idx = *setting_idx;
            let sel = *selected;
            app.screen = Screen::OptionHelp {
                app_name: name,
                config: cfg,
                setting_idx: idx,
                picker_selected: sel,
            };
            app.needs_clear = true;
        }
        _ => {}
    }
}

// ── Option help popup ─────────────────────────────────────────────────────────

fn on_option_help(app: &mut App, _code: KeyCode) {
    let Screen::OptionHelp { app_name, config, setting_idx, picker_selected } = &mut app.screen else { return };
    let name = app_name.clone();
    let cfg = config.clone();
    let idx = *setting_idx;
    let sel = *picker_selected;
    app.screen = Screen::OptionPicker {
        app_name: name,
        config: cfg,
        setting_idx: idx,
        selected: sel,
    };
    app.needs_clear = true;
}

// ── Setting help popup ────────────────────────────────────────────────────────

fn on_setting_help(app: &mut App, _code: KeyCode) {
    let Screen::SettingHelp { app_name, config, back_selected } = &mut app.screen else { return };
    let name = app_name.clone();
    let cfg = config.clone();
    let sel = *back_selected;
    if name.is_empty() {
        // Global settings mode
        app.global_config = cfg;
        app.global_selected = sel;
        app.tab = Tab::Settings;
        app.screen = Screen::Main;
    } else {
        app.screen = Screen::Config { app_name: name, config: cfg, selected: sel };
    }
    app.needs_clear = true;
}

// ── Text input overlay (spoof settings) ──────────────────────────────────────

fn set_spoof_field(config: &mut AppConfig, idx: usize, value: String) {
    // RAM limit is numeric-with-units, not a free string.
    if idx == CFG_RAM_LIMIT {
        config.ram_limit = crate::config::parse_ram_limit(&value);
        return;
    }
    // Uptime is a duration (e.g. "3d4h") or bare seconds, not a free string.
    if idx == CFG_SPOOF_UPTIME {
        config.spoof_uptime = crate::config::parse_uptime(&value);
        return;
    }
    let v = if value.is_empty() { None } else { Some(value) };
    match idx {
        CFG_SPOOF_HOSTNAME    => config.spoof_hostname    = v,
        CFG_SPOOF_USERNAME    => config.spoof_username    = v,
        CFG_SPOOF_MACHINE_ID  => config.spoof_machine_id  = v,
        CFG_SPOOF_CPUINFO     => config.spoof_cpuinfo     = v,
        CFG_SPOOF_OS          => config.spoof_os          = v,
        _ => {}
    }
}

/// Byte offset of the `char_idx`-th character in `s` (or `s.len()` at the end).
fn byte_of(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map(|(b, _)| b).unwrap_or(s.len())
}

/// Apply a cursor/editing key to a text field held as (`value`, `cursor`), where
/// `cursor` is a character index into `value`. Returns true if `code` was an
/// editing key (Left/Right/Home/End/Backspace/Delete or an accepted Char) so the
/// caller can stop; false for other keys (Enter/Esc/…). `accept` filters typed
/// characters; `max_len` caps the length in characters.
fn edit_input(
    value: &mut String,
    cursor: &mut usize,
    code: KeyCode,
    max_len: usize,
    accept: impl Fn(char) -> bool,
) -> bool {
    let count = value.chars().count();
    if *cursor > count { *cursor = count; }
    match code {
        KeyCode::Left => { *cursor = cursor.saturating_sub(1); true }
        KeyCode::Right => { if *cursor < count { *cursor += 1; } true }
        KeyCode::Home => { *cursor = 0; true }
        KeyCode::End => { *cursor = count; true }
        KeyCode::Backspace => {
            if *cursor > 0 {
                let b = byte_of(value, *cursor - 1);
                value.remove(b);
                *cursor -= 1;
            }
            true
        }
        KeyCode::Delete => {
            if *cursor < count {
                let b = byte_of(value, *cursor);
                value.remove(b);
            }
            true
        }
        KeyCode::Char(c) if accept(c) && count < max_len => {
            let b = byte_of(value, *cursor);
            value.insert(b, c);
            *cursor += 1;
            true
        }
        _ => false,
    }
}

fn on_text_input(app: &mut App, code: KeyCode) {
    // Cursor/editing keys mutate value + the shared cursor together.
    {
        let App { screen, input_cursor, .. } = app;
        if let Screen::TextInput { value, .. } = screen {
            if edit_input(value, input_cursor, code, 4096, |_| true) { return; }
        }
    }
    let Screen::TextInput { app_name, config, back_selected, field_idx, value } = &mut app.screen
    else {
        return;
    };
    match code {
        KeyCode::Esc => {
            let name = app_name.clone();
            let cfg = config.clone();
            let sel = *back_selected;
            if name.is_empty() {
                app.global_config = cfg;
                app.global_selected = sel;
                app.tab = Tab::Settings;
                app.screen = Screen::Main;
            } else {
                app.screen = Screen::Config { app_name: name, config: cfg, selected: sel };
            }
            app.needs_clear = true;
        }
        KeyCode::Enter => {
            let name = app_name.clone();
            let mut cfg = config.clone();
            let sel = *back_selected;
            let idx = *field_idx;
            let v = value.trim().to_string();
            // Game-field inputs mutate the in-memory editing_wine_game tuple;
            // spoof-field inputs mutate AppConfig as before.
            let is_game_field = matches!(idx, CFG_GAME_EXE | CFG_GAME_PREFIX);
            if is_game_field {
                if let Some((exe, prefix)) = app.editing_wine_game.as_mut() {
                    if idx == CFG_GAME_EXE {
                        *exe = v;
                    } else {
                        *prefix = v;
                    }
                }
            } else {
                set_spoof_field(&mut cfg, idx, v);
            }
            if name.is_empty() {
                // Global settings mode
                app.global_config = cfg;
                app.global_selected = sel;
                app.tab = Tab::Settings;
                app.screen = Screen::Main;
            } else {
                app.screen = Screen::Config { app_name: name, config: cfg, selected: sel };
            }
            app.needs_clear = true;
        }
        _ => {}
    }
}

// ── Space tab ─────────────────────────────────────────────────────────────────

fn on_space_tab(app: &mut App, code: KeyCode) {
    if code == KeyCode::Char('r') {
        launch_op(app, "Dedup".to_string(), vec!["dedup".to_string()], None, true);
    }
}

// ── Settings tab (global defaults) ───────────────────────────────────────────

/// Master-store rows are actions, not values — they must never be cycled.
fn is_master_action(idx: usize) -> bool {
    matches!(idx, CFG_MASTER_PASSWORD | CFG_MASTER_SHOW | CFG_MASTER_FORGET)
}

/// Run the action for a master-store row. Returns whether it handled `idx`.
fn handle_master_action(app: &mut App, idx: usize) -> bool {
    match idx {
        CFG_MASTER_PASSWORD => open_master_password(app),
        CFG_MASTER_SHOW => open_reveal_passwords(app),
        CFG_MASTER_FORGET => {
            app.status = match crate::secrets::lock() {
                Ok(()) => "Master password forgotten — it will be asked for again.".into(),
                Err(e) => format!("error: {e:#}"),
            };
        }
        _ => return false,
    }
    true
}

fn on_settings_tab(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.global_selected =
                config_nav_step(true, false, EncryptionRows::Hidden, CFG_SAVE, app.global_selected, -1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.global_selected =
                config_nav_step(true, false, EncryptionRows::Hidden, CFG_SAVE, app.global_selected, 1);
        }
        KeyCode::Right | KeyCode::Char(' ') => {
            if app.global_selected == CFG_SAVE {
                let cfg = app.global_config.clone();
                if write_global_config(&cfg).is_ok() {
                    app.status = "Global defaults saved.".into();
                } else {
                    app.status = "error: failed to save global defaults".into();
                }
                return;
            }
            if app.global_selected == CFG_SHARES {
                open_shared_dirs_global(app);
                return;
            }
            if handle_master_action(app, app.global_selected) {
                return;
            }
            cycle_setting(&mut app.global_config, app.global_selected, 1);
        }
        KeyCode::Left
            if app.global_selected != CFG_SAVE
                && app.global_selected != CFG_SHARES
                && !is_master_action(app.global_selected) => {
                cycle_setting(&mut app.global_config, app.global_selected, -1);
            }
        KeyCode::Enter => {
            if app.global_selected == CFG_SAVE {
                let cfg = app.global_config.clone();
                if write_global_config(&cfg).is_ok() {
                    app.status = "Global defaults saved.".into();
                } else {
                    app.status = "error: failed to save global defaults".into();
                }
                return;
            }
            if app.global_selected == CFG_SHARES {
                open_shared_dirs_global(app);
                return;
            }
            if handle_master_action(app, app.global_selected) {
                return;
            }
            let idx = app.global_selected;
            let cur = setting_current(&app.global_config, idx);
            // Use empty app_name as sentinel for global settings mode
            app.screen = Screen::OptionPicker {
                app_name: String::new(),
                config: app.global_config.clone(),
                setting_idx: idx,
                selected: cur,
            };
            app.needs_clear = true;
        }
        KeyCode::Char('?') => {
            let idx = app.global_selected;
            app.screen = Screen::SettingHelp {
                app_name: String::new(),
                config: app.global_config.clone(),
                back_selected: idx,
            };
            app.needs_clear = true;
        }
        _ => {}
    }
}

// ── Shared dirs screen ────────────────────────────────────────────────────────

/// Open the SharedDirs editor for the global defaults (empty app_name
/// signals global mode throughout the SharedDirs / file-browser flow).
fn open_shared_dirs_global(app: &mut App) {
    let dirs = app.global_config.shared_dirs.clone();
    let sel = dirs.len().saturating_sub(1);
    app.screen = Screen::SharedDirs { app_name: String::new(), dirs, selected: sel };
    app.needs_clear = true;
}

/// Read shared-dirs config from either the per-app file or the global defaults
/// file, depending on whether app_name is set.
fn read_shared_cfg(app_name: &str) -> AppConfig {
    if app_name.is_empty() {
        read_global_config()
    } else {
        read_config(app_name).unwrap_or_default()
    }
}

/// Persist a config back to either the per-app file or defaults.ini.
fn write_shared_cfg(app_name: &str, cfg: &AppConfig) {
    if app_name.is_empty() {
        let _ = write_global_config(cfg);
    } else {
        let _ = write_config(app_name, cfg);
    }
}

fn on_shared_dirs(app: &mut App, code: KeyCode) {
    let Screen::SharedDirs { app_name, dirs, selected } = &mut app.screen else { return };
    if list_nav(selected, dirs.len(), code) {
        return;
    }
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            let name = app_name.clone();
            if name.is_empty() {
                // Returning from the global-defaults editor: refresh the
                // in-memory global_config from disk and pop back to the
                // Settings tab instead of the per-app Config popup.
                app.global_config = read_global_config();
                app.global_selected = CFG_SHARES;
                app.tab = Tab::Settings;
                app.screen = Screen::Main;
            } else {
                let config = read_config(&name).unwrap_or_default();
                app.screen = Screen::Config { app_name: name, config, selected: CFG_SHARES };
            }
            app.needs_clear = true;
        }
        KeyCode::Char('d') | KeyCode::Delete
            if !dirs.is_empty() => {
                let name = app_name.clone();
                let idx = *selected;
                dirs.remove(idx);
                if !dirs.is_empty() && idx >= dirs.len() {
                    *selected = dirs.len() - 1;
                }
                let mut config = read_shared_cfg(&name);
                config.shared_dirs = dirs.clone();
                write_shared_cfg(&name, &config);
            }
        KeyCode::Char('a') => {
            let name = app_name.clone();
            open_file_browser(app, BrowserMode::PickShareDir(name));
        }
        _ => {}
    }
}

/// Open the bound-apps multi-select for `app_name`: every other installed app
/// (root apps and aliases that can be launched), pre-ticked from the current
/// config.bound_apps.
fn open_bound_apps(app: &mut App, app_name: String) {
    let cfg = read_shared_cfg(&app_name);
    let chosen = cfg.bound_apps;
    let mut apps: Vec<(String, bool)> = crate::manifest::list_all_apps()
        .unwrap_or_default()
        .into_iter()
        .map(|m| m.app.name)
        .filter(|n| *n != app_name)
        .map(|n| {
            let on = chosen.contains(&n);
            (n, on)
        })
        .collect();
    apps.sort_by(|a, b| a.0.cmp(&b.0));
    app.screen = Screen::BoundApps { app_name, apps, selected: 0 };
    app.needs_clear = true;
}

fn on_bound_apps(app: &mut App, code: KeyCode) {
    let Screen::BoundApps { app_name, apps, selected } = &mut app.screen else { return };
    if list_nav(selected, apps.len(), code) {
        return;
    }
    match code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
            // Persist the ticked set into config.bound_apps and return to Config.
            let name = app_name.clone();
            let chosen: Vec<String> = apps.iter()
                .filter(|(_, on)| *on)
                .map(|(n, _)| n.clone())
                .collect();
            let mut cfg = read_shared_cfg(&name);
            cfg.bound_apps = chosen;
            write_shared_cfg(&name, &cfg);
            let config = read_config(&name).unwrap_or_default();
            app.screen = Screen::Config { app_name: name, config, selected: CFG_BOUND };
            app.needs_clear = true;
        }
        KeyCode::Char(' ') if !apps.is_empty() => {
            if let Some(entry) = apps.get_mut(*selected) {
                entry.1 = !entry.1;
            }
        }
        _ => {}
    }
}

// ── File browser ──────────────────────────────────────────────────────────────

fn open_file_browser(app: &mut App, mode: BrowserMode) {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    let dir = PathBuf::from(home);
    let entries = load_dir_entries(&dir);
    let mut fb_state = ListState::default();
    if !entries.is_empty() {
        fb_state.select(Some(0));
    }
    app.screen = Screen::FileBrowser { current_dir: dir, entries, fb_state, mode };
}

fn load_dir_entries(dir: &PathBuf) -> Vec<FbEntry> {
    let mut entries = vec![];
    let Ok(rd) = std::fs::read_dir(dir) else { return entries };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') { continue; }
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let is_zip = name.ends_with(".zip");
        entries.push(FbEntry { name, is_dir, is_zip });
    }
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    entries
}

enum FbAction {
    Nothing,
    EnterDir(PathBuf),
    SelectFile(String),
    SelectShareDir { path: PathBuf, app_name: String },
    SelectGameDir(PathBuf),
    GoUp,
    Close(BrowserMode),
}

fn on_file_browser(app: &mut App, code: KeyCode) {
    let action = {
        let Screen::FileBrowser { current_dir, entries, fb_state, mode } = &mut app.screen else { return };
        let mode_clone = mode.clone();
        let pick_dir = !matches!(mode_clone, BrowserMode::ImportZip);
        match code {
            KeyCode::Esc | KeyCode::Char('q') => FbAction::Close(mode_clone),
            // Space / s selects the current directory in pick-dir modes
            KeyCode::Char(' ') | KeyCode::Char('s') if pick_dir => {
                match mode_clone {
                    BrowserMode::PickShareDir(app_name) => {
                        FbAction::SelectShareDir { path: current_dir.clone(), app_name }
                    }
                    BrowserMode::PickGameDir => {
                        FbAction::SelectGameDir(current_dir.clone())
                    }
                    BrowserMode::ImportZip => FbAction::Nothing,
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let i = fb_state.selected().unwrap_or(0);
                fb_state.select(Some(i.saturating_sub(1)));
                FbAction::Nothing
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let i = fb_state.selected().unwrap_or(0);
                fb_state.select(Some((i + 1).min(entries.len().saturating_sub(1))));
                FbAction::Nothing
            }
            KeyCode::Left | KeyCode::Backspace => {
                if current_dir.parent().is_some() { FbAction::GoUp } else { FbAction::Nothing }
            }
            KeyCode::Enter | KeyCode::Right => {
                let i = fb_state.selected().unwrap_or(0);
                if let Some(entry) = entries.get(i) {
                    if entry.is_dir {
                        FbAction::EnterDir(current_dir.join(&entry.name))
                    } else if entry.is_zip && matches!(mode_clone, BrowserMode::ImportZip) {
                        FbAction::SelectFile(
                            current_dir.join(&entry.name).to_string_lossy().into_owned(),
                        )
                    } else {
                        FbAction::Nothing
                    }
                } else {
                    FbAction::Nothing
                }
            }
            _ => FbAction::Nothing,
        }
    };

    match action {
        FbAction::Nothing => {}
        FbAction::Close(BrowserMode::ImportZip) => {
            app.screen = Screen::Main;
            app.tab = Tab::Import;
            app.needs_clear = true;
        }
        FbAction::Close(BrowserMode::PickGameDir) => {
            app.screen = Screen::Main;
            app.tab = Tab::Games;
            app.needs_clear = true;
        }
        FbAction::Close(BrowserMode::PickShareDir(app_name)) => {
            let config = read_shared_cfg(&app_name);
            let dirs = config.shared_dirs;
            let selected = dirs.len().saturating_sub(1);
            app.screen = Screen::SharedDirs { app_name, dirs, selected };
            app.needs_clear = true;
        }
        FbAction::GoUp => {
            if let Screen::FileBrowser { current_dir, entries, fb_state, .. } = &mut app.screen {
                if let Some(parent) = current_dir.parent().map(|p| p.to_path_buf()) {
                    *entries = load_dir_entries(&parent);
                    *current_dir = parent;
                    fb_state.select(if entries.is_empty() { None } else { Some(0) });
                }
            }
        }
        FbAction::EnterDir(new_dir) => {
            if let Screen::FileBrowser { current_dir, entries, fb_state, .. } = &mut app.screen {
                let new_entries = load_dir_entries(&new_dir);
                *current_dir = new_dir;
                *entries = new_entries;
                fb_state.select(if entries.is_empty() { None } else { Some(0) });
            }
        }
        FbAction::SelectFile(path) => {
            app.import_input = path;
            app.screen = Screen::Main;
            app.tab = Tab::Import;
        }
        FbAction::SelectShareDir { path, app_name } => {
            let path_str = path.to_string_lossy().into_owned();
            let mut config = read_shared_cfg(&app_name);
            if !config.shared_dirs.contains(&path_str) {
                config.shared_dirs.push(path_str);
                write_shared_cfg(&app_name, &config);
            }
            let dirs = config.shared_dirs;
            let selected = dirs.len().saturating_sub(1);
            app.screen = Screen::SharedDirs { app_name, dirs, selected };
            app.needs_clear = true;
        }
        FbAction::SelectGameDir(path) => {
            enter_game_wizard(app, path);
        }
    }
}

// ── Install queue + background ops ────────────────────────────────────────────

/// Pop the next package from the install queue and show InstallTarget or Confirm.
/// Build the install queue from all marked packages and kick it off.
/// Marks in display order come first, then any marked packages not currently
/// in search_results (sorted). Clears selected_pkgs before returning.
fn enqueue_marked(app: &mut App) {
    // Preserve visible display order for marks that appear in current results
    let mut pkgs: Vec<String> = app.search_results.iter()
        .filter(|(name, _)| app.selected_pkgs.contains(name.as_str()))
        .map(|(name, _)| name.clone())
        .collect();
    // Append marks that are outside the current search results (sorted)
    let mut hidden: Vec<String> = app.selected_pkgs.iter()
        .filter(|p| !pkgs.contains(*p))
        .cloned()
        .collect();
    hidden.sort();
    pkgs.extend(hidden);
    app.selected_pkgs.clear();
    app.install_queue = pkgs.into_iter().collect();
    process_install_queue(app);
}

/// Already-installed packages are skipped with a status note.
fn process_install_queue(app: &mut App) {
    loop {
        let Some(pkg) = app.install_queue.pop_front() else { return };
        if app.installed.iter().any(|m| m.app.name == pkg) {
            let note = format!("'{pkg}' already installed — skipped");
            app.status = if app.status.is_empty() { note } else { format!("{}; {}", app.status, note) };
            continue;
        }
        let targets: Vec<String> = app.installed
            .iter()
            .filter(|m| m.app.alias_of.is_none())
            .map(|m| m.app.name.clone())
            .collect();
        if targets.is_empty() {
            install_confirm(
                app,
                format!("Install '{pkg}'?"),
                vec![
                    format!("Installs {pkg} into ~/.wryayer/{pkg}/"),
                    String::new(),
                    "Press y to confirm, n or Esc to cancel.".into(),
                ],
                PendingAction::Install { pkg, app_name: None, into: None },
            );
        } else {
            app.screen = Screen::InstallTarget { pkg, targets, selected: 0 };
        }
        app.needs_clear = true;
        return;
    }
}

// ── Shortcut confirmation ─────────────────────────────────────────────────────

fn on_ask_shortcut(app: &mut App, code: KeyCode) {
    let Screen::AskShortcut { selected, .. } = &mut app.screen else { return };
    if list_nav(selected, 2, code) {
        return;
    }
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.install_queue.clear();
            app.screen = Screen::Main;
            app.needs_clear = true;
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let screen = std::mem::replace(&mut app.screen, Screen::Main);
            if let Screen::AskShortcut { pkg, title, mut args, selected } = screen {
                if selected == 1 {
                    args.push("--keep-without-launcher".into());
                }
                ask_encrypt(app, pkg, title, args);
            }
        }
        _ => {}
    }
}

// ── Container confirmation ────────────────────────────────────────────────────

fn on_ask_encrypt(app: &mut App, code: KeyCode) {
    let Screen::AskEncrypt { selected, kind, .. } = &mut app.screen else { return };
    let kind = *kind;
    if list_nav(selected, kind.choices().len(), code) {
        return;
    }
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            // Only an install has a queue behind it; a conversion is one app.
            if kind == EncryptAsk::Install {
                app.install_queue.clear();
            }
            app.screen = Screen::Main;
            app.needs_clear = true;
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let screen = std::mem::replace(&mut app.screen, Screen::Main);
            if let Screen::AskEncrypt { title, mut args, selected, .. } = screen {
                // Index 0 is the "don't" option in both tables. For an install
                // that still means running it, unencrypted; for a conversion
                // there is nothing left to do.
                if selected == 0 {
                    if kind == EncryptAsk::Install {
                        launch_op(app, title, args, None, true);
                    } else {
                        app.needs_clear = true;
                    }
                    return;
                }
                let extra = kind.choices()[selected].2;
                args.extend(extra.iter().map(|s| s.to_string()));
                let use_master =
                    extra.contains(&"--encrypt-master") || extra.contains(&"--master");
                let generate =
                    extra.contains(&"--encrypt-generate") || extra.contains(&"--generate");
                begin_encrypted_op(app, title, args, use_master, generate);
            }
        }
        _ => {}
    }
}

// ── Shared helpers ────────────────────────────────────────────────────────────

fn launch_op(app: &mut App, title: String, args: Vec<String>, total_bytes: Option<u64>, reload: bool) {
    launch_op_with_stdin(app, title, args, total_bytes, reload, None)
}

/// As [`launch_op`], but writes `stdin_data` to the child's stdin and closes it.
///
/// Used to hand collected passwords to an encrypted install without putting
/// them in argv (world-readable via /proc) or the environment (inherited by
/// veracrypt and every other child).
fn launch_op_with_stdin(
    app: &mut App,
    title: String,
    args: Vec<String>,
    total_bytes: Option<u64>,
    reload: bool,
    stdin_data: Option<String>,
) {
    let into_target = args.windows(2)
        .find(|w| w[0] == "--into")
        .map(|w| w[1].clone());
    let (tx, rx) = mpsc::channel();
    let original_args = args.clone();
    spawn_wryayer(args, tx, stdin_data);
    app.log_scroll = 0;
    app.screen = Screen::Operation {
        title,
        log: vec![],
        done: false,
        success: false,
        rx,
        total_bytes,
        progress: None,
        started: Instant::now(),
        reload,
        show_log: false,
        launcher_choice: None,
        into_target,
        outdated_pkg: None,
        original_args,
    };
}

/// Parse `PROGRESS <done>/<total>` lines emitted by subprocess commands.
pub fn parse_progress(line: &str) -> Option<(u64, u64)> {
    let rest = line.strip_prefix("PROGRESS ")?;
    let (a, b) = rest.split_once('/')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// The canonical konami code: ↑↑↓↓←→←→BA.
pub const KONAMI: &[KeyCode] = &[
    KeyCode::Up, KeyCode::Up,
    KeyCode::Down, KeyCode::Down,
    KeyCode::Left, KeyCode::Right,
    KeyCode::Left, KeyCode::Right,
    KeyCode::Char('b'), KeyCode::Char('a'),
];

/// Advance the konami detection FSM by one keystroke. Returns true iff the
/// code was just completed on this step (used by tests).
pub fn konami_advance(state: &mut usize, code: KeyCode) -> bool {
    let expected = KONAMI[*state];
    // Match — including case-insensitive char compare so SHIFT doesn't break it.
    let matches = match (expected, code) {
        (KeyCode::Char(a), KeyCode::Char(b)) => a.eq_ignore_ascii_case(&b),
        (a, b) => a == b,
    };
    if matches {
        *state += 1;
        if *state == KONAMI.len() {
            *state = 0;
            return true;
        }
    } else {
        // Allow the failed key to be the start of a new attempt
        *state = if code == KONAMI[0] { 1 } else { 0 };
    }
    false
}

fn konami_step(app: &mut App, code: KeyCode) {
    if konami_advance(&mut app.konami_state, code) {
        app.konami_mode = !app.konami_mode;
        app.status = konami_status_for_toggle(app.konami_mode);
    }
}

/// What to write into `app.status` after a konami toggle. The statusbar
/// already renders a dedicated `★ konami mode` chip from `app.konami_mode`,
/// so mirroring the same text into `app.status` would double-render. We only
/// emit a status string when toggling OFF, since the chip is gone by then.
pub fn konami_status_for_toggle(now_active: bool) -> String {
    if now_active {
        String::new()
    } else {
        "konami mode off".to_string()
    }
}

fn spawn_wryayer(args: Vec<String>, tx: mpsc::Sender<Msg>, stdin_data: Option<String>) {
    let exe = std::env::current_exe().unwrap_or_else(|_| "wryayer".into());
    thread::spawn(move || {
        let mut child = match Command::new(&exe)
            .args(&args)
            .stdin(if stdin_data.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Msg::Line(format!("error: {e}")));
                let _ = tx.send(Msg::Done(false));
                return;
            }
        };

        // Write the secrets and close stdin, so the child's reader sees EOF
        // and stops waiting for more.
        if let Some(data) = stdin_data {
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(data.as_bytes());
            }
        }

        let stderr = child.stderr.take().unwrap();
        let tx2 = tx.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = tx2.send(Msg::Line(crate::child_output::sanitize_line(&line)));
            }
        });

        if let Some(stdout) = child.stdout.take() {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(Msg::Line(crate::child_output::sanitize_line(&line)));
            }
        }

        let ok = child.wait().map(|s| s.success()).unwrap_or(false);
        let _ = tx.send(Msg::Done(ok));
    });
}

fn dir_bytes(path: &str) -> Option<u64> {
    let out = Command::new("du").args(["-sb", path]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.split_whitespace().next()?.parse().ok()
}

// ── Games tab ─────────────────────────────────────────────────────────────────

fn on_games(app: &mut App, code: KeyCode) {
    let games_count = app.games().len();
    // Make sure games_state has a sensible selection when the list is non-empty.
    if games_count > 0 && app.games_state.selected().is_none() {
        app.games_state.select(Some(0));
    } else if games_count == 0 {
        app.games_state.select(None);
    }

    match code {
        KeyCode::Char('i') | KeyCode::Char('a') => {
            open_file_browser(app, BrowserMode::PickGameDir);
        }
        KeyCode::Up | KeyCode::Char('k') if games_count > 0 => {
            let i = app.games_state.selected().unwrap_or(0);
            app.games_state.select(Some(if i == 0 { games_count - 1 } else { i - 1 }));
        }
        KeyCode::Down | KeyCode::Char('j') if games_count > 0 => {
            let i = app.games_state.selected().unwrap_or(0);
            app.games_state.select(Some((i + 1) % games_count));
        }
        KeyCode::Char('r') | KeyCode::Enter if games_count > 0 => {
            if let Some(m) = app.selected_game() {
                app.run_request = Some(m.app.name.clone());
            }
        }
        KeyCode::Enter if games_count == 0 => {
            // No games yet — Enter is a shortcut to start the import wizard.
            open_file_browser(app, BrowserMode::PickGameDir);
        }
        KeyCode::Char('s') if games_count > 0 => {
            if let Some(m) = app.selected_game() {
                let name = m.app.name.clone();
                let config = read_config(&name).unwrap_or_default();
                app.editing_wine_game = m.app.wine_game.as_ref()
                    .map(|w| (w.exe.clone(), w.prefix.clone()));
                app.screen = Screen::Config { app_name: name, config, selected: 0 };
                app.needs_clear = true;
            }
        }
        KeyCode::Char('d') | KeyCode::Delete if games_count > 0 => {
            if let Some(m) = app.selected_game() {
                let name = m.app.name.clone();
                app.screen = Screen::Confirm {
                    title: format!("Remove game '{name}'?"),
                    body: vec![
                        format!("Delete ~/.wryayer/{name}/ and its wine prefix?"),
                        String::new(),
                        "Press y to continue, n or Esc to cancel.".into(),
                    ],
                    action: PendingAction::Remove(name),
                    danger: true,
                };
                app.needs_clear = true;
            }
        }
        _ => {}
    }
}

/// Walk the picked folder for .exe files, return Vec<(relative_path, size_bytes)>.
fn scan_exes(root: &std::path::Path) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 6 { continue; }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if ft.is_dir() {
                if name_str.starts_with('.') || name_str.eq_ignore_ascii_case("drive_c") {
                    continue;
                }
                stack.push((entry.path(), depth + 1));
            } else if ft.is_file() && name_str.to_lowercase().ends_with(".exe") {
                let sz = entry.metadata().map(|m| m.len()).unwrap_or(0);
                let rel = entry.path().strip_prefix(root).unwrap_or(&entry.path())
                    .to_string_lossy().into_owned();
                out.push((rel, sz));
            }
        }
    }
    // Sort: prefer top-level, then by size desc
    out.sort_by(|a, b| {
        let da = a.0.matches('/').count();
        let db = b.0.matches('/').count();
        da.cmp(&db).then(b.1.cmp(&a.1))
    });
    out
}

fn sanitize_game_name(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn enter_game_wizard(app: &mut App, game_dir: PathBuf) {
    let exes = scan_exes(&game_dir);
    if exes.is_empty() {
        app.status = format!("No .exe files found under {}", game_dir.display());
        app.screen = Screen::Main;
        app.tab = Tab::Games;
        app.needs_clear = true;
        return;
    }
    app.screen = Screen::GameExePick { game_dir, exes, selected: 0 };
    app.needs_clear = true;
}

fn on_game_exe_pick(app: &mut App, code: KeyCode) {
    let Screen::GameExePick { game_dir, exes, selected } = &mut app.screen else { return };
    if list_nav(selected, exes.len(), code) {
        return;
    }
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.screen = Screen::Main;
            app.tab = Tab::Games;
            app.needs_clear = true;
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let gd = game_dir.clone();
            let exe = exes[*selected].0.clone();
            let default_name = sanitize_game_name(
                gd.file_name().and_then(|n| n.to_str()).unwrap_or("game"),
            );
            app.input_cursor = default_name.chars().count();
            app.screen = Screen::GameNameInput {
                game_dir: gd,
                exe,
                value: default_name,
            };
            app.needs_clear = true;
        }
        _ => {}
    }
}

fn on_game_name_input(app: &mut App, code: KeyCode) {
    {
        let App { screen, input_cursor, .. } = app;
        if let Screen::GameNameInput { value, .. } = screen {
            if edit_input(value, input_cursor, code, 256, |_| true) { return; }
        }
    }
    let Screen::GameNameInput { game_dir, exe, value } = &mut app.screen else { return };
    match code {
        KeyCode::Esc => {
            let gd = game_dir.clone();
            let exes = scan_exes(&gd);
            app.screen = Screen::GameExePick { game_dir: gd, exes, selected: 0 };
            app.needs_clear = true;
        }
        KeyCode::Enter => {
            let name = value.trim().to_string();
            if name.is_empty() { return; }
            if app.installed.iter().any(|m| m.app.name == name) {
                app.status = format!("'{name}' is already taken — pick a different name");
                return;
            }
            let gd = game_dir.clone();
            let exe = exe.clone();
            app.screen = Screen::GameConfirm {
                game_dir: gd,
                exe,
                app_name: name,
                delete_source: false,
                selected: 0,
            };
            app.needs_clear = true;
        }
        _ => {}
    }
}

fn on_game_confirm(app: &mut App, code: KeyCode) {
    let Screen::GameConfirm { game_dir, exe, app_name, delete_source, selected } = &mut app.screen else { return };
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.screen = Screen::Main;
            app.tab = Tab::Games;
            app.needs_clear = true;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            *selected = if *selected == 0 { 2 } else { *selected - 1 };
        }
        KeyCode::Down | KeyCode::Char('j') => {
            *selected = (*selected + 1) % 3;
        }
        KeyCode::Char(' ') if *selected == 1 => {
            *delete_source = !*delete_source;
        }
        KeyCode::Enter => {
            let s = *selected;
            let gd = game_dir.clone();
            let exe = exe.clone();
            let app_name = app_name.clone();
            let delete = *delete_source;
            match s {
                0 => {
                    let mut args = vec![
                        "install-game".to_string(),
                        gd.to_string_lossy().into_owned(),
                        "--exe".to_string(), exe,
                        "--app-name".to_string(), app_name.clone(),
                    ];
                    if delete {
                        args.push("--delete-source".into());
                    }
                    let total = dir_bytes(&gd.to_string_lossy());
                    launch_op(app, format!("Import game — {app_name}"), args, total, true);
                }
                1 => { *delete_source = !*delete_source; }
                _ => {
                    app.screen = Screen::Main;
                    app.tab = Tab::Games;
                    app.needs_clear = true;
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod op_log_tests {
    use super::*;

    /// Build an Operation screen with `n` log lines, plus a live channel so the
    /// receiver isn't dropped.
    fn op_screen(n: usize, done: bool) -> (Screen, mpsc::Sender<Msg>) {
        let (tx, rx) = mpsc::channel();
        let screen = Screen::Operation {
            title: "Install — bash".into(),
            log: (0..n).map(|i| format!("line {i}")).collect(),
            done,
            success: false,
            rx,
            total_bytes: None,
            progress: None,
            started: Instant::now(),
            reload: true,
            show_log: false,
            launcher_choice: None,
            into_target: None,
            outdated_pkg: None,
            original_args: vec![],
        };
        (screen, tx)
    }

    fn show_log_of(app: &App) -> bool {
        match &app.screen {
            Screen::Operation { show_log, .. } => *show_log,
            _ => panic!("not an Operation screen"),
        }
    }

    #[test]
    fn merge_target_is_read_from_the_into_flag() {
        let args: Vec<String> = ["install", "vim", "--into", "toolbox"]
            .iter().map(|s| s.to_string()).collect();
        // No manifest for "toolbox" in the test environment, so it resolves to
        // itself rather than following an alias.
        assert_eq!(merge_target_root(&args).as_deref(), Some("toolbox"));
    }

    #[test]
    fn a_fresh_install_has_no_merge_target() {
        let args: Vec<String> = ["install", "vim"].iter().map(|s| s.to_string()).collect();
        assert_eq!(merge_target_root(&args), None);
    }

    #[test]
    fn t_toggles_the_log_while_an_operation_runs() {
        let _home = crate::test_support::test_home();
        let mut app = App::new().unwrap();
        let (screen, _tx) = op_screen(500, false);
        app.screen = screen;

        assert!(!show_log_of(&app), "log starts hidden");
        handle_key(&mut app, KeyCode::Char('t')).unwrap();
        assert!(show_log_of(&app), "'t' should open the log");
        handle_key(&mut app, KeyCode::Char('t')).unwrap();
        assert!(!show_log_of(&app), "'t' should close it again");
    }

    #[test]
    fn t_toggles_the_log_after_an_operation_finishes() {
        let _home = crate::test_support::test_home();
        let mut app = App::new().unwrap();
        let (screen, _tx) = op_screen(500, true);
        app.screen = screen;

        handle_key(&mut app, KeyCode::Char('t')).unwrap();
        assert!(show_log_of(&app), "'t' should open the log when done");
    }

    #[test]
    fn a_container_being_created_is_not_read_as_finished() {
        use crate::tui::ui::is_progress_line;
        // "Done: 0.000%" arrives the instant a container creation starts, and
        // the plain "contains Done" rule painted it with the success colour.
        assert!(is_progress_line("Done:   0.000%  Speed:            Left:"));
        assert!(is_progress_line("Done: 100.000%  Speed: 1.7 MiB/s  Left: 0 s"));
    }

    #[test]
    fn real_outcomes_are_not_mistaken_for_progress() {
        use crate::tui::ui::is_progress_line;
        for line in ["Done", "Update complete", "Updated firefox", "Saved", "Done installing"] {
            assert!(!is_progress_line(line), "{line:?} is an outcome, not progress");
        }
    }

    #[test]
    fn a_plain_app_is_offered_encryption() {
        let rows = config_nav_order(false, false, EncryptionRows::Offer);
        assert!(rows.contains(&CFG_ENCRYPT_APP));
        assert!(!rows.contains(&CFG_DECRYPT_APP), "nothing to decrypt yet");
        assert!(!rows.contains(&CFG_PASSWORD_SOURCE), "no container to configure");
    }

    #[test]
    fn an_encrypted_app_is_offered_its_settings_and_the_way_out() {
        let rows = config_nav_order(false, false, EncryptionRows::Manage);
        assert!(rows.contains(&CFG_PASSWORD_SOURCE));
        assert!(rows.contains(&CFG_LOCK_ON_EXIT));
        assert!(rows.contains(&CFG_DECRYPT_APP));
        assert!(!rows.contains(&CFG_ENCRYPT_APP), "already encrypted");
    }

    #[test]
    fn an_app_that_cannot_be_encrypted_gets_no_section() {
        let sections = config_sections(false, false, EncryptionRows::Hidden);
        assert!(
            !sections.iter().any(|(title, _)| *title == "Encryption"),
            "an empty Encryption header would be worse than none"
        );
    }

    #[test]
    fn an_alias_is_told_why_rather_than_shown_nothing() {
        // It owns no tree, so it cannot be encrypted — but an absent section
        // reads as a missing feature, and an alias is exactly the kind of small
        // merged-in tool people try encryption on first.
        assert_eq!(EncryptionRows::for_app("no-such-app", true), EncryptionRows::Alias);
        assert_eq!(EncryptionRows::Alias.rows(), vec![CFG_ENCRYPT_ALIAS]);
        assert!(!EncryptionRows::Alias.rows().contains(&CFG_ENCRYPT_APP));
    }

    #[test]
    fn the_alias_row_points_at_the_app_that_can_be_encrypted() {
        let help = setting_description(CFG_ENCRYPT_ALIAS);
        assert!(help.contains("wryayer encrypt"), "{help}");
        assert!(help.contains("alias"), "{help}");
    }

    #[test]
    fn every_encryption_layout_reaches_save() {
        // Each variant adds a different number of rows; a stale Save index
        // would make Enter on Save cycle a setting instead.
        for rows in [
            EncryptionRows::Hidden,
            EncryptionRows::Offer,
            EncryptionRows::Manage,
            EncryptionRows::Alias,
        ] {
            let save = app_cfg_save_idx(false, rows);
            let mut at = 0;
            let mut seen_save = false;
            for _ in 0..save + 2 {
                at = config_nav_step(false, false, rows, save, at, 1);
                seen_save |= at == save;
            }
            assert!(seen_save, "Save unreachable for {rows:?}");
        }
    }

    #[test]
    fn the_save_button_moves_down_as_encryption_rows_appear() {
        // The Save index is derived from the row count; a stale one would make
        // Enter on Save cycle a setting instead.
        let hidden = app_cfg_save_idx(false, EncryptionRows::Hidden);
        assert_eq!(app_cfg_save_idx(false, EncryptionRows::Offer), hidden + 1);
        assert_eq!(app_cfg_save_idx(false, EncryptionRows::Manage), hidden + 3);
    }

    #[test]
    fn navigation_reaches_every_encryption_row_and_then_save() {
        // Rows the renderer draws but ↑/↓ cannot reach are invisible in
        // practice — this is how the Encryption section shipped empty once.
        let save = app_cfg_save_idx(false, EncryptionRows::Manage);
        let mut seen = vec![];
        let mut at = 0;
        for _ in 0..save + 2 {
            seen.push(at);
            at = config_nav_step(false, false, EncryptionRows::Manage, save, at, 1);
        }
        for row in [CFG_PASSWORD_SOURCE, CFG_LOCK_ON_EXIT, CFG_DECRYPT_APP, save] {
            assert!(seen.contains(&row), "row {row} unreachable: {seen:?}");
        }
    }

    #[test]
    fn converting_an_app_offers_a_way_to_back_out_first() {
        // Index 0 is the "don't" option in both tables, which is what
        // on_ask_encrypt keys its cancel path on.
        assert!(CONVERT_CHOICES[0].2.is_empty());
        assert_eq!(ENCRYPT_CHOICES[0].2.len(), CONVERT_CHOICES[0].2.len());
    }

    #[test]
    fn convert_choices_pass_encrypt_flags_not_install_flags() {
        // `wryayer encrypt` spells them --master/--generate; the install
        // command spells them --encrypt-master/--encrypt-generate. Sending the
        // wrong pair silently produces a prompt-source container.
        let flags: Vec<&str> = CONVERT_CHOICES.iter().flat_map(|c| c.2.iter().copied()).collect();
        assert!(flags.contains(&"--master"), "{flags:?}");
        assert!(flags.contains(&"--generate"), "{flags:?}");
        assert!(!flags.iter().any(|f| f.starts_with("--encrypt")), "{flags:?}");
    }

    #[test]
    fn a_locked_container_shows_a_closed_padlock() {
        assert_eq!(
            crate::tui::ui::encryption_glyphs(EncState { locked: true, master: false, fill: None }),
            ("🔒", None)
        );
    }

    #[test]
    fn an_open_container_shows_an_open_padlock() {
        assert_eq!(
            crate::tui::ui::encryption_glyphs(EncState { locked: false, master: false, fill: None }),
            ("🔓", None)
        );
    }

    #[test]
    fn a_master_backed_container_is_marked_whatever_its_lock_state() {
        // The key answers "will the next launch stop to ask me for a password",
        // which is true whether the container happens to be open right now.
        for locked in [true, false] {
            let (_, key) = crate::tui::ui::encryption_glyphs(EncState { locked, master: true, fill: None });
            assert_eq!(key, Some("🔑"), "locked = {locked}");
        }
    }

    /// A minimal installed app the list and detail pane can render.
    fn stub_manifest(name: &str) -> Manifest {
        Manifest {
            app: crate::manifest::AppMeta {
                name: name.to_string(),
                main_binary: name.to_string(),
                installed_at: "2026-01-01T00:00:00Z".into(),
                launchers: vec![name.to_string()],
                alias_of: None,
                display_name: None,
                pkg_name: None,
                wine_game: None,
            },
            packages: vec![],
        }
    }

    /// Render the app list with one encrypted app in the given state.
    fn render_with_encrypted(state: EncState) -> String {
        let _home = crate::test_support::test_home();
        let mut app = App::new().unwrap();
        app.screen = Screen::Main;
        app.tab = Tab::Installed;
        app.installed = vec![stub_manifest("vault")];
        app.inst_state.select(Some(0));
        app.encrypted_apps = HashMap::from([("vault".to_string(), state)]);
        render(&mut app, 110, 30)
    }

    #[test]
    fn the_details_pane_spells_out_a_prompting_container() {
        let out = render_with_encrypted(EncState { locked: true, master: false, fill: None });
        assert!(out.contains("Encrypted:"), "no encryption line:\n{out}");
        assert!(out.contains("locked"), "lock state missing:\n{out}");
        assert!(out.contains("asks for a password"), "source missing:\n{out}");
    }

    #[test]
    fn the_details_pane_spells_out_a_master_backed_container() {
        let out = render_with_encrypted(EncState { locked: false, master: true, fill: None });
        assert!(out.contains("unlocked"), "lock state missing:\n{out}");
        assert!(out.contains("master store"), "source missing:\n{out}");
    }

    #[test]
    fn an_open_container_shows_how_full_it_is() {
        let out = render_with_encrypted(EncState {
            locked: false,
            master: false,
            fill: Some(crate::veracrypt::Usage {
                used: 512 * 1024 * 1024,
                available: 512 * 1024 * 1024,
                total: 1024 * 1024 * 1024,
            }),
        });
        assert!(out.contains("Container:"), "no fill line:\n{out}");
        assert!(out.contains("50%"), "fill percentage missing:\n{out}");
    }

    #[test]
    fn a_nearly_full_container_says_what_to_do_about_it() {
        let out = render_with_encrypted(EncState {
            locked: false,
            master: false,
            fill: Some(crate::veracrypt::Usage {
                used: 990 * 1024 * 1024,
                available: 10 * 1024 * 1024,
                total: 1024 * 1024 * 1024,
            }),
        });
        assert!(out.contains("nearly full"), "no warning:\n{out}");
        assert!(out.contains("wryayer grow vault"), "no remedy named:\n{out}");
    }

    #[test]
    fn a_locked_container_shows_no_fill_at_all() {
        // Reading it would mean statvfs on an unmounted mount point, which
        // describes the host filesystem — a plausible, wrong number.
        let out = render_with_encrypted(EncState { locked: true, master: false, fill: None });
        assert!(!out.contains("Container:"), "fill shown for a locked container:\n{out}");
    }

    #[test]
    fn a_plain_app_has_no_encryption_line() {
        let _home = crate::test_support::test_home();
        let mut app = App::new().unwrap();
        app.screen = Screen::Main;
        app.tab = Tab::Installed;
        app.installed = vec![stub_manifest("plain")];
        app.inst_state.select(Some(0));
        app.encrypted_apps = HashMap::new();
        let out = render(&mut app, 110, 30);
        assert!(!out.contains("Encrypted:"), "encryption line shown for a plain app:\n{out}");
    }

    /// The config popup for `name`, with the encryption rows forced on.
    ///
    /// Returns the sandbox alongside the `App`: the test keeps driving it with
    /// keystrokes, and every one of those resolves paths from HOME again.
    fn config_screen(name: &str, selected: usize) -> (crate::test_support::TestHome, App) {
        let home = crate::test_support::test_home();
        let mut app = App::new().unwrap();
        app.installed = vec![stub_manifest(name)];
        app.inst_state.select(Some(0));
        app.screen = Screen::Config {
            app_name: name.to_string(),
            config: crate::config::AppConfig::default(),
            selected,
        };
        (home, app)
    }

    #[test]
    fn enter_on_the_encrypt_row_opens_the_password_source_choice() {
        let (_home, mut app) = config_screen("plainapp", CFG_ENCRYPT_APP);
        handle_key(&mut app, KeyCode::Enter).unwrap();

        let Screen::AskEncrypt { kind, args, selected, .. } = &app.screen else {
            panic!("expected the encrypt choice screen");
        };
        assert_eq!(*kind, EncryptAsk::Convert);
        assert_eq!(args, &["encrypt".to_string(), "plainapp".to_string()]);
        assert_ne!(*selected, 0, "the cursor should not start on the back-out row");
    }

    #[test]
    fn backing_out_of_the_encrypt_choice_runs_nothing() {
        let (_home, mut app) = config_screen("plainapp", CFG_ENCRYPT_APP);
        handle_key(&mut app, KeyCode::Enter).unwrap();
        // Move to the "leave it as it is" row and take it.
        while !matches!(&app.screen, Screen::AskEncrypt { selected: 0, .. }) {
            handle_key(&mut app, KeyCode::Up).unwrap();
        }
        handle_key(&mut app, KeyCode::Enter).unwrap();

        assert!(
            matches!(app.screen, Screen::Main),
            "backing out must not start an operation"
        );
    }

    #[test]
    fn enter_on_the_decrypt_row_asks_for_confirmation_first() {
        let (_home, mut app) = config_screen("vaultapp", CFG_DECRYPT_APP);
        handle_key(&mut app, KeyCode::Enter).unwrap();

        let Screen::Confirm { action, .. } = &app.screen else {
            panic!("expected a confirmation, got straight to work");
        };
        assert!(matches!(action, PendingAction::ConfirmedDecrypt(n) if n == "vaultapp"));
    }

    #[test]
    fn left_on_an_action_row_does_nothing() {
        // Left cycles a setting's value. On an action row there is no value —
        // and starting a multi-gigabyte copy from a stray arrow key would be a
        // nasty surprise.
        let (_home, mut app) = config_screen("plainapp", CFG_ENCRYPT_APP);
        handle_key(&mut app, KeyCode::Left).unwrap();
        assert!(matches!(app.screen, Screen::Config { .. }), "Left should stay put");
    }

    #[test]
    fn the_encrypt_choice_screen_renders_its_options() {
        let (_home, mut app) = config_screen("plainapp", CFG_ENCRYPT_APP);
        handle_key(&mut app, KeyCode::Enter).unwrap();
        let out = render(&mut app, 100, 30);
        assert!(out.contains("plainapp"), "{out}");
        assert!(out.contains("master store"), "master option missing:\n{out}");
        assert!(out.contains("Leave it as it is"), "no way back out:\n{out}");
    }

    #[test]
    fn the_key_help_explains_the_list_markers() {
        // The padlocks are guessable, the key beside them is not — so it has to
        // be written down somewhere the user can reach without leaving the TUI.
        let _home = crate::test_support::test_home();
        let mut app = App::new().unwrap();
        app.screen = Screen::KeyHelp;
        let out = render(&mut app, 100, 40);
        assert!(out.contains("🔑"), "the key marker is missing from ? help:\n{out}");
        assert!(out.contains("password stored"), "the key is unexplained:\n{out}");
        assert!(out.contains("🔒"), "the padlock is missing:\n{out}");
    }

    /// Render the whole TUI to an off-screen buffer and return it as text.
    fn render(app: &mut App, w: u16, h: u16) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| crate::tui::ui::draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_log_view_actually_shows_log_lines() {
        // Regression: pressing 't' during an install showed an empty log.
        let _home = crate::test_support::test_home();
        let mut app = App::new().unwrap();
        let (screen, _tx) = op_screen(500, false);
        app.screen = screen;
        handle_key(&mut app, KeyCode::Char('t')).unwrap();

        let out = render(&mut app, 150, 46);
        assert!(out.contains("Hide log"), "log view footer missing:\n{out}");
        assert!(
            out.contains("line 499") || out.contains("line 4"),
            "no log lines rendered:\n{out}"
        );
    }

    #[test]
    fn the_log_view_follows_new_output() {
        // With progress lines streaming in, a fixed offset strands the view on
        // stale output. Following must always show the newest window.
        let _home = crate::test_support::test_home();
        let mut app = App::new().unwrap();
        let (screen, _tx) = op_screen(20, false);
        app.screen = screen;
        handle_key(&mut app, KeyCode::Char('t')).unwrap();
        assert!(app.log_follow);

        // Simulate the operation emitting a lot more output.
        if let Screen::Operation { log, .. } = &mut app.screen {
            for i in 20..2000 {
                log.push(format!("line {i}"));
            }
        }
        let out = render(&mut app, 150, 46);
        assert!(out.contains("line 1999"), "newest line not shown:\n{out}");

        // Scrolling up detaches; the newest line should no longer be pinned.
        handle_key(&mut app, KeyCode::Up).unwrap();
        assert!(!app.log_follow, "scrolling up should stop following");
    }

    #[test]
    fn opening_the_log_scrolls_to_content_that_exists() {
        // Regression: log_scroll must leave the visible window inside the log,
        // or the view renders blank even though lines are present.
        let _home = crate::test_support::test_home();
        let mut app = App::new().unwrap();
        let (screen, _tx) = op_screen(500, false);
        app.screen = screen;
        handle_key(&mut app, KeyCode::Char('t')).unwrap();

        let log_len = match &app.screen {
            Screen::Operation { log, .. } => log.len(),
            _ => unreachable!(),
        };
        for visible in [10usize, 28, 40] {
            let scroll = app.log_scroll.min(log_len.saturating_sub(visible));
            assert!(
                scroll < log_len,
                "scroll {scroll} past end of {log_len} lines (visible {visible})"
            );
            assert!(
                log_len - scroll >= visible.min(log_len),
                "only {} lines left to render for a {visible}-row window",
                log_len - scroll
            );
        }
    }
}

/// Regenerates the README's TUI screenshots.
///
/// Run explicitly — it writes into the repository:
///
/// ```sh
/// cargo test --lib readme_screenshots -- --ignored --nocapture
/// python3 scripts/render_screenshots.py
/// ```
///
/// Kept as a test rather than an example or a hidden subcommand because
/// `tui::ui` is private: this is the only place that can already reach the
/// renderer, and none of it ships in the binary.
#[cfg(test)]
mod readme_screenshots {
    use super::*;

    /// A width where nothing important is clipped — the encrypt prompt's option
    /// descriptions run past 100 columns.
    const COLS: u16 = 120;

    /// Dump `app`'s rendered screen as a JSON grid: one entry per cell with its
    /// symbol, foreground, background and whether it is bold.
    ///
    /// A grid rather than an image because nothing in reach renders colour
    /// emoji into SVG — librsvg draws them as black outlines, which on a dark
    /// terminal background is worse than nothing, and the padlocks are half the
    /// point of these screenshots. `scripts/render_screenshots.py` turns these
    /// into PNGs with a font stack that can.
    fn dump_grid(app: &mut App, w: u16, h: u16, name: &str) {
        use ratatui::backend::TestBackend;
        use ratatui::style::{Color, Modifier};
        use ratatui::Terminal;

        fn hex(c: Color) -> Option<String> {
            Some(match c {
                Color::Reset => return None,
                Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
                Color::Black => "#1c2128".into(),
                Color::Red => "#e5534b".into(),
                Color::Green => "#57ab5a".into(),
                Color::Yellow => "#c69026".into(),
                Color::Blue => "#539bf5".into(),
                Color::Magenta => "#b083f0".into(),
                Color::Cyan => "#39c5cf".into(),
                Color::Gray => "#adbac7".into(),
                Color::DarkGray => "#636e7b".into(),
                Color::LightRed => "#ff938a".into(),
                Color::LightGreen => "#6bc46d".into(),
                Color::LightYellow => "#daaa3f".into(),
                Color::LightBlue => "#6cb6ff".into(),
                Color::LightMagenta => "#dcbdfb".into(),
                Color::LightCyan => "#56d4dd".into(),
                Color::White => "#cdd9e5".into(),
                _ => "#cdd9e5".into(),
            })
        }
        fn json_string(s: &str) -> String {
            let mut out = String::from("\"");
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    c if (c as u32) < 0x20 => out.push(' '),
                    c => out.push(c),
                }
            }
            out.push('"');
            out
        }

        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| crate::tui::ui::draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();

        let mut cells = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let cell = &buf[(x, y)];
                let symbol = cell.symbol();
                let bg = hex(cell.bg);
                if symbol.trim().is_empty() && bg.is_none() {
                    continue;
                }
                let mut entry = format!("{{\"x\":{x},\"y\":{y},\"s\":{}", json_string(symbol));
                if let Some(fg) = hex(cell.fg) {
                    entry.push_str(&format!(",\"fg\":\"{fg}\""));
                }
                if let Some(bg) = bg {
                    entry.push_str(&format!(",\"bg\":\"{bg}\""));
                }
                if cell.modifier.contains(Modifier::BOLD) {
                    entry.push_str(",\"b\":1");
                }
                entry.push('}');
                cells.push(entry);
            }
        }

        std::fs::create_dir_all("target/screenshots").unwrap();
        let path = format!("target/screenshots/{name}.json");
        std::fs::write(
            &path,
            format!("{{\"w\":{w},\"h\":{h},\"cells\":[{}]}}", cells.join(",")),
        )
        .unwrap();
        println!("wrote {path}");
    }

    /// A plausible installed tree: a root app with a merged-in alias, plus two
    /// encrypted apps in different states.
    fn fixtures() -> Vec<Manifest> {
        let app = |name: &str, alias_of: Option<&str>, pkg: Option<&str>| Manifest {
            app: crate::manifest::AppMeta {
                name: name.into(),
                main_binary: name.into(),
                installed_at: "2026-07-28T09:14:00Z".into(),
                launchers: vec![name.into()],
                alias_of: alias_of.map(str::to_string),
                display_name: None,
                pkg_name: pkg.map(str::to_string),
                wine_game: None,
            },
            packages: vec![
                crate::manifest::PackageEntry {
                    name: name.into(),
                    version: "141.0.3-1".into(),
                    source: crate::manifest::PackageSource::Official,
                },
                crate::manifest::PackageEntry {
                    name: "gtk3".into(),
                    version: "1:3.24.51-1".into(),
                    source: crate::manifest::PackageSource::Official,
                },
            ],
        };
        vec![
            app("firefox", None, None),
            app("fastfetch", Some("firefox"), None),
            app("signal-desktop", None, None),
            app("thunderbird", None, None),
            app("vivaldi", None, None),
        ]
    }

    fn encrypted() -> HashMap<String, EncState> {
        HashMap::from([
            (
                "signal-desktop".to_string(),
                EncState {
                    locked: true,
                    master: true,
                    fill: None,
                },
            ),
            (
                "thunderbird".to_string(),
                EncState {
                    locked: false,
                    master: false,
                    fill: Some(crate::veracrypt::Usage {
                        used: 1_400 * 1024 * 1024,
                        available: 2_100 * 1024 * 1024,
                        total: 3_600 * 1024 * 1024,
                    }),
                },
            ),
        ])
    }

    fn app_with_fixtures() -> App {
        let mut app = App::new().unwrap();
        app.installed = fixtures();
        app.inst_state.select(Some(2));
        app.encrypted_apps = encrypted();
        app.update_available =
            HashMap::from([("vivaldi".to_string(), "7.6.3797.48-1".to_string())]);
        app
    }

    #[test]
    #[ignore = "writes grids for scripts/render_screenshots.py; run to refresh the README"]
    fn readme_screenshots() {
        let home = crate::test_support::test_home();

        // Give thunderbird a container so the settings screen shows what an
        // encrypted app actually offers, rather than the offer to encrypt it.
        // Only the marker and the container file matter here: nothing is
        // mounted, and `is_encrypted` is a file-exists check.
        let root = home.root();
        std::fs::create_dir_all(root.join(".containers")).unwrap();
        std::fs::write(root.join(".containers/thunderbird.hc"), b"stand-in").unwrap();
        std::fs::write(
            root.join(".containers/thunderbird.toml"),
            "name = \"thunderbird\"\nmain_binary = \"thunderbird\"\n\
             installed_at = \"2026-07-28T09:14:00Z\"\nlaunchers = [\"thunderbird\"]\n\
             password_source = \"prompt\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("thunderbird")).unwrap();

        // A store, so Settings shows what it looks like once there is one to
        // reveal, forget or delete — not just the row offering to create it.
        crate::secrets::init("only-ever-inside-this-sandbox").unwrap();

        // Installed tab: badges in the list, encryption spelled out in details.
        let mut app = app_with_fixtures();
        app.tab = Tab::Installed;
        app.screen = Screen::Main;
        dump_grid(&mut app, COLS, 30, "installed");

        // The unlocked container that is filling up.
        let mut app = app_with_fixtures();
        app.inst_state.select(Some(3));
        app.detail_focused = true;
        dump_grid(&mut app, COLS, 30, "encrypted-details");

        // A plain app's settings: the row that offers to encrypt it.
        let mut app = app_with_fixtures();
        app.screen = Screen::Config {
            app_name: "vivaldi".into(),
            config: crate::config::AppConfig::default(),
            selected: CFG_ENCRYPT_APP,
        };
        dump_grid(&mut app, COLS, 44, "config-encrypt-offer");

        // An encrypted app's settings: password source, lock on exit, and out.
        let mut app = app_with_fixtures();
        app.screen = Screen::Config {
            app_name: "thunderbird".into(),
            config: crate::config::AppConfig::default(),
            selected: CFG_PASSWORD_SOURCE,
        };
        dump_grid(&mut app, COLS, 44, "config-encryption");

        // The choice offered when encrypting an app that is already installed.
        let mut app = app_with_fixtures();
        app.screen = Screen::AskEncrypt {
            pkg: "vivaldi".into(),
            title: "Encrypt — vivaldi".into(),
            args: vec!["encrypt".into(), "vivaldi".into()],
            selected: 2,
            kind: EncryptAsk::Convert,
        };
        dump_grid(&mut app, COLS, 30, "encrypt-choice");

        // Install tab, regenerated alongside the rest so every picture in the
        // README shares one look.
        let mut app = app_with_fixtures();
        app.tab = Tab::Install;
        app.screen = Screen::Main;
        app.search_input = "keepass".into();
        // The second field is the repo tag the results are labelled with, not a
        // description — a screenshot showing otherwise would teach the wrong
        // thing about the list.
        app.search_results = vec![
            ("keepassxc".into(), Some("extra".into())),
            ("keepass".into(), Some("extra".into())),
            ("keepassxc-browser".into(), Some("aur".into())),
            ("keepmenu".into(), Some("aur".into())),
        ];
        app.selected_pkgs = std::collections::HashSet::from(["keepassxc".to_string()]);
        app.avail_state.select(Some(0));
        app.search_list_focused = true;
        dump_grid(&mut app, COLS, 30, "install");

        // Settings tab, including the master password store.
        let mut app = app_with_fixtures();
        app.tab = Tab::Settings;
        app.screen = Screen::Main;
        dump_grid(&mut app, COLS, 44, "settings");
    }
}
