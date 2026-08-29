//! Turning a child process's output into something safe to display.
//!
//! Both front-ends stream `wryayer` subprocesses into a view — the TUI into a
//! ratatui `Paragraph`, the GUI into a GTK `TextView` — and neither is a
//! terminal emulator. Bytes a child emitted *for* a terminal are therefore
//! either acted on by the real terminal underneath (the TUI's case, which
//! wrecks its layout) or drawn as mojibake (the GUI's). Neither is wanted, and
//! the answer is the same for both, so it lives here rather than in either.

/// Make one line of a child's output safe to display.
///
/// Two things arrive routinely:
///
/// * **Carriage returns.** `veracrypt --create` draws its progress by rewriting
///   one line with `\r`, and never emits a newline until it finishes — so a
///   whole container creation arrives as a single line hundreds of characters
///   long, carrying a `\r` before every update. Drawn verbatim, each one sends
///   the cursor back to column 0 in the middle of a frame. Only the text after
///   the last `\r` is current, which is exactly what a terminal would have been
///   showing, so that is what is kept.
/// * **Escape sequences and other control bytes**, from a package's postinstall
///   script, `mkfs`, or anything else wryayer shells out to. Colour changes
///   outlive the line they were on; a stray bell or NUL is pure noise.
///
/// Applied where output enters the log rather than at draw time, so the stored
/// log is clean for everything that reads it — including the TUI's `PROGRESS`
/// and `PROMPT_*` protocol lines, which are plain ASCII and pass through
/// byte-identical.
pub fn sanitize_line(raw: &str) -> String {
    // Everything before the final carriage return has already been overwritten.
    let current = raw.rsplit('\r').next().unwrap_or(raw);

    let mut out = String::with_capacity(current.len());
    let mut chars = current.chars();
    while let Some(c) = chars.next() {
        match c {
            // ESC starts a sequence that ends at its first byte in @..~ — with
            // '[' excepted, since the CSI introducer falls in that range too.
            '\u{1b}' => {
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) && next != '[' {
                        break;
                    }
                }
            }
            // Tabs are legitimate in package output, but ratatui does not
            // expand them and the terminal would.
            '\t' => out.push_str("    "),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {}
            c => out.push(c),
        }
    }
    // veracrypt pads its progress with trailing spaces to erase the previous,
    // longer update. Nothing is being erased here.
    out.truncate(out.trim_end().len());
    out
}

/// A protocol line a child emitted to tell the front-end something it cannot be
/// told any other way.
///
/// A `wryayer` subprocess has no terminal of its own — both front-ends stream
/// it into a view — so where the CLI would stop and ask, the child prints one
/// of these and exits, leaving the front-end to put the question to the user
/// and re-run the command with the answer folded in. Recognising them is the
/// same job in the TUI and the GUI, so it is done once, here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildLine {
    /// `PROGRESS <done>/<total>` — units are the caller's (bytes, files).
    Progress(u64, u64),
    /// `PROMPT_LAUNCHER_CHOICE:<pkg>:<bin>,<bin>` — the package installed
    /// nothing that looked like a launcher; `bins` is what it did install.
    NoLauncher { pkg: String, bins: Vec<String> },
    /// `PROMPT_OUTDATED_PACKAGES:<pkg>` — a download 404'd because the local
    /// package databases are behind the mirror.
    OutdatedPackages { pkg: String },
    /// `PROMPT_BUILD_DEPS:<pkg>:<dep>,<dep>` — a source build needs packages
    /// installed on the host first, and installing them needs root that this
    /// child has no way to ask for.
    BuildDeps { pkg: String, deps: Vec<String> },
}

/// Read one line as a protocol line, or None when it is ordinary output.
pub fn classify(line: &str) -> Option<ChildLine> {
    if let Some((done, total)) = parse_progress(line) {
        return Some(ChildLine::Progress(done, total));
    }
    if let Some(rest) = line.strip_prefix("PROMPT_LAUNCHER_CHOICE:") {
        let (pkg, bins) = rest.split_once(':')?;
        let bins = if bins.is_empty() {
            Vec::new()
        } else {
            bins.split(',').map(str::to_string).collect()
        };
        return Some(ChildLine::NoLauncher { pkg: pkg.to_string(), bins });
    }
    if let Some(pkg) = line.strip_prefix("PROMPT_OUTDATED_PACKAGES:") {
        return Some(ChildLine::OutdatedPackages { pkg: pkg.to_string() });
    }
    if let Some(rest) = line.strip_prefix("PROMPT_BUILD_DEPS:") {
        let (pkg, deps) = rest.split_once(':')?;
        let deps = if deps.is_empty() {
            Vec::new()
        } else {
            deps.split(',').map(str::to_string).collect()
        };
        return Some(ChildLine::BuildDeps { pkg: pkg.to_string(), deps });
    }
    None
}

/// Parse a `PROGRESS <done>/<total>` line into its two counts.
pub fn parse_progress(line: &str) -> Option<(u64, u64)> {
    let rest = line.strip_prefix("PROGRESS ")?;
    let (a, b) = rest.split_once('/')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_veracrypt_progress_line_collapses_to_its_last_update() {
        // Verbatim shape of `veracrypt --text --create` output: no newline
        // until the very end, one \r before every update. Drawn as-is, every
        // \r sends the cursor to column 0 mid-frame and the TUI comes apart.
        let raw = "\rDone:   0.000%  Speed:            Left:          \
                   \rDone:  50.000%  Speed: 3.3 MiB/s  Left: 4 s         \
                   \rDone: 100.000%  Speed: 1.7 MiB/s  Left: 0 s         ";
        let out = sanitize_line(raw);

        assert!(!out.contains('\r'), "carriage returns survived: {out:?}");
        assert_eq!(out, "Done: 100.000%  Speed: 1.7 MiB/s  Left: 0 s");
    }

    #[test]
    fn colour_escapes_do_not_outlive_their_line() {
        // A colour left unterminated recolours everything drawn after it.
        let out = sanitize_line("\u{1b}[32mgreen\u{1b}[0m and \u{1b}[1;31mred");
        assert_eq!(out, "green and red");
    }

    #[test]
    fn stray_control_bytes_are_dropped() {
        let out = sanitize_line("bell\u{7}nul\u{0}del\u{7f}done");
        assert_eq!(out, "bellnuldeldone");
    }

    #[test]
    fn tabs_become_spaces_rather_than_terminal_tab_stops() {
        assert_eq!(sanitize_line("a\tb"), "a    b");
    }

    #[test]
    fn ordinary_output_is_left_exactly_as_it_was() {
        // The common case by far — this runs on every line of every install.
        for line in [
            "installing gtk3",
            "  [3/12] looking up libGL.so.1",
            "error: no such package",
            "Encrypting 'bash2': 2.2 GB of files -> 3.6 GB container",
        ] {
            assert_eq!(sanitize_line(line), line);
        }
    }

    #[test]
    fn the_protocol_lines_the_log_reader_parses_survive_intact() {
        // These are matched by prefix at the receiving end; mangling one would
        // silently break the progress bar or a launcher prompt.
        for line in [
            "PROGRESS 1024/4096",
            "PROMPT_LAUNCHER_CHOICE:vim:vim,vimdiff",
            "PROMPT_OUTDATED_PACKAGES:gtk3 qt5-base",
        ] {
            assert_eq!(sanitize_line(line), line);
        }
    }

    #[test]
    fn a_line_of_pure_control_bytes_becomes_empty_not_garbage() {
        assert_eq!(sanitize_line("\u{1b}[2J\u{1b}[H"), "");
    }

    #[test]
    fn a_progress_line_is_read_as_progress() {
        assert_eq!(classify("PROGRESS 42/100"), Some(ChildLine::Progress(42, 100)));
    }

    #[test]
    fn a_launcher_prompt_carries_the_binaries_that_were_found() {
        assert_eq!(
            classify("PROMPT_LAUNCHER_CHOICE:vim:vim,vimdiff"),
            Some(ChildLine::NoLauncher {
                pkg: "vim".to_string(),
                bins: vec!["vim".to_string(), "vimdiff".to_string()],
            })
        );
        // A package that installed nothing at all still names itself.
        assert_eq!(
            classify("PROMPT_LAUNCHER_CHOICE:foo:"),
            Some(ChildLine::NoLauncher { pkg: "foo".to_string(), bins: Vec::new() })
        );
    }

    #[test]
    fn an_outdated_prompt_names_the_package() {
        assert_eq!(
            classify("PROMPT_OUTDATED_PACKAGES:gtk3"),
            Some(ChildLine::OutdatedPackages { pkg: "gtk3".to_string() })
        );
    }

    #[test]
    fn ordinary_output_is_not_a_protocol_line() {
        for line in ["installing gtk3", "PROGRESS abc/100", "PROMPT_LAUNCHER_CHOICE:novalue", ""] {
            assert_eq!(classify(line), None, "{line}");
        }
    }

    #[test]
    fn a_build_deps_line_carries_every_dependency() {
        assert_eq!(
            classify("PROMPT_BUILD_DEPS:ayugram-desktop-git:cmake,ninja,extra-cmake-modules"),
            Some(ChildLine::BuildDeps {
                pkg: "ayugram-desktop-git".into(),
                deps: vec!["cmake".into(), "ninja".into(), "extra-cmake-modules".into()],
            })
        );
    }

    #[test]
    fn a_build_deps_line_with_no_dependencies_is_still_a_prompt() {
        // Never emitted in practice, but an empty list must not read as a
        // single dependency named "".
        assert_eq!(
            classify("PROMPT_BUILD_DEPS:pkg:"),
            Some(ChildLine::BuildDeps { pkg: "pkg".into(), deps: vec![] })
        );
    }

    #[test]
    fn ordinary_output_mentioning_the_marker_word_is_left_alone() {
        assert_eq!(classify("  Installing makedepends for pkg: cmake"), None);
    }
}
