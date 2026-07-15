use wryayer::{avahi_stub, commands};

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "wryayer",
    about = "Isolated per-app package manager (Arch Linux and Debian/Ubuntu)",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Install a package and all its dependencies into an isolated app directory
    Install {
        /// The package name (AUR or official repo)
        pkg: String,
        /// Override the app directory name under ~/.wryayer/ (default: pkg name)
        #[arg(long)]
        app_name: Option<String>,
        /// Override the launcher binary name placed in ~/bin/ (default: pkg name)
        #[arg(long)]
        bin_name: Option<String>,
        /// Create multiple launchers — comma-separated list of binary names
        /// (overrides --bin-name). Each binary must exist in the package.
        #[arg(long, value_delimiter = ',')]
        bin_names: Vec<String>,
        /// Install this package additively into an existing app's directory
        /// instead of creating a new one. Useful for plugins, multi-tool bundles.
        #[arg(long)]
        into: Option<String>,
        /// Keep installed files even when no launcher binary was found (no ~/bin/ shortcut created).
        /// Used internally by the TUI after the user confirms the choice popup.
        #[arg(long, hide = true)]
        keep_without_launcher: bool,
        /// Refresh package databases with 'sudo pacman -Sy' before downloading.
        /// Used internally by the TUI after the user confirms an outdated-databases popup.
        #[arg(long, hide = true)]
        sync_db: bool,
    },
    /// Remove an installed app and its launchers
    Remove {
        /// The app name as shown by `wryayer list`
        app_name: String,
        /// Also remove all alias apps that point at this target
        #[arg(long)]
        cascade: bool,
    },
    /// List all installed apps
    List,
    /// Run an installed app with its isolated environment
    Run {
        /// The app name as shown by `wryayer list`
        app_name: String,
        /// Run a specific binary inside the app (default: app's main binary)
        #[arg(long)]
        bin: Option<String>,
        /// Arguments to pass to the app binary
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Update one or all installed apps
    Update {
        /// The app to update (default: all apps)
        app_name: Option<String>,
        /// Show available updates without installing them
        #[arg(long)]
        check: bool,
    },
    /// Scan an app for missing shared libraries and install them
    Repair {
        /// The app name as shown by `wryayer list`
        app_name: String,
    },
    /// View or change per-app configuration
    Config {
        /// The app name as shown by `wryayer list`
        app_name: String,
        #[command(subcommand)]
        setting: Option<ConfigSetting>,
    },
    /// Create a zip export of an installed app
    Export {
        /// The app name as shown by `wryayer list`
        app_name: String,
        /// Output file path (default: ./<app>-YYYY-MM-DD.zip)
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// Import an app from a wryayer export zip
    Import {
        /// Path to the zip file created by `wryayer export`
        path: PathBuf,
    },
    /// Import a Windows game folder as a self-contained wine container.
    /// Each game gets its own ~/.wryayer/<name>/ with a fresh wine install
    /// and its own WINEPREFIX, so games can't interfere with each other.
    InstallGame {
        /// Path to the game folder on the host (will be copied into the container)
        path: PathBuf,
        /// Relative path (inside the game folder) of the main .exe to launch.
        /// If omitted, wryayer scores all .exe files and picks the most likely one.
        #[arg(long)]
        exe: Option<String>,
        /// Override the container name (default: sanitized folder name)
        #[arg(long)]
        app_name: Option<String>,
        /// Delete the source folder after a successful copy
        #[arg(long)]
        delete_source: bool,
        /// Skip the disk-space precheck (use if statvfs is unreliable)
        #[arg(long)]
        skip_size_check: bool,
    },
    /// Create a hard-linked snapshot of an installed app (cheap, instant)
    Snapshot {
        /// The app name as shown by `wryayer list`
        app_name: String,
    },
    /// Roll an installed app back to a previous snapshot
    Rollback {
        /// The app name as shown by `wryayer list`
        app_name: String,
        /// Snapshot label to restore (default: most recent)
        snapshot: Option<String>,
    },
    /// List snapshots for an installed app
    Snapshots {
        /// The app name as shown by `wryayer list`
        app_name: String,
    },
    /// Delete old snapshots, keeping the N most recent
    SnapshotPrune {
        /// The app name as shown by `wryayer list`
        app_name: String,
        /// Number of most-recent snapshots to keep (default: 3)
        #[arg(long, default_value = "3")]
        keep: usize,
    },
    /// Delete a single snapshot by label
    SnapshotDelete {
        /// The app name as shown by `wryayer list`
        app_name: String,
        /// Snapshot label to delete (see `wryayer snapshots <app>`)
        snapshot: String,
    },
    /// Launch the interactive TUI
    Tui,
    /// Launch the native GTK desktop GUI (requires a build with --features gui)
    Gui,
    /// Hard-link identical files across app directories to reclaim disk space
    Dedup {
        /// Print every file that gets linked
        #[arg(long, short)]
        verbose: bool,
    },
    /// Delete the shared download/build cache (~/.cache/wryayer)
    Clean,
    /// Print shell completion script to stdout
    Completions {
        /// Shell to generate completions for (bash, fish, zsh, elvish, powershell)
        shell: Shell,
    },
    /// Internal: run the private-bus Avahi stub for a sandbox (not for direct use)
    #[command(hide = true)]
    AvahiStub {
        /// Path the private dbus-daemon listens on
        socket: String,
        /// Path to the generated dbus-daemon config file
        config: String,
    },
    /// Internal: host-side portal listener for cross-container app binding (not for direct use)
    #[command(hide = true)]
    PortalListener {
        /// AF_UNIX socket path to listen on
        socket: String,
        /// Comma-separated list of app names allowed to be launched
        allowed: String,
    },
}

#[derive(Subcommand)]
enum ConfigSetting {
    /// Set temp directory mode (system | ramdisk | local | uuid)
    Tempmode {
        /// system  = share host /tmp
        /// ramdisk = private in-memory tmpfs, wiped on close
        /// local   = persistent per-app dir (see tempdelete)
        /// uuid    = per-instance UUID dir, wiped on close
        mode: String,
    },
    /// Set temp cleanup policy — only applies when tempmode is local
    Tempdelete {
        /// never    = keep temp across restarts
        /// on_start = wipe on launch when no other instance is running
        /// on_close = wipe when this instance exits
        policy: String,
    },
    /// Enable or disable network access inside the sandbox
    Network {
        /// on = allow internet access (default), off = block all network
        enabled: String,
    },
    /// Enable or disable camera access inside the sandbox
    Camera {
        /// on = allow /dev/video* access (default), off = mask all cameras
        enabled: String,
    },
    /// Enable or disable microphone input inside the sandbox
    Microphone {
        /// on = allow mic input (default), off = mask ALSA capture devices
        enabled: String,
    },
    /// Enable or disable audio output inside the sandbox
    Audio {
        /// on = allow audio output (default), off = mask ALSA + PipeWire/PulseAudio
        enabled: String,
    },
    /// Manage host directories shared read-write into the sandbox
    Share {
        #[command(subcommand)]
        action: ShareAction,
    },
    /// Set hostname shown inside the sandbox, or "off" to disable
    SpoofHostname {
        /// Hostname string, or "off" to disable
        value: String,
    },
    /// Set $USER/$LOGNAME inside the sandbox, or "off" to disable
    SpoofUsername {
        /// Username string, or "off" to disable
        value: String,
    },
    /// Set /etc/machine-id inside the sandbox ("random" = fresh UUID each launch, "off" = disable)
    SpoofMachineId {
        /// ID value, "random", or "off"
        value: String,
    },
    /// Bind a custom file over /proc/cpuinfo inside the sandbox, or "off" to disable
    SpoofCpuinfo {
        /// Path to a cpuinfo file, or "off" to disable
        path: String,
    },
    /// Override /etc/os-release inside the sandbox ("sample" = generic, "off" = disable, or an OS name like "ubuntu")
    SpoofOs {
        /// OS name string, "sample", or "off" to disable
        value: String,
    },
    /// Detect the real terminal and pass it into the sandbox so tools like fastfetch show the correct terminal name
    SpoofTerminal {
        /// on = detect and forward terminal identity, off = do nothing (default)
        value: String,
    },
    /// Report a fake system uptime (fools fastfetch, uptime/w, sysinfo readers)
    SpoofUptime {
        /// Duration like 3d4h / 90m, bare seconds, or "system" to disable
        value: String,
    },
    /// Limit maximum RAM usage in MiB via systemd-run (0 or "none" = no limit)
    RamLimit {
        /// RAM limit in MiB (e.g. 2048 for 2 GiB), or "none" to disable
        mib: String,
    },
}

#[derive(Subcommand)]
enum ShareAction {
    /// Add a directory to the shared list
    Add {
        /// Absolute path of the directory to share
        path: String,
    },
    /// Remove a directory from the shared list
    Remove {
        /// Absolute path of the directory to remove
        path: String,
    },
    /// List currently shared directories
    List,
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Install { pkg, app_name, bin_name, bin_names, into, keep_without_launcher, sync_db } => {
            let names: Vec<String> = if !bin_names.is_empty() {
                bin_names
            } else if let Some(b) = bin_name {
                vec![b]
            } else {
                vec![]
            };
            commands::install::run(&pkg, app_name.as_deref(), &names, into.as_deref(), keep_without_launcher, sync_db)
        }
        Commands::Remove { app_name, cascade } => {
            if cascade {
                commands::remove::run_cascade(&app_name)
            } else {
                commands::remove::run(&app_name)
            }
        }
        Commands::List => commands::list::run(),
        Commands::Run { app_name, bin, args } => commands::run::run(&app_name, bin.as_deref(), &args),
        Commands::Update { app_name, check } => {
            commands::update::run(app_name.as_deref(), check)
        }
        Commands::Repair { app_name } => commands::repair::run(&app_name),
        Commands::Config { app_name, setting } => match setting {
            None => commands::config::run(&app_name, None, None, None, None, None, None, None, None, None, None, None, None, None, None),
            Some(ConfigSetting::Tempmode { mode }) => {
                commands::config::run(&app_name, Some(&mode), None, None, None, None, None, None, None, None, None, None, None, None, None)
            }
            Some(ConfigSetting::Tempdelete { policy }) => {
                commands::config::run(&app_name, None, Some(&policy), None, None, None, None, None, None, None, None, None, None, None, None)
            }
            Some(ConfigSetting::Network { enabled }) => {
                commands::config::run(&app_name, None, None, Some(&enabled), None, None, None, None, None, None, None, None, None, None, None)
            }
            Some(ConfigSetting::Camera { enabled }) => {
                commands::config::run(&app_name, None, None, None, Some(&enabled), None, None, None, None, None, None, None, None, None, None)
            }
            Some(ConfigSetting::Microphone { enabled }) => {
                commands::config::run(&app_name, None, None, None, None, Some(&enabled), None, None, None, None, None, None, None, None, None)
            }
            Some(ConfigSetting::Audio { enabled }) => {
                commands::config::run(&app_name, None, None, None, None, None, Some(&enabled), None, None, None, None, None, None, None, None)
            }
            Some(ConfigSetting::Share { action }) => match action {
                ShareAction::Add { path } => commands::config::share_add(&app_name, &path),
                ShareAction::Remove { path } => commands::config::share_remove(&app_name, &path),
                ShareAction::List => commands::config::share_list(&app_name),
            },
            Some(ConfigSetting::SpoofHostname { value }) => {
                commands::config::run(&app_name, None, None, None, None, None, None, Some(&value), None, None, None, None, None, None, None)
            }
            Some(ConfigSetting::SpoofUsername { value }) => {
                commands::config::run(&app_name, None, None, None, None, None, None, None, Some(&value), None, None, None, None, None, None)
            }
            Some(ConfigSetting::SpoofMachineId { value }) => {
                commands::config::run(&app_name, None, None, None, None, None, None, None, None, Some(&value), None, None, None, None, None)
            }
            Some(ConfigSetting::SpoofCpuinfo { path }) => {
                commands::config::run(&app_name, None, None, None, None, None, None, None, None, None, Some(&path), None, None, None, None)
            }
            Some(ConfigSetting::SpoofOs { value }) => {
                commands::config::run(&app_name, None, None, None, None, None, None, None, None, None, None, Some(&value), None, None, None)
            }
            Some(ConfigSetting::SpoofTerminal { value }) => {
                commands::config::run(&app_name, None, None, None, None, None, None, None, None, None, None, None, Some(&value), None, None)
            }
            Some(ConfigSetting::SpoofUptime { value }) => {
                commands::config::run(&app_name, None, None, None, None, None, None, None, None, None, None, None, None, Some(&value), None)
            }
            Some(ConfigSetting::RamLimit { mib }) => {
                commands::config::run(&app_name, None, None, None, None, None, None, None, None, None, None, None, None, None, Some(&mib))
            }
        },
        Commands::Export { app_name, output } => {
            commands::export::run(&app_name, output.as_ref())
        }
        Commands::Import { path } => commands::import::run(&path),
        Commands::InstallGame { path, exe, app_name, delete_source, skip_size_check } => {
            commands::install_game::run(
                &path,
                exe.as_deref(),
                app_name.as_deref(),
                delete_source,
                skip_size_check,
            )
        }
        Commands::Snapshot { app_name } => commands::snapshot::create(&app_name).map(|_| ()),
        Commands::Rollback { app_name, snapshot } => {
            commands::snapshot::rollback(&app_name, snapshot.as_deref())
        }
        Commands::Snapshots { app_name } => commands::snapshot::list(&app_name),
        Commands::SnapshotPrune { app_name, keep } => commands::snapshot::prune(&app_name, keep),
        Commands::SnapshotDelete { app_name, snapshot } => {
            commands::snapshot::delete(&app_name, &snapshot)
        }
        Commands::Tui => {
            #[cfg(feature = "tui")]
            {
                wryayer::tui::run()
            }
            #[cfg(not(feature = "tui"))]
            {
                Err(anyhow::anyhow!(
                    "this build has no TUI. Rebuild with the tui feature:\n    cargo build --release --features tui"
                ))
            }
        }
        Commands::Gui => {
            #[cfg(feature = "gui")]
            {
                wryayer::gui::run()
            }
            #[cfg(not(feature = "gui"))]
            {
                Err(anyhow::anyhow!(
                    "this build has no GUI. Rebuild with the gui feature:\n    cargo build --release --features gui"
                ))
            }
        }
        Commands::Dedup { verbose } => commands::dedup::run(verbose),
        Commands::Clean => commands::clean::run(),
        Commands::Completions { shell } => {
            generate(shell, &mut Cli::command(), "wryayer", &mut std::io::stdout());
            Ok(())
        }
        Commands::AvahiStub { socket, config } => avahi_stub::run(&socket, &config),
        Commands::PortalListener { socket, allowed } => commands::portal::run(&socket, &allowed),
    };

    if let Err(e) = result {
        eprintln!("error: {:#}", e);
        std::process::exit(1);
    }
}
