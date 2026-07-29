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
}
