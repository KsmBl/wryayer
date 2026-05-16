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
set -l cmds install remove list run update repair config backup import help
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a install -d 'Install a package in an isolated directory'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a remove  -d 'Remove an installed app and its launchers'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a list    -d 'List all installed apps'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a run     -d 'Run an installed app in its isolated environment'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a update  -d 'Update one or all installed apps'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a repair  -d 'Fix missing shared library deps in an installed app'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a config  -d 'View or change per-app configuration'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a backup  -d 'Create a zip backup of an installed app'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a import  -d 'Import an app from a wryayer backup zip'
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a help    -d 'Print help'

# ── install ───────────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from install' -a '(__wryayer_pkgs)' -d 'package'
complete -c wryayer -n '__fish_seen_subcommand_from install' -l app-name -d 'Override app directory name under ~/.wryayer/' -r
complete -c wryayer -n '__fish_seen_subcommand_from install' -l bin-name -d 'Override launcher binary name in ~/bin/' -r

# ── remove ────────────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from remove' -a '(__wryayer_apps)' -d 'installed app'

# ── run ───────────────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from run' -a '(__wryayer_apps)' -d 'installed app'

# ── update ────────────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from update' -a '(__wryayer_apps)' -d 'installed app (omit to update all)'
complete -c wryayer -n '__fish_seen_subcommand_from update' -l check -d 'Show available updates without installing'

# ── repair ────────────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from repair' -a '(__wryayer_apps)' -d 'installed app'

# ── backup ────────────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from backup' -a '(__wryayer_apps)' -d 'installed app'
complete -c wryayer -n '__fish_seen_subcommand_from backup' -l output -s o -d 'Output zip file path' -r

# ── import ────────────────────────────────────────────────────────────────────
# Re-enable file completions for import so the user can tab-complete zip paths
complete -c wryayer -n '__fish_seen_subcommand_from import' -F

# ── config ────────────────────────────────────────────────────────────────────
#
# Completion tree:
#   wryayer config <TAB>                       → app names
#   wryayer config firefox <TAB>               → tempmode | tempdelete | network
#   wryayer config firefox tempmode <TAB>      → system | ramdisk | local | uuid
#   wryayer config firefox tempdelete <TAB>    → never | on_start | on_close
#   wryayer config firefox network <TAB>       → on | off
#
# Level 1 (app names):   config seen  AND  no app in line yet      AND  no setting keyword
# Level 2 (settings):    config seen  AND  an app IS in line        AND  no setting keyword
# Level 3a (tempmode):   tempmode seen
# Level 3b (tempdelete): tempdelete seen
# Level 3c (network):    network seen

set -l settings tempmode tempdelete network

# Level 1 — app name
complete -c wryayer -n "__fish_seen_subcommand_from config; and not __wryayer_config_has_app; and not __fish_seen_subcommand_from $settings" -a '(__wryayer_apps)' -d 'installed app'

# Level 2 — setting name
complete -c wryayer -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $settings" -a tempmode   -d 'Set temp directory mode'
complete -c wryayer -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $settings" -a tempdelete -d 'Set temp cleanup policy (for local mode)'
complete -c wryayer -n "__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from $settings" -a network    -d 'Enable or disable network access'

# Level 3a — tempmode values
complete -c wryayer -n '__fish_seen_subcommand_from tempmode; and not __fish_seen_subcommand_from tempdelete network' -a system   -d 'Share host /tmp with all other apps (default)'
complete -c wryayer -n '__fish_seen_subcommand_from tempmode; and not __fish_seen_subcommand_from tempdelete network' -a ramdisk  -d 'Private in-memory tmpfs — fast, wiped on close'
complete -c wryayer -n '__fish_seen_subcommand_from tempmode; and not __fish_seen_subcommand_from tempdelete network' -a local    -d 'Persistent per-app dir — lifetime set by tempdelete'
complete -c wryayer -n '__fish_seen_subcommand_from tempmode; and not __fish_seen_subcommand_from tempdelete network' -a uuid     -d 'Per-instance UUID dir — isolated, wiped on close'

# Level 3b — tempdelete values
complete -c wryayer -n '__fish_seen_subcommand_from tempdelete; and not __fish_seen_subcommand_from tempmode network' -a never    -d 'Keep temp dir across restarts'
complete -c wryayer -n '__fish_seen_subcommand_from tempdelete; and not __fish_seen_subcommand_from tempmode network' -a on_start -d 'Wipe on launch when no other instance is running'
complete -c wryayer -n '__fish_seen_subcommand_from tempdelete; and not __fish_seen_subcommand_from tempmode network' -a on_close -d 'Wipe when this instance exits'

# Level 3c — network values
complete -c wryayer -n '__fish_seen_subcommand_from network; and not __fish_seen_subcommand_from tempmode tempdelete' -a on  -d 'Allow internet access (default)'
complete -c wryayer -n '__fish_seen_subcommand_from network; and not __fish_seen_subcommand_from tempmode tempdelete' -a off -d 'Block all network access'
