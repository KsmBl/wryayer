mod commands;
mod config;
mod launcher;
mod manifest;
mod package;

use clap::{Parser, Subcommand};

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
        Commands::Update { app_name } => commands::update::run(app_name.as_deref()),
        Commands::Repair { app_name } => commands::repair::run(&app_name),
        Commands::Config { app_name, setting } => match setting {
            None => commands::config::run(&app_name, None, None),
            Some(ConfigSetting::Tempmode { mode }) => {
                commands::config::run(&app_name, Some(&mode), None)
            }
            Some(ConfigSetting::Tempdelete { policy }) => {
                commands::config::run(&app_name, None, Some(&policy))
            }
        },
    };

    if let Err(e) = result {
        eprintln!("error: {:#}", e);
        std::process::exit(1);
    }
}
