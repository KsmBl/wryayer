//! Whether this process is allowed to ask a human for a password.
//!
//! Two things in wryayer read a password straight from `/dev/tty` rather than
//! from stdin: `rpassword`, which is how the master and container passwords are
//! entered, and `sudo`, which is how every root operation authenticates. Both
//! open the controlling terminal themselves, so redirecting a child's stdin
//! does *not* stop them.
//!
//! That matters because the TUI owns the terminal. It runs in raw mode on the
//! alternate screen, and it is reading the same keyboard. A child that prompts
//! there writes its prompt into a frame that is repainted fifty milliseconds
//! later, turns echo off on a terminal the TUI has already configured, and then
//! competes with the TUI for every keystroke. The user sees an operation that
//! has simply stopped, with no way to tell it anything. The GUI is worse still:
//! its children have no terminal at all, or the one it happened to be launched
//! from, and their output is discarded.
//!
//! So both front-ends collect every password up front — in a TUI overlay or a
//! GTK dialog — and hand it to the child on stdin. This module is the backstop
//! that makes that the *only* way it can happen: in a process that has no
//! terminal of its own to ask on, `sudo` is run with `-n` and a password prompt
//! is refused outright with a message that names the front-end route. A missed
//! path then fails visibly in the operation log instead of hanging invisibly.

use anyhow::{anyhow, Error};
use std::process::Command;

/// Set in every child a front-end spawns, to forbid terminal prompts even if
/// that child ends up with a terminal on stdin after all.
pub const NO_TTY_ENV: &str = "WRYAYER_NO_TTY";

/// Whether this process may prompt on the terminal.
///
/// Two questions, in order. A front-end child is forbidden outright, because it
/// *has* a terminal — the one the front-end is drawing on — and that is exactly
/// the terminal it must not touch. Anything else may prompt if there is a
/// controlling terminal to prompt on, which is the same question `sudo` and
/// `rpassword` ask by opening `/dev/tty`, asked before they do rather than
/// after.
///
/// Deliberately not a test of stdin: `wryayer install … | tee log` has a pipe
/// on stdin and a perfectly good terminal to ask on, and both of those tools
/// would have used it.
pub fn allowed() -> bool {
    std::env::var_os(NO_TTY_ENV).is_none() && has_terminal()
}

/// Whether `/dev/tty` can be opened — i.e. whether this process has a
/// controlling terminal at all.
fn has_terminal() -> bool {
    std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty").is_ok()
}

/// Mark `cmd` as a process that must not prompt.
pub fn forbid_prompts(cmd: &mut Command) -> &mut Command {
    cmd.env(NO_TTY_ENV, "1")
}

/// Forbid prompts for the rest of *this* process, and for anything it spawns.
///
/// Called by a front-end as it takes the terminal. Marking each child is not
/// quite enough on its own: a front-end also calls into `commands` and
/// `secrets` directly, in its own process, where a prompt would land on the
/// screen it is drawing just the same.
pub fn forbid_here() {
    std::env::set_var(NO_TTY_ENV, "1");
}

/// Allow prompts again for one child, whatever this process decided.
///
/// For the TUI's inline launch, which is the one case where a child *should*
/// prompt: the TUI leaves raw mode and the alternate screen first, so the
/// terminal is a plain one again and a password typed on it is visible.
pub fn allow_prompts(cmd: &mut Command) -> &mut Command {
    cmd.env_remove(NO_TTY_ENV)
}

/// The error for a password that cannot be asked for here.
///
/// `what` names the password, so the message says which one to go and provide
/// rather than leaving the user to guess between the master password, a
/// container's, and root's.
pub fn refused(what: &str) -> Error {
    anyhow!(
        "no terminal to type {what} on.\n\
         Run this command in a terminal, where it can ask.\n\
         From the TUI or the desktop app, the password is asked for before the \
         operation starts — reaching this means one was needed that nothing asked for, \
         which is a bug worth reporting."
    )
}

/// A `sudo` that will never prompt when this process cannot be prompted at.
///
/// `-n` makes sudo fail with "a password is required" instead of opening
/// `/dev/tty`, which turns a silent hang into a line in the operation log. In a
/// terminal it is left off, so the plain CLI keeps asking as it always has.
///
/// stdin is deliberately left alone: [`crate::veracrypt`] pipes a *volume*
/// password into `sudo veracrypt`, and that is a different secret from sudo's
/// own.
pub fn sudo() -> Command {
    sudo_when(allowed())
}

/// [`sudo`] for a known answer, so the decision can be tested without a
/// terminal to depend on.
fn sudo_when(may_prompt: bool) -> Command {
    let mut cmd = Command::new("sudo");
    if !may_prompt {
        cmd.arg("-n");
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn a_process_that_cannot_be_asked_gets_a_non_interactive_sudo() {
        // -n makes sudo fail with "a password is required" instead of opening
        // /dev/tty, which is the difference between a line in the operation log
        // and an invisible prompt the TUI paints over.
        assert_eq!(args_of(&sudo_when(false)), ["-n"]);
    }

    #[test]
    fn a_process_with_a_terminal_gets_the_ordinary_sudo() {
        assert!(args_of(&sudo_when(true)).is_empty());
    }

    #[test]
    fn a_front_end_child_is_forbidden_whatever_terminal_it_has() {
        // The marker is what the front-ends set, and it wins on its own: their
        // children do have a controlling terminal — the one being drawn on.
        crate::test_support::with_temp_home(|_| {
            std::env::set_var(NO_TTY_ENV, "1");
            let forbidden = !allowed();
            std::env::remove_var(NO_TTY_ENV);
            assert!(forbidden);
        });
    }

    #[test]
    fn children_are_marked() {
        let mut cmd = Command::new("true");
        forbid_prompts(&mut cmd);
        let set: Vec<_> = cmd.get_envs().collect();
        assert!(
            set.iter().any(|(k, v)| *k == NO_TTY_ENV && v.is_some()),
            "the marker is not in the child's environment: {set:?}"
        );
    }

    /// Every `.rs` file in the crate except this one, path and contents.
    ///
    /// This file is skipped because it is the wrapper: it names the very
    /// patterns it is looking for, in the code that makes them safe and in the
    /// tests below.
    fn sources() -> Vec<(String, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
            for entry in std::fs::read_dir(dir).expect("src/ is readable").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs")
                    && path.file_name().is_some_and(|n| n != "prompt.rs")
                {
                    let text = std::fs::read_to_string(&path).expect("a source file reads");
                    out.push((path.display().to_string(), text));
                }
            }
        }
        let mut out = Vec::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut out);
        assert!(!out.is_empty(), "found no sources to scan");
        out
    }

    /// No `sudo` anywhere may be left able to open `/dev/tty` on its own.
    ///
    /// This is the property the whole module exists for, and it is one grep
    /// away from being broken by a new call site — so it is checked here rather
    /// than trusted. A raw `Command::new("sudo")` is allowed only where the
    /// very next lines make it non-interactive (`-n`), feed it a password
    /// (`-S`), or where prompting has just been established as safe.
    #[test]
    fn no_sudo_is_left_able_to_prompt() {
        let mut offenders = Vec::new();
        for (path, text) in sources() {
            let lines: Vec<&str> = text.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if !line.contains(r#"Command::new("sudo")"#) {
                    continue;
                }
                let from = i.saturating_sub(2);
                let to = (i + 4).min(lines.len());
                let window = lines[from..to].join("\n");
                let vouched = window.contains(r#""-n""#)
                    || window.contains(r#""-S""#)
                    || window.contains("may_prompt")
                    || window.contains("prompt::allowed()");
                if !vouched {
                    offenders.push(format!("{path}:{}: {}", i + 1, line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "sudo that could prompt on a terminal a front-end is drawing on:\n{}\n\n             Use crate::prompt::sudo() instead.",
            offenders.join("\n")
        );
    }

    /// The terminal password reader is reached through one guarded door.
    ///
    /// `rpassword` reads `/dev/tty` directly, so a second call site anywhere
    /// would bypass [`allowed`] entirely.
    #[test]
    fn rpassword_is_only_called_behind_the_guard() {
        let callers: Vec<String> = sources()
            .into_iter()
            .filter(|(_, text)| text.contains("rpassword::"))
            .map(|(path, _)| path)
            .collect();
        assert_eq!(
            callers.len(),
            1,
            "rpassword should only be reached through secrets::prompt_password, found: {callers:?}"
        );
        assert!(callers[0].ends_with("secrets.rs"), "{callers:?}");
    }

    #[test]
    fn one_child_can_be_let_through() {
        // The TUI's inline launch restores the terminal before spawning, so
        // that child is allowed to ask even though its parent is not.
        let mut cmd = Command::new("true");
        forbid_prompts(&mut cmd);
        allow_prompts(&mut cmd);
        let removed = cmd.get_envs().any(|(k, v)| k == NO_TTY_ENV && v.is_none());
        assert!(removed, "the marker is not cleared for the child");
    }

    #[test]
    fn the_refusal_names_the_password() {
        let msg = format!("{:#}", refused("the master password"));
        assert!(msg.contains("the master password"), "{msg}");
        assert!(msg.contains("terminal"), "{msg}");
        assert!(msg.contains("TUI"), "the way out is not named: {msg}");
    }
}
