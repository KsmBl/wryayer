use wryayer::{commands, tui};

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "wryayer",
    about = "Isolated per-app package manager for Arch Linux",
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
    },
    /// Remove an installed app and its launchers
    Remove {
        /// The app name as shown by `wryayer list`
        app_name: String,
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
    /// Launch the interactive TUI
    Tui,
    /// Hard-link identical files across app directories to reclaim disk space
    Dedup {
        /// Print every file that gets linked
        #[arg(long, short)]
        verbose: bool,
    },
    /// Print shell completion script to stdout
    Completions {
        /// Shell to generate completions for (bash, fish, zsh, elvish, powershell)
        shell: Shell,
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
        Commands::Install { pkg, app_name, bin_name, bin_names, into } => {
            let names: Vec<String> = if !bin_names.is_empty() {
                bin_names
            } else if let Some(b) = bin_name {
                vec![b]
            } else {
                vec![]
            };
            commands::install::run(&pkg, app_name.as_deref(), &names, into.as_deref())
        }
        Commands::Remove { app_name } => commands::remove::run(&app_name),
        Commands::List => commands::list::run(),
        Commands::Run { app_name, bin, args } => commands::run::run(&app_name, bin.as_deref(), &args),
        Commands::Update { app_name, check } => {
            commands::update::run(app_name.as_deref(), check)
        }
        Commands::Repair { app_name } => commands::repair::run(&app_name),
        Commands::Config { app_name, setting } => match setting {
            None => commands::config::run(&app_name, None, None, None, None, None, None),
            Some(ConfigSetting::Tempmode { mode }) => {
                commands::config::run(&app_name, Some(&mode), None, None, None, None, None)
            }
            Some(ConfigSetting::Tempdelete { policy }) => {
                commands::config::run(&app_name, None, Some(&policy), None, None, None, None)
            }
            Some(ConfigSetting::Network { enabled }) => {
                commands::config::run(&app_name, None, None, Some(&enabled), None, None, None)
            }
            Some(ConfigSetting::Camera { enabled }) => {
                commands::config::run(&app_name, None, None, None, Some(&enabled), None, None)
            }
            Some(ConfigSetting::Microphone { enabled }) => {
                commands::config::run(&app_name, None, None, None, None, Some(&enabled), None)
            }
            Some(ConfigSetting::Audio { enabled }) => {
                commands::config::run(&app_name, None, None, None, None, None, Some(&enabled))
            }
            Some(ConfigSetting::Share { action }) => match action {
                ShareAction::Add { path } => commands::config::share_add(&app_name, &path),
                ShareAction::Remove { path } => commands::config::share_remove(&app_name, &path),
                ShareAction::List => commands::config::share_list(&app_name),
            },
        },
        Commands::Export { app_name, output } => {
            commands::export::run(&app_name, output.as_ref())
        }
        Commands::Import { path } => commands::import::run(&path),
        Commands::Snapshot { app_name } => commands::snapshot::create(&app_name).map(|_| ()),
        Commands::Rollback { app_name, snapshot } => {
            commands::snapshot::rollback(&app_name, snapshot.as_deref())
        }
        Commands::Snapshots { app_name } => commands::snapshot::list(&app_name),
        Commands::Tui => tui::run(),
        Commands::Dedup { verbose } => commands::dedup::run(verbose),
        Commands::Completions { shell } => {
            generate(shell, &mut Cli::command(), "wryayer", &mut std::io::stdout());
            Ok(())
        }
    };

    if let Err(e) = result {
        eprintln!("error: {:#}", e);
        std::process::exit(1);
    }
}
