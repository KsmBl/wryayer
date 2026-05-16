mod commands;
mod config;
mod launcher;
mod manifest;
mod package;

use clap::{Parser, Subcommand};
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
    /// Create a zip backup of an installed app
    Backup {
        /// The app name as shown by `wryayer list`
        app_name: String,
        /// Output file path (default: ./<app>-YYYY-MM-DD.zip)
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// Import an app from a wryayer backup zip
    Import {
        /// Path to the zip file created by `wryayer backup`
        path: PathBuf,
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
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Install { pkg, app_name, bin_name } => {
            commands::install::run(&pkg, app_name.as_deref(), bin_name.as_deref())
        }
        Commands::Remove { app_name } => commands::remove::run(&app_name),
        Commands::List => commands::list::run(),
        Commands::Run { app_name, args } => commands::run::run(&app_name, &args),
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
        },
        Commands::Backup { app_name, output } => {
            commands::backup::run(&app_name, output.as_ref())
        }
        Commands::Import { path } => commands::import::run(&path),
    };

    if let Err(e) = result {
        eprintln!("error: {:#}", e);
        std::process::exit(1);
    }
}
