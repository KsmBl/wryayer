# wryayer fish shell completions

# ── Helpers ───────────────────────────────────────────────────────────────────

function __wryayer_apps
    wryayer list 2>/dev/null | awk 'NR>2 && NF>0 {print $1}'
end

function __wryayer_pkgs
    pacman -Ssq 2>/dev/null
end

# True when at least one installed app name appears in the current command line.
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
set -l cmds install remove list run update repair config export import snapshot rollback snapshots tui dedup completions
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a install     -d 'Install a package in an isolated directory'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a remove      -d 'Remove an installed app and its launchers'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a list        -d 'List all installed apps'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a run         -d 'Run an installed app in its isolated environment'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a update      -d 'Update one or all installed apps'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a repair      -d 'Fix missing shared library deps in an installed app'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a config      -d 'View or change per-app configuration'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a export      -d 'Pack an app into a portable zip'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a import      -d 'Import an app from a wryayer export zip'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a snapshot    -d 'Create a hard-linked snapshot of an installed app'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a rollback    -d 'Roll an app back to a previous snapshot'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a snapshots   -d 'List snapshots for an installed app'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a tui         -d 'Launch the interactive TUI'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a dedup       -d 'Hard-link identical files across app directories'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a completions -d 'Print shell completion script to stdout'

# ── install ───────────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from install' -a '(__wryayer_pkgs)' -d 'package'
complete -c wryayer -n '__fish_seen_subcommand_from install' -l app-name  -d 'Override app directory name under ~/.wryayer/' -r
complete -c wryayer -n '__fish_seen_subcommand_from install' -l bin-name  -d 'Override launcher binary name in ~/bin/' -r
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

# ── import ────────────────────────────────────────────────────────────────────
# Re-enable file completions for import so the user can tab-complete zip paths
complete -c wryayer -n '__fish_seen_subcommand_from import' -F

# ── snapshot ──────────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from snapshot' -a '(__wryayer_apps)' -d 'installed app'

# ── rollback ──────────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from rollback' -a '(__wryayer_apps)' -d 'installed app'

# ── snapshots ─────────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from snapshots' -a '(__wryayer_apps)' -d 'installed app'

# ── dedup ─────────────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from dedup' -l verbose -s v -d 'Print every file that gets linked'

# ── completions ───────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from completions' -a 'bash fish zsh elvish powershell' -d 'shell'

# ── config ────────────────────────────────────────────────────────────────────
#
# Completion tree:
#   wryayer config <TAB>                       → app names
#   wryayer config firefox <TAB>               → tempmode | tempdelete | network | ... | share
#   wryayer config firefox tempmode <TAB>      → system | ramdisk | local | uuid
#   wryayer config firefox tempdelete <TAB>    → never | on_start | on_close
#   wryayer config firefox network <TAB>       → on | off
#   wryayer config firefox share <TAB>         → add | remove | list
#
# Level 1 (app names):   config seen  AND  no app in line yet      AND  no setting keyword
# Level 2 (settings):    config seen  AND  an app IS in line        AND  no setting keyword
# Level 3a (tempmode):   tempmode seen
# Level 3b (tempdelete): tempdelete seen
# Level 3c-f (toggles):  network | camera | microphone | audio seen
# Level 3g (share):      share seen

set -l settings tempmode tempdelete network camera microphone audio share

# Level 1 — app name
complete -c wryayer -n "__fish_seen_subcommand_from config; and not __wryayer_config_has_app; and not __fish_seen_subcommand_from $settings" -a '(__wryayer_apps)' -d 'installed app'

# Level 2 — setting name
complete -c wryayer -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $settings" -a tempmode   -d 'Set temp directory mode'
complete -c wryayer -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $settings" -a tempdelete -d 'Set temp cleanup policy (for local mode)'
complete -c wryayer -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $settings" -a network    -d 'Enable or disable network access'
complete -c wryayer -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $settings" -a camera     -d 'Enable or disable camera access'
complete -c wryayer -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $settings" -a microphone -d 'Enable or disable microphone input'
complete -c wryayer -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $settings" -a audio      -d 'Enable or disable audio output'
complete -c wryayer -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $settings" -a share      -d 'Manage shared directories'

# Level 3a — tempmode values
complete -c wryayer -n "__fish_seen_subcommand_from tempmode; and not __fish_seen_subcommand_from tempdelete $settings[3..]" -a system   -d 'Share host /tmp with all other apps (default)'
complete -c wryayer -n "__fish_seen_subcommand_from tempmode; and not __fish_seen_subcommand_from tempdelete $settings[3..]" -a ramdisk  -d 'Private in-memory tmpfs — fast, wiped on close'
complete -c wryayer -n "__fish_seen_subcommand_from tempmode; and not __fish_seen_subcommand_from tempdelete $settings[3..]" -a local    -d 'Persistent per-app dir — lifetime set by tempdelete'
complete -c wryayer -n "__fish_seen_subcommand_from tempmode; and not __fish_seen_subcommand_from tempdelete $settings[3..]" -a uuid     -d 'Per-instance UUID dir — isolated, wiped on close'

# Level 3b — tempdelete values
complete -c wryayer -n "__fish_seen_subcommand_from tempdelete; and not __fish_seen_subcommand_from tempmode $settings[3..]" -a never    -d 'Keep temp dir across restarts'
complete -c wryayer -n "__fish_seen_subcommand_from tempdelete; and not __fish_seen_subcommand_from tempmode $settings[3..]" -a on_start -d 'Wipe on launch when no other instance is running'
complete -c wryayer -n "__fish_seen_subcommand_from tempdelete; and not __fish_seen_subcommand_from tempmode $settings[3..]" -a on_close -d 'Wipe when this instance exits'

# Levels 3c-3f — on/off toggle for network, camera, microphone, audio
for _setting in network camera microphone audio
    complete -c wryayer -n "__fish_seen_subcommand_from $_setting; and not __fish_seen_subcommand_from tempmode tempdelete" -a on  -d 'Enable (default)'
    complete -c wryayer -n "__fish_seen_subcommand_from $_setting; and not __fish_seen_subcommand_from tempmode tempdelete" -a off -d 'Disable'
end

# Level 3g — share subcommands
complete -c wryayer -n "__fish_seen_subcommand_from share; and not __fish_seen_subcommand_from add remove list" -a add    -d 'Add a directory to the shared list'
complete -c wryayer -n "__fish_seen_subcommand_from share; and not __fish_seen_subcommand_from add remove list" -a remove -d 'Remove a directory from the shared list'
complete -c wryayer -n "__fish_seen_subcommand_from share; and not __fish_seen_subcommand_from add remove list" -a list   -d 'List currently shared directories'
