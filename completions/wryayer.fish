# wryayer fish shell completions

# ── Helpers ───────────────────────────────────────────────────────────────────

function __wryayer_apps
    wryayer list 2>/dev/null | awk 'NR>2 && NF>0 {print $1}'
end

function __wryayer_pkgs
    pacman -Ssq 2>/dev/null
end

# True when at least one installed app name appears in the current command line.
# Used to distinguish "wryayer config <TAB>" from "wryayer config firefox <TAB>".
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
complete -c wryayer -n 'not __fish_seen_subcommand_from install remove list run update repair config help' -a install -d 'Install a package in an isolated directory'
complete -c wryayer -n 'not __fish_seen_subcommand_from install remove list run update repair config help' -a remove  -d 'Remove an installed app and its launchers'
complete -c wryayer -n 'not __fish_seen_subcommand_from install remove list run update repair config help' -a list    -d 'List all installed apps'
complete -c wryayer -n 'not __fish_seen_subcommand_from install remove list run update repair config help' -a run     -d 'Run an installed app in its isolated environment'
complete -c wryayer -n 'not __fish_seen_subcommand_from install remove list run update repair config help' -a update  -d 'Update one or all installed apps'
complete -c wryayer -n 'not __fish_seen_subcommand_from install remove list run update repair config help' -a repair  -d 'Fix missing shared library deps in an installed app'
complete -c wryayer -n 'not __fish_seen_subcommand_from install remove list run update repair config help' -a config  -d 'View or change per-app configuration'
complete -c wryayer -n 'not __fish_seen_subcommand_from install remove list run update repair config help' -a help    -d 'Print help'

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

# ── repair ────────────────────────────────────────────────────────────────────
complete -c wryayer -n '__fish_seen_subcommand_from repair' -a '(__wryayer_apps)' -d 'installed app'

# ── config ────────────────────────────────────────────────────────────────────
#
# Completion tree:
#   wryayer config <TAB>                     → app names
#   wryayer config firefox <TAB>             → tempmode | tempdelete
#   wryayer config firefox tempmode <TAB>    → system | ramdisk | local | uuid
#   wryayer config firefox tempdelete <TAB>  → never | on_start | on_close
#
# Gate logic (no token counting — uses seen-subcommand checks only):
#
#   Level 1 (app names):   config seen  AND  no app in line yet         AND  no setting keyword
#   Level 2 (settings):    config seen  AND  an app IS in line          AND  no setting keyword
#   Level 3a (tempmode):   tempmode seen (implies config + app already present)
#   Level 3b (tempdelete): tempdelete seen

# Level 1 — app name
complete -c wryayer -n '__fish_seen_subcommand_from config; and not __wryayer_config_has_app; and not __fish_seen_subcommand_from tempmode tempdelete' -a '(__wryayer_apps)' -d 'installed app'

# Level 2 — setting name
complete -c wryayer -n '__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from tempmode tempdelete' -a tempmode   -d 'Set temp directory mode'
complete -c wryayer -n '__fish_seen_subcommand_from config; and __wryayer_config_has_app; and not __fish_seen_subcommand_from tempmode tempdelete' -a tempdelete -d 'Set temp cleanup policy (for local mode)'

# Level 3a — tempmode values
complete -c wryayer -n '__fish_seen_subcommand_from tempmode; and not __fish_seen_subcommand_from tempdelete' -a system   -d 'Share host /tmp with all other apps (default)'
complete -c wryayer -n '__fish_seen_subcommand_from tempmode; and not __fish_seen_subcommand_from tempdelete' -a ramdisk  -d 'Private in-memory tmpfs — fast, wiped on close'
complete -c wryayer -n '__fish_seen_subcommand_from tempmode; and not __fish_seen_subcommand_from tempdelete' -a local    -d 'Persistent per-app dir — lifetime set by tempdelete'
complete -c wryayer -n '__fish_seen_subcommand_from tempmode; and not __fish_seen_subcommand_from tempdelete' -a uuid     -d 'Per-instance UUID dir — isolated, wiped on close'

# Level 3b — tempdelete values
complete -c wryayer -n '__fish_seen_subcommand_from tempdelete; and not __fish_seen_subcommand_from tempmode' -a never    -d 'Keep temp dir across restarts'
complete -c wryayer -n '__fish_seen_subcommand_from tempdelete; and not __fish_seen_subcommand_from tempmode' -a on_start -d 'Wipe on launch when no other instance is running'
complete -c wryayer -n '__fish_seen_subcommand_from tempdelete; and not __fish_seen_subcommand_from tempmode' -a on_close -d 'Wipe when this instance exits'
