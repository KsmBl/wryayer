//! What an app says while it runs, kept where the launcher popup can show it.
//!
//! An app started from a terminal writes to that terminal, and the user is
//! already looking at it. One started from a menu, a desktop entry or the popup
//! has nobody reading its output: it goes to `/dev/null` or a journal nobody
//! connects back to the app. Those launches are the ones logged here.
//!
//! The capture is done by `wryayer run` on its own file descriptors 1 and 2,
//! before it does anything else: each becomes a pipe, and a thread copies what
//! arrives both to wherever the stream pointed before (so a caller piping
//! `wryayer run` still gets everything) and to `~/.wryayer/.logs/<app>.log`.
//! Everything the sandbox is started with inherits those pipes, so wryayer's
//! own messages, bwrap's and the app's all land in one place, in order.
//!
//! The logs sit inside the wryayer root rather than under `/run` or `/tmp`,
//! which every sandbox can see: one app's output is nobody else's business.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// A log is moved aside to `<app>.log.1` once it grows past this, so a chatty
/// app cannot fill the disk; one generation of history is kept.
const ROTATE_AT: u64 = 2 * 1024 * 1024;

/// Opens every run's section of the log. [`started_pid`] reads it back.
const HEADER_START: &str = "── ";
const HEADER_PID: &str = " · pid ";

pub fn log_dir() -> Option<PathBuf> {
    crate::manifest::wryayer_root().ok().map(|root| root.join(".logs"))
}

pub fn log_path(app: &str) -> Option<PathBuf> {
    Some(log_dir()?.join(format!("{app}.log")))
}

/// The capture this process set up, if any.
struct Capture {
    /// The streams' original destinations, put back by [`finish`].
    saved: [RawFd; 2],
    /// One per pump, sent when it has copied everything and stopped.
    done: Mutex<Vec<mpsc::Receiver<()>>>,
}

static CAPTURE: OnceLock<Capture> = OnceLock::new();

/// Start logging this process's output for `app`.
///
/// Does nothing when either stream is a terminal: someone is watching it, and
/// a pipe in its place would stop an interactive app from seeing a terminal.
pub fn start(app: &str) {
    if CAPTURE.get().is_some() || unsafe { libc::isatty(1) == 1 || libc::isatty(2) == 1 } {
        return;
    }
    let Some(path) = log_path(app) else { return };
    let Some(mut sink) = Sink::open(path) else { return };
    sink.write(header(app, std::process::id()).as_bytes());
    let sink = Arc::new(Mutex::new(sink));

    let mut saved = [-1; 2];
    let mut done = Vec::new();
    for (i, fd) in [1, 2].into_iter().enumerate() {
        match divert(fd, sink.clone()) {
            Some((orig, rx)) => {
                saved[i] = orig;
                done.push(rx);
            }
            None => {
                // Half a capture would split the output between two places.
                if i == 1 {
                    unsafe {
                        libc::dup2(saved[0], 1);
                        libc::close(saved[0]);
                    }
                }
                return;
            }
        }
    }
    let _ = CAPTURE.set(Capture { saved, done: Mutex::new(done) });
}

/// Point `fd` at a new pipe and start the thread emptying it. Returns a copy of
/// where `fd` pointed before, and the channel the thread reports its end on.
fn divert(fd: RawFd, sink: Arc<Mutex<Sink>>) -> Option<(RawFd, mpsc::Receiver<()>)> {
    // CLOEXEC throughout: the sandbox must inherit the pipe at 1 and 2 and
    // nothing else — not the original stream, not the pipe's read end.
    let orig = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if orig < 0 {
        return None;
    }
    let mut pipe = [0; 2];
    if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        unsafe { libc::close(orig) };
        return None;
    }
    unsafe {
        libc::dup2(pipe[1], fd);
        libc::close(pipe[1]);
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        pump(pipe[0], orig, &sink);
        unsafe { libc::close(pipe[0]) };
        let _ = tx.send(());
    });
    Some((orig, rx))
}

/// Copy everything from `from` to `to` and into the log, until the last writer
/// closes the pipe.
fn pump(from: RawFd, to: RawFd, sink: &Mutex<Sink>) {
    let mut buf = [0u8; 8192];
    let mut forward = true;
    loop {
        let n = unsafe { libc::read(from, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if n == 0 {
            break;
        }
        let chunk = &buf[..n as usize];
        // Whoever held the original stream may have gone away; the log still
        // wants the rest.
        if forward {
            forward = write_all_fd(to, chunk);
        }
        if let Ok(mut sink) = sink.lock() {
            sink.write(chunk);
        }
    }
}

fn write_all_fd(fd: RawFd, mut data: &[u8]) -> bool {
    while !data.is_empty() {
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
        data = &data[n as usize..];
    }
    true
}

/// Put the streams back and let the pumps drain what is left in the pipes.
///
/// Called before this process exits or `exec`s: the pumps are threads of it,
/// and anything still in a pipe when they die is lost. A process the app left
/// behind can keep a pipe open indefinitely, so the wait is bounded.
pub fn finish() {
    let Some(capture) = CAPTURE.get() else { return };
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    for (i, fd) in [1, 2].into_iter().enumerate() {
        if capture.saved[i] >= 0 {
            unsafe { libc::dup2(capture.saved[i], fd) };
        }
    }
    if let Ok(mut done) = capture.done.lock() {
        for rx in done.drain(..) {
            let _ = rx.recv_timeout(Duration::from_millis(300));
        }
    }
}

/// Delete an app's log, rotated generation included.
///
/// For an app kept in its own encrypted container: once that is locked again,
/// its output must not stay readable next to it.
pub fn discard(app: &str) {
    if let Some(path) = log_path(app) {
        let _ = std::fs::remove_file(rotated(&path));
        let _ = std::fs::remove_file(path);
    }
}

fn header(app: &str, pid: u32) -> String {
    format!(
        "\n{HEADER_START}{app} · started {}{HEADER_PID}{pid} ──\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    )
}

fn rotated(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

/// The log file, appended to and rotated as it grows.
struct Sink {
    path: PathBuf,
    file: Option<File>,
    size: u64,
}

impl Sink {
    fn open(path: PathBuf) -> Option<Sink> {
        let dir = path.parent()?;
        std::fs::create_dir_all(dir).ok()?;
        let _ = std::fs::set_permissions(
            dir,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        );
        let mut sink = Sink { path, file: None, size: 0 };
        if std::fs::metadata(&sink.path).map(|m| m.len() >= ROTATE_AT).unwrap_or(false) {
            let _ = std::fs::rename(&sink.path, rotated(&sink.path));
        }
        sink.reopen();
        sink.file.is_some().then_some(sink)
    }

    fn reopen(&mut self) {
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)
            .ok();
        self.size = self.file.as_ref().and_then(|f| f.metadata().ok()).map_or(0, |m| m.len());
    }

    fn write(&mut self, data: &[u8]) {
        if self.size >= ROTATE_AT {
            let _ = std::fs::rename(&self.path, rotated(&self.path));
            self.reopen();
        }
        if let Some(file) = self.file.as_mut() {
            if file.write_all(data).is_ok() {
                self.size += data.len() as u64;
            }
        }
    }
}

// ── reading ─────────────────────────────────────────────────────────────────

/// The last `max_bytes` of an app's log as display-ready lines, oldest first.
///
/// A cut through the middle of a line drops that line rather than showing its
/// tail as if it were whole.
pub fn tail(app: &str, max_bytes: u64) -> Option<Vec<String>> {
    let path = log_path(app)?;
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut raw = Vec::with_capacity((len - start) as usize);
    file.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    Some(lines.into_iter().map(crate::child_output::sanitize_line).collect())
}

/// Size and modification time of an app's log — enough to tell whether it has
/// changed since it was last read.
pub fn stamp(app: &str) -> Option<(u64, std::time::SystemTime)> {
    let meta = std::fs::metadata(log_path(app)?).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

/// The pid of the `wryayer run` that wrote the last section of `lines`.
pub fn started_pid(lines: &[String]) -> Option<u32> {
    lines.iter().rev().find_map(|line| {
        let rest = line.strip_prefix(HEADER_START)?;
        let (_, pid) = rest.rsplit_once(HEADER_PID)?;
        pid.trim_end_matches(" ──").trim().parse().ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_names_the_process_that_wrote_it() {
        let lines: Vec<String> = header("firefox", 4242)
            .lines()
            .map(str::to_string)
            .chain(["some output".to_string()])
            .collect();
        assert_eq!(started_pid(&lines), Some(4242));
    }

    #[test]
    fn the_latest_run_is_the_one_reported() {
        let text = format!("{}old output\n{}new output\n", header("a", 1), header("a", 2));
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        assert_eq!(started_pid(&lines), Some(2));
    }

    #[test]
    fn output_that_merely_looks_like_a_header_is_not_one() {
        let lines = vec!["── not a header ──".to_string(), "pid 7".to_string()];
        assert_eq!(started_pid(&lines), None);
    }

    #[test]
    fn a_tail_starts_at_a_whole_line_and_is_cleaned_for_display() {
        let _home = crate::test_support::test_home();
        let path = log_path("app").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "first line\nsecond \u{1b}[31mred\u{1b}[0m\nthird\n").unwrap();

        assert_eq!(
            tail("app", 1024).unwrap(),
            vec!["first line", "second red", "third"]
        );
        // Cutting into "second …" drops it rather than showing half of it.
        assert_eq!(tail("app", 8).unwrap(), vec!["third"]);
    }

    #[test]
    fn a_full_log_is_moved_aside_rather_than_grown() {
        let _home = crate::test_support::test_home();
        let path = log_path("big").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![b'x'; ROTATE_AT as usize]).unwrap();

        let mut sink = Sink::open(path.clone()).unwrap();
        sink.write(b"fresh\n");

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh\n");
        assert_eq!(std::fs::metadata(rotated(&path)).unwrap().len(), ROTATE_AT);

        discard("big");
        assert!(!path.exists() && !rotated(&path).exists());
    }
}
