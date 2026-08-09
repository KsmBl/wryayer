# wryayer fish shell completions

# ── Helpers ───────────────────────────────────────────────────────────────────

# List installed app names by reading manifests directly — no subprocess needed.
function __wryayer_apps
    if not test -d ~/.wryayer
        return
    end
    for d in ~/.wryayer/*/
        if test -f $d/.manifest.toml
            basename $d
        end
    end
end

# List available packages via pacman (Arch / derivatives).
function __wryayer_pkgs
    pacman -Ssq 2>/dev/null
end

# List snapshot labels for the app that appears immediately after 'rollback'
# in the current command line. Reads from the filesystem — fast.
function __wryayer_rollback_snapshots
    set -l cmd (commandline -opc)
    set -l past_rollback 0
    for tok in $cmd
        if test $tok = rollback
            set past_rollback 1
        else if test $past_rollback = 1; and not string match -q -- '-*' $tok
            set -l snap_dir ~/.wryayer/$tok/.snapshots
            if test -d $snap_dir
                for d in $snap_dir/*/
                    basename $d
                end
            end
            return
        end
    end
end

# Snapshot labels for the app that follows 'snapshot-delete' on the command line.
function __wryayer_snapdel_snapshots
    set -l cmd (commandline -opc)
    set -l past 0
    for tok in $cmd
        if test $tok = snapshot-delete
            set past 1
        else if test $past = 1; and not string match -q -- '-*' $tok
            set -l snap_dir ~/.wryayer/$tok/.snapshots
            if test -d $snap_dir
                for d in $snap_dir/*/
                    basename $d
                end
            end
            return
        end
    end
end

# True when an app name already follows 'snapshot-delete'.
function __wryayer_snapdel_has_app
    set -l cmd (commandline -opc)
    set -l past 0
    for tok in $cmd
        if test $tok = snapshot-delete
            set past 1
        else if test $past = 1; and not string match -q -- '-*' $tok
            return 0
        end
    end
    return 1
end

# True when at least one positional arg already follows 'rollback'.
function __wryayer_rollback_needs_snapshot
    set -l cmd (commandline -opc)
    set -l past 0
    set -l n 0
    for tok in $cmd
        if test $tok = rollback
            set past 1
        else if test $past = 1; and not string match -q -- '-*' $tok
            set n (math $n + 1)
        end
    end
    test $n -ge 1
end

# True when at least one installed app name appears in the command line.
# Used to gate Level-2 (setting keyword) and Level-3 (value) completions
# inside the 'config' subcommand.
function __wryayer_config_has_app
    for app in (__wryayer_apps)
        if __fish_seen_subcommand_from $app
            return 0
        end
    end
    return 1
end

# ── Disable file completions globally ─────────────────────────────────────────
complete -c wryayer -f

# ── Top-level subcommands ─────────────────────────────────────────────────────
# Front-end subcommands (tui/gui) are appended by install.sh to match the build;
# they are kept in this guard list so once one is typed, top-level completions stop.
set -l cmds install remove list run update repair config export import \
           snapshot rollback snapshots snapshot-prune snapshot-delete tui gui dedup completions

complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a install         -d 'Install a package in an isolated directory'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a remove          -d 'Remove an installed app and its launchers'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a list            -d 'List all installed apps'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a run             -d 'Run an installed app in its isolated environment'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a update          -d 'Update one or all installed apps'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a repair          -d 'Scan an app for missing shared libraries and install them'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a config          -d 'View or change per-app configuration'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a export          -d 'Pack an installed app into a portable zip'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a import          -d 'Import an app from a wryayer export zip'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a snapshot        -d 'Create a hard-linked snapshot of an installed app'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a rollback        -d 'Roll an app back to a previous snapshot'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a snapshots       -d 'List snapshots for an installed app'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a snapshot-prune  -d 'Delete old snapshots, keeping the N most recent'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a tui             -d 'Launch the interactive TUI'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a dedup           -d 'Hard-link identical files across app directories'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a completions     -d 'Print shell completion script to stdout'

# ── install ───────────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from install' -a '(__wryayer_pkgs)' -d 'package'
complete -c wryayer -n '__fish_seen_subcommand_from install' -l app-name  -d 'Override app directory name under ~/.wryayer/' -r
complete -c wryayer -n '__fish_seen_subcommand_from install' -l bin-name  -d 'Override launcher binary name in /usr/bin/' -r
complete -c wryayer -n '__fish_seen_subcommand_from install' -l bin-names -d 'Comma-separated list of launcher binary names' -r
complete -c wryayer -n '__fish_seen_subcommand_from install' -l into      -d 'Merge into an existing app instead of creating a new one' -xa '(__wryayer_apps)'

# ── remove ────────────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from remove' -a '(__wryayer_apps)' -d 'installed app'
complete -c wryayer -n '__fish_seen_subcommand_from remove' -l cascade -d 'Also remove all alias apps that point at this target'

# ── run ───────────────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from run' -a '(__wryayer_apps)' -d 'installed app'
complete -c wryayer -n '__fish_seen_subcommand_from run' -l bin -d 'Run a specific binary registered for the app' -r

# ── update ────────────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from update' -a '(__wryayer_apps)' -d 'installed app (omit to update all)'
complete -c wryayer -n '__fish_seen_subcommand_from update' -l check -d 'Show available updates without installing'

# ── repair ────────────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from repair' -a '(__wryayer_apps)' -d 'installed app'

# ── export ────────────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from export' -a '(__wryayer_apps)' -d 'installed app'
complete -c wryayer -n '__fish_seen_subcommand_from export' -l output -s o -d 'Output zip file path' -r

# ── import — re-enable file completion so the user can navigate to the zip ───
complete -c wryayer -n '__fish_seen_subcommand_from import' -F

# ── snapshot ──────────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from snapshot' -a '(__wryayer_apps)' -d 'installed app'

# ── snapshots ─────────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from snapshots' -a '(__wryayer_apps)' -d 'installed app'

# ── rollback — two levels: app name, then snapshot label ─────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from rollback; and not __wryayer_rollback_needs_snapshot' \
    -a '(__wryayer_apps)' -d 'installed app'
complete -c wryayer -n '__fish_seen_subcommand_from rollback; and __wryayer_rollback_needs_snapshot' \
    -a '(__wryayer_rollback_snapshots)' -d 'snapshot label'

# ── snapshot-delete — app name, then snapshot label ──────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from snapshot-delete; and not __wryayer_snapdel_has_app' \
    -a '(__wryayer_apps)' -d 'installed app'
complete -c wryayer -n '__fish_seen_subcommand_from snapshot-delete; and __wryayer_snapdel_has_app' \
    -a '(__wryayer_snapdel_snapshots)' -d 'snapshot label'

# ── snapshot-prune ────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from snapshot-prune' -a '(__wryayer_apps)' -d 'installed app'
complete -c wryayer -n '__fish_seen_subcommand_from snapshot-prune' -l keep \
    -d 'Number of most-recent snapshots to keep (default: 3)' -r

# ── dedup ─────────────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from dedup' -l verbose -s v -d 'Print every file that gets linked'

# ── completions ───────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from completions' \
    -a 'bash fish zsh elvish powershell' -d 'target shell'

# ── config — three-level completion tree ──────────────────────────────────────
#
# Level 1:  config <TAB>                              → installed app names
# Level 2:  config firefox <TAB>                      → setting keywords
# Level 3:  config firefox tempmode <TAB>             → system ramdisk local uuid
#           config firefox tempdelete <TAB>           → never on_start on_close
#           config firefox network|camera|... <TAB>   → on off
#           config firefox share <TAB>                → add remove list
#           config firefox share add <TAB>            → (directories)
#           config firefox share remove <TAB>         → (directories)
#           config firefox spoof-hostname <TAB>       → sample system off
#           config firefox spoof-username <TAB>       → sample system off
#           config firefox spoof-machine-id <TAB>     → system random sample off
#           config firefox spoof-cpuinfo <TAB>        → sample system off (+ files)
#           config firefox spoof-os <TAB>             → system ubuntu arch windows arduinoide off
#           config firefox spoof-terminal <TAB>       → on off
#           config firefox ram-limit <TAB>            → none 512 1024 2048 4096 8192

set -l cfg_settings \
    tempmode tempdelete \
    network camera microphone audio \
    share \
    spoof-hostname spoof-username spoof-machine-id spoof-cpuinfo spoof-os spoof-terminal \
    ram-limit

# ── Level 1 — app name ────────────────────────────────────────────────────────
complete -c wryayer \
    -n "__fish_seen_subcommand_from config; and not __wryayer_config_has_app; and not __fish_seen_subcommand_from $cfg_settings" \
    -a '(__wryayer_apps)' -d 'installed app'

# ── Level 2 — setting keyword ─────────────────────────────────────────────────
complete -c wryayer \
    -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $cfg_settings" \
    -a tempmode       -d 'Set temp directory mode'
complete -c wryayer \
    -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $cfg_settings" \
    -a tempdelete     -d 'Set temp cleanup policy (only applies when tempmode=local)'
complete -c wryayer \
    -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $cfg_settings" \
    -a network        -d 'Enable or disable network access'
complete -c wryayer \
    -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $cfg_settings" \
    -a camera         -d 'Enable or disable camera access'
complete -c wryayer \
    -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $cfg_settings" \
    -a microphone     -d 'Enable or disable microphone input'
complete -c wryayer \
    -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $cfg_settings" \
    -a audio          -d 'Enable or disable audio output'
complete -c wryayer \
    -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $cfg_settings" \
    -a share          -d 'Manage directories shared read-write into the sandbox'
complete -c wryayer \
    -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $cfg_settings" \
    -a spoof-hostname  -d 'Override /etc/hostname and $HOSTNAME inside the sandbox'
complete -c wryayer \
    -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $cfg_settings" \
    -a spoof-username  -d 'Override $USER and $LOGNAME inside the sandbox'
complete -c wryayer \
    -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $cfg_settings" \
    -a spoof-machine-id -d 'Override /etc/machine-id inside the sandbox'
complete -c wryayer \
    -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $cfg_settings" \
    -a spoof-cpuinfo   -d 'Override /proc/cpuinfo inside the sandbox'
complete -c wryayer \
    -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $cfg_settings" \
    -a spoof-os        -d 'Override /etc/os-release inside the sandbox'
complete -c wryayer \
    -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $cfg_settings" \
    -a spoof-terminal  -d 'Detect real terminal and pass it into sandbox (fixes fastfetch showing bwrap)'
complete -c wryayer \
    -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $cfg_settings" \
    -a ram-limit       -d 'Limit maximum RAM usage in MiB (requires systemd)'

# ── Level 3a — tempmode values ────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from tempmode' -a system   -d 'Share host /tmp (default)'
complete -c wryayer -n '__fish_seen_subcommand_from tempmode' -a ramdisk  -d 'Private in-memory tmpfs — fast, wiped on close'
complete -c wryayer -n '__fish_seen_subcommand_from tempmode' -a local    -d 'Persistent per-app dir — lifetime controlled by tempdelete'
complete -c wryayer -n '__fish_seen_subcommand_from tempmode' -a uuid     -d 'Per-instance UUID dir — isolated, wiped on close'

# ── Level 3b — tempdelete values ─────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from tempdelete' -a never    -d 'Keep temp dir across restarts'
complete -c wryayer -n '__fish_seen_subcommand_from tempdelete' -a on_start -d 'Wipe on launch when no other instance is running'
complete -c wryayer -n '__fish_seen_subcommand_from tempdelete' -a on_close -d 'Wipe when this instance exits'

# ── Level 3c — on/off toggles (network, camera, microphone, audio) ───────────
for _setting in network camera microphone audio
    complete -c wryayer -n "__fish_seen_subcommand_from $_setting" -a on  -d 'Enable (default)'
    complete -c wryayer -n "__fish_seen_subcommand_from $_setting" -a off -d 'Disable'
end

# ── Level 3d — share subcommands and their arguments ─────────────────────────
complete -c wryayer -n "__fish_seen_subcommand_from share; and not __fish_seen_subcommand_from add remove list" \
    -a add    -d 'Add a directory to the shared list'
complete -c wryayer -n "__fish_seen_subcommand_from share; and not __fish_seen_subcommand_from add remove list" \
    -a remove -d 'Remove a directory from the shared list'
complete -c wryayer -n "__fish_seen_subcommand_from share; and not __fish_seen_subcommand_from add remove list" \
    -a list   -d 'List currently shared directories'
# Both add and remove take a directory path — enable file completion
complete -c wryayer -n '__fish_seen_subcommand_from share; and __fish_seen_subcommand_from add'    -F
complete -c wryayer -n '__fish_seen_subcommand_from share; and __fish_seen_subcommand_from remove' -F

# ── Level 3e — spoof-hostname values ─────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from spoof-hostname' -a system -d 'Use real hostname (no spoofing)'
complete -c wryayer -n '__fish_seen_subcommand_from spoof-hostname' -a sample -d 'Generic hostname: workstation'
complete -c wryayer -n '__fish_seen_subcommand_from spoof-hostname' -a off    -d 'Alias for system — disable spoofing'

# ── Level 3f — spoof-username values ─────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from spoof-username' -a system -d 'Use real username (no spoofing)'
complete -c wryayer -n '__fish_seen_subcommand_from spoof-username' -a sample -d 'Generic username: user'
complete -c wryayer -n '__fish_seen_subcommand_from spoof-username' -a off    -d 'Alias for system — disable spoofing'

# ── Level 3g — spoof-machine-id values ───────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from spoof-machine-id' -a system -d 'Use real machine-id (no spoofing)'
complete -c wryayer -n '__fish_seen_subcommand_from spoof-machine-id' -a random -d 'Generate a fresh UUID on every launch'
complete -c wryayer -n '__fish_seen_subcommand_from spoof-machine-id' -a sample -d 'Fixed placeholder: cafebabe0011223344556677deadbeef'
complete -c wryayer -n '__fish_seen_subcommand_from spoof-machine-id' -a off    -d 'Alias for system — disable spoofing'

# ── Level 3h — spoof-cpuinfo values (+ file completion for custom paths) ──────
complete -c wryayer -n '__fish_seen_subcommand_from spoof-cpuinfo' -a sample -d 'Built-in generic Intel Core i7-8550U cpuinfo'
complete -c wryayer -n '__fish_seen_subcommand_from spoof-cpuinfo' -a system -d 'Use real /proc/cpuinfo (no spoofing)'
complete -c wryayer -n '__fish_seen_subcommand_from spoof-cpuinfo' -a off    -d 'Alias for system — disable spoofing'
complete -c wryayer -n '__fish_seen_subcommand_from spoof-cpuinfo' -F

# ── Level 3i — spoof-os values ────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from spoof-os' -a system     -d 'Use real /etc/os-release (no spoofing)'
complete -c wryayer -n '__fish_seen_subcommand_from spoof-os' -a ubuntu     -d 'Spoof as Ubuntu 24.04 LTS'
complete -c wryayer -n '__fish_seen_subcommand_from spoof-os' -a arch       -d 'Spoof as Arch Linux'
complete -c wryayer -n '__fish_seen_subcommand_from spoof-os' -a windows    -d 'Spoof as Windows 11'
complete -c wryayer -n '__fish_seen_subcommand_from spoof-os' -a arduinoide -d 'Spoof as ArduinoIDE'
complete -c wryayer -n '__fish_seen_subcommand_from spoof-os' -a off        -d 'Alias for system — disable spoofing'

# ── Level 3i2 — spoof-terminal values ────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from spoof-terminal' -a on  -d 'Detect real terminal and set TERM_PROGRAM inside the sandbox'
complete -c wryayer -n '__fish_seen_subcommand_from spoof-terminal' -a off -d 'Disable terminal spoofing (default)'

# ── Level 3j — ram-limit values ──────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from ram-limit' -a none -d 'No RAM limit (default)'
complete -c wryayer -n '__fish_seen_subcommand_from ram-limit' -a 512  -d '512 MiB'
complete -c wryayer -n '__fish_seen_subcommand_from ram-limit' -a 1024 -d '1 GiB'
complete -c wryayer -n '__fish_seen_subcommand_from ram-limit' -a 2048 -d '2 GiB'
complete -c wryayer -n '__fish_seen_subcommand_from ram-limit' -a 4096 -d '4 GiB'
complete -c wryayer -n '__fish_seen_subcommand_from ram-limit' -a 8192 -d '8 GiB'
