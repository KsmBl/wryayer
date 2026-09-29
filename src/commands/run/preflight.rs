//! A last look at the sandbox's mounts against the host as it is right now.
//!
//! bwrap refuses the whole launch when a single bind does not fit — a source
//! that has gone, or a file to be mounted where the host now has a directory
//! — and says so in one line before exiting, so the app simply never
//! appears. The binds come from what the host looked like when each rule was
//! written: a camera mask that bound `/dev/null` over every `/dev/media*`
//! broke the day an update added a `/dev/media/` directory beside
//! `/dev/media0`. The host keeps changing under the apps, which is the point
//! of wryayer, so nothing here trusts a rule to stay right.
//!
//! Every launch passes its final argument list through [`check`], which
//! follows the mounts in order — the app tree at `/`, `/dev`, `/run`, … —
//! to find the host path each destination really lands on, and repairs what
//! bwrap would reject:
//!
//! * a mask (`/dev/null` bound over something) that meets a directory hides
//!   it under an empty tmpfs instead — a mask is never dropped, since running
//!   an app with its camera or microphone exposed is worse than not running;
//! * a bind whose source has vanished becomes its `-try` form, which bwrap
//!   skips;
//! * any other bind whose source and destination disagree about being a
//!   directory is left out, with a warning naming it.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// What a sandbox path is backed by.
#[derive(Debug, Clone)]
enum Backing {
    /// A host directory, bound there.
    Host(PathBuf),
    /// A tmpfs, `--proc`, `--dev` or similar: nothing on the host to check.
    Opaque,
}

#[derive(Debug, Default)]
pub(super) struct Checked {
    pub args: Vec<OsString>,
    /// What was changed and why, for the user.
    pub notes: Vec<String>,
}

/// Options by how many values follow them.
fn arity(option: &str) -> Option<usize> {
    Some(match option {
        "--bind" | "--bind-try" | "--ro-bind" | "--ro-bind-try" | "--dev-bind" | "--dev-bind-try"
        | "--symlink" | "--setenv" | "--chmod" | "--file" | "--bind-data" | "--ro-bind-data" => 2,
        "--tmpfs" | "--proc" | "--dev" | "--dir" | "--mqueue" | "--unsetenv" | "--chdir"
        | "--hostname" | "--remount-ro" | "--info-fd" | "--json-status-fd" | "--sync-fd"
        | "--block-fd" | "--userns-block-fd" | "--seccomp" | "--add-seccomp-fd" | "--uid"
        | "--gid" | "--lock-file" | "--perms" | "--size" | "--cap-add" | "--cap-drop"
        | "--userns" | "--userns2" | "--pidns" | "--exec-label" | "--file-label" | "--argv0"
        | "--args" | "--level-prefix" => 1,
        o if o.starts_with("--unshare-")
            || matches!(
                o,
                "--share-net" | "--die-with-parent" | "--new-session" | "--as-pid-1"
                    | "--clearenv" | "--disable-userns" | "--assert-userns-disabled"
            ) =>
        {
            0
        }
        _ => return None,
    })
}

/// Check and repair a bwrap argument list (everything after the program).
pub(super) fn check(args: &[OsString]) -> Checked {
    let mut out = Checked::default();
    let mut mounts: Vec<(PathBuf, Backing)> = Vec::new();
    let mut i = 0;

    while i < args.len() {
        let Some(option) = args[i].to_str() else {
            // Not ours to understand: leave the rest exactly as it was.
            out.args.extend_from_slice(&args[i..]);
            break;
        };
        if option == "--" || !option.starts_with("--") {
            out.args.extend_from_slice(&args[i..]);
            break;
        }
        let Some(n) = arity(option) else {
            out.args.extend_from_slice(&args[i..]);
            break;
        };
        let values = &args[(i + 1).min(args.len())..(i + 1 + n).min(args.len())];
        i += 1 + n;
        if values.len() < n {
            out.args.push(option.into());
            out.args.extend_from_slice(values);
            continue;
        }

        match option {
            "--bind" | "--bind-try" | "--ro-bind" | "--ro-bind-try" | "--dev-bind" | "--dev-bind-try" => {
                bind(option, &values[0], &values[1], &mut mounts, &mut out);
            }
            "--tmpfs" | "--proc" | "--dev" | "--mqueue" => {
                mounts.push((PathBuf::from(&values[0]), Backing::Opaque));
                out.args.push(option.into());
                out.args.extend_from_slice(values);
            }
            _ => {
                out.args.push(option.into());
                out.args.extend_from_slice(values);
            }
        }
    }
    out
}

fn bind(option: &str, source: &OsStr, dest: &OsStr, mounts: &mut Vec<(PathBuf, Backing)>, out: &mut Checked) {
    let keep = |out: &mut Checked, option: &str| {
        out.args.push(option.into());
        out.args.push(source.into());
        out.args.push(dest.into());
    };
    let tolerant = option.ends_with("-try");
    let is_mask = source == OsStr::new("/dev/null");
    let dest_path = PathBuf::from(dest);

    let Ok(source_meta) = fs::metadata(source) else {
        if tolerant {
            keep(out, option);
        } else {
            out.notes.push(format!(
                "{} is not on this system — left out of the sandbox",
                Path::new(source).display()
            ));
            keep(out, &format!("{option}-try"));
        }
        return;
    };

    // Where the destination really is, if a host directory backs it. A
    // symlink there is resolved by bwrap inside the sandbox, against paths
    // this check cannot follow, so it is trusted as it stands.
    let dest_kind = host_path(mounts, &dest_path)
        .and_then(|p| fs::symlink_metadata(p).ok())
        .filter(|m| !m.file_type().is_symlink())
        .map(|m| m.is_dir());

    match (source_meta.is_dir(), dest_kind) {
        (false, Some(true)) if is_mask => {
            // An empty tmpfs hides a directory the way /dev/null hides a file.
            out.args.push("--tmpfs".into());
            out.args.push(dest.into());
            mounts.push((dest_path, Backing::Opaque));
        }
        (src_dir, Some(dest_dir)) if src_dir != dest_dir => {
            out.notes.push(format!(
                "{} is a {} on this system but {} is not — left out of the sandbox",
                dest_path.display(),
                if dest_dir { "directory" } else { "file" },
                Path::new(source).display(),
            ));
        }
        (src_dir, _) => {
            keep(out, option);
            if src_dir {
                let real = fs::canonicalize(source).unwrap_or_else(|_| PathBuf::from(source));
                mounts.push((dest_path, Backing::Host(real)));
            } else {
                mounts.push((dest_path, Backing::Opaque));
            }
        }
    }
}

/// The host path a sandbox path lands on: under the most recent mount whose
/// destination contains it.
fn host_path(mounts: &[(PathBuf, Backing)], path: &Path) -> Option<PathBuf> {
    let (dest, backing) = mounts
        .iter()
        .rev()
        .filter(|(dest, _)| path.starts_with(dest))
        .max_by_key(|(dest, _)| dest.components().count())?;
    match backing {
        Backing::Host(source) => {
            let rest: PathBuf = path
                .strip_prefix(dest)
                .ok()?
                .components()
                .filter(|c| matches!(c, Component::Normal(_)))
                .collect();
            Some(source.join(rest))
        }
        Backing::Opaque => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn strs(args: &[OsString]) -> Vec<String> {
        args.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    /// A fake host: an app tree and a /dev with a device beside a directory
    /// of the same prefix, as udev now makes for /dev/media*.
    fn host() -> (tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("app");
        let dev = dir.path().join("dev");
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::create_dir_all(dev.join("media/by-path")).unwrap();
        fs::write(dev.join("media0"), b"").unwrap();
        fs::write(root.join("etc/hostname"), b"x").unwrap();
        let (root, dev) = (root.to_string_lossy().into_owned(), dev.to_string_lossy().into_owned());
        (dir, root, dev)
    }

    #[test]
    fn a_mask_over_a_directory_becomes_an_empty_tmpfs() {
        let (_dir, root, dev) = host();
        let checked = check(&os(&[
            "--bind", &root, "/",
            "--dev-bind", &dev, "/dev",
            "--bind", "/dev/null", "/dev/media0",
            "--bind", "/dev/null", "/dev/media",
            "--", "/usr/bin/thunderbird",
        ]));
        let args = strs(&checked.args);
        assert!(args.windows(3).any(|w| w == ["--bind", "/dev/null", "/dev/media0"]), "{args:?}");
        assert!(args.windows(2).any(|w| w == ["--tmpfs", "/dev/media"]), "{args:?}");
        assert!(!args.windows(3).any(|w| w == ["--bind", "/dev/null", "/dev/media"]), "{args:?}");
        assert_eq!(args.last().unwrap(), "/usr/bin/thunderbird");
        assert!(checked.notes.is_empty(), "masking is not worth a warning: {:?}", checked.notes);
    }

    #[test]
    fn a_vanished_source_becomes_optional_rather_than_fatal() {
        let (_dir, root, _dev) = host();
        let checked = check(&os(&["--bind", &root, "/", "--ro-bind", "/no/such/path", "/opt/x", "--", "app"]));
        let args = strs(&checked.args);
        assert!(args.windows(3).any(|w| w == ["--ro-bind-try", "/no/such/path", "/opt/x"]), "{args:?}");
        assert_eq!(checked.notes.len(), 1);
    }

    #[test]
    fn a_file_bound_over_a_directory_is_left_out_with_a_warning() {
        let (dir, root, _dev) = host();
        let file = dir.path().join("spoofed");
        fs::write(&file, b"x").unwrap();
        let file = file.to_string_lossy().into_owned();
        // /etc is a directory in the app tree.
        let checked = check(&os(&["--bind", &root, "/", "--ro-bind", &file, "/etc", "--", "app"]));
        let args = strs(&checked.args);
        assert!(!args.contains(&file), "{args:?}");
        assert!(checked.notes[0].contains("/etc is a directory"), "{:?}", checked.notes);
    }

    #[test]
    fn binds_that_fit_pass_through_untouched() {
        let (dir, root, dev) = host();
        let file = dir.path().join("hostname");
        fs::write(&file, b"y").unwrap();
        let file = file.to_string_lossy().into_owned();
        let args = os(&[
            "--bind", &root, "/",
            "--dev-bind", &dev, "/dev",
            "--proc", "/proc",
            "--ro-bind", &file, "/etc/hostname",
            "--ro-bind-try", "/nowhere", "/nowhere",
            "--setenv", "A", "B",
            "--unshare-net",
            "--", "app", "--bind", "not/an/option/of/ours",
        ]);
        let checked = check(&args);
        assert_eq!(checked.args, args);
        assert!(checked.notes.is_empty());
    }

    #[test]
    fn a_destination_under_a_tmpfs_is_not_checked_against_the_host() {
        let (_dir, root, _dev) = host();
        let args = os(&["--bind", &root, "/", "--tmpfs", "/etc", "--ro-bind", "/dev/null", "/etc", "--", "app"]);
        // /etc is a tmpfs by then; what the app tree has there no longer counts.
        let checked = check(&args);
        assert_eq!(checked.args, args);
    }

    #[test]
    fn an_option_it_does_not_know_ends_the_check_without_changing_anything() {
        let args = os(&["--some-future-option", "--bind", "/dev/null", "/tmp", "--", "app"]);
        assert_eq!(check(&args).args, args);
    }
}
