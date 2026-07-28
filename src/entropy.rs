//! Multi-source random password generator.
//!
//! Passwords are drawn from an entropy pool that mixes the kernel CSPRNG with a
//! spread of live machine state: `/dev/random`, `/dev/urandom`, every hardware
//! temperature sensor, the current mouse position, memory usage, scheduler
//! counters and the high-resolution clock.
//!
//! ## What the extra sources actually buy
//!
//! To be honest about the security model: `/dev/urandom` alone is already a
//! cryptographically secure source, and nothing here can improve on it. The
//! additional sources are folded in through SHA-512, which means they can only
//! ever *add* to the pool — a hash of (strong ‖ weak) is no weaker than a hash
//! of (strong) alone. Their real value is defence against a specific failure
//! mode: a system whose CSPRNG is broken or unseeded (a freshly imaged VM, a
//! container with a cloned entropy pool, a kernel RNG bug). In that scenario
//! sensor noise, cursor position and cycle-level timing are the only things
//! that differ between two otherwise identical machines.
//!
//! Sources that are unavailable simply contribute nothing; the pool is never
//! weaker than `/dev/urandom`.

use anyhow::{Context, Result};
use sha2::{Digest, Sha512};
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use zeroize::Zeroizing;

/// Characters a generated password may contain.
///
/// Split by class so the generator can guarantee at least one of each — many
/// sites and tools reject passwords that happen to miss a class, and VeraCrypt
/// itself accepts the full printable ASCII range. Ambiguous glyphs are kept:
/// these passwords are stored and pasted, not read aloud.
const LOWER: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
const UPPER: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const DIGITS: &[u8] = b"0123456789";
/// Quotes, backslashes and backticks are excluded: a container password ends up
/// in shell-adjacent contexts often enough that avoiding them is worth the
/// negligible loss of ~0.1 bits per character.
const SYMBOLS: &[u8] = b"!#$%&()*+,-./:;<=>?@[]^_{|}~";

/// Default length for a generated container password.
///
/// The alphabet above is 90 characters (26 + 26 + 10 + 28), so each one carries
/// log2(90) ≈ 6.49 bits: 32 of them is ≈ 207 bits — far past any brute-force
/// concern, and comfortably below VeraCrypt's 128-character limit.
pub const DEFAULT_LENGTH: usize = 32;

/// Which sources contributed to a generated password, for display to the user.
#[derive(Debug, Default, Clone)]
pub struct SourceReport {
    pub names: Vec<String>,
}

impl SourceReport {
    fn add(&mut self, name: impl Into<String>) {
        self.names.push(name.into());
    }

    /// Human-readable one-line summary, e.g. `/dev/urandom, 6 temp sensors, …`.
    pub fn summary(&self) -> String {
        if self.names.is_empty() {
            return "no sources".to_string();
        }
        self.names.join(", ")
    }
}

/// Accumulates entropy from many sources into a SHA-512 state.
struct Pool {
    hasher: Sha512,
    report: SourceReport,
}

impl Pool {
    fn new() -> Self {
        Self {
            hasher: Sha512::new(),
            report: SourceReport::default(),
        }
    }

    /// Fold raw bytes into the pool. Length-prefixed so that two different
    /// source splits can't produce the same concatenated input.
    fn mix(&mut self, data: &[u8]) {
        self.hasher.update((data.len() as u64).to_le_bytes());
        self.hasher.update(data);
    }

    /// Read up to `n` bytes from a character device and mix them in.
    fn mix_device(&mut self, path: &str, n: usize, label: &str) {
        if let Some(buf) = read_bytes(path, n) {
            if !buf.is_empty() {
                self.mix(&buf);
                self.report.add(label);
            }
        }
    }

    /// Mix the contents of a text file (sysfs/procfs), if readable.
    fn mix_file(&mut self, path: &Path) -> bool {
        match std::fs::read(path) {
            Ok(b) if !b.is_empty() => {
                self.mix(&b);
                true
            }
            _ => false,
        }
    }
}

/// Read at most `n` bytes from `path` without blocking forever.
///
/// `/dev/random` can block on a system whose pool is not yet initialised, and
/// `/dev/input/mice` blocks until the mouse actually moves — neither may stall
/// password generation, so both are opened non-blocking and a short read (or no
/// read at all) is treated as success.
fn read_bytes(path: &str, n: usize) -> Option<Vec<u8>> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let mut buf = vec![0u8; n];
    match f.read(&mut buf) {
        Ok(got) => {
            buf.truncate(got);
            Some(buf)
        }
        // EAGAIN/EWOULDBLOCK: nothing available right now, which is fine.
        Err(_) => Some(Vec::new()),
    }
}

/// Every hardware temperature reading the kernel exposes.
///
/// The low-order digits of a CPU/GPU/NVMe temperature are genuinely noisy —
/// they track airflow, load and ambient conditions at millidegree resolution —
/// and differ between two machines booted from the same image.
fn mix_temperatures(pool: &mut Pool) {
    let mut count = 0usize;

    // hwmon: /sys/class/hwmon/hwmon*/temp*_input
    if let Ok(entries) = std::fs::read_dir("/sys/class/hwmon") {
        for hwmon in entries.flatten() {
            let Ok(files) = std::fs::read_dir(hwmon.path()) else { continue };
            for f in files.flatten() {
                let name = f.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("temp") && name.ends_with("_input") && pool.mix_file(&f.path()) {
                    count += 1;
                }
            }
        }
    }

    // thermal zones: /sys/class/thermal/thermal_zone*/temp
    if let Ok(entries) = std::fs::read_dir("/sys/class/thermal") {
        for zone in entries.flatten() {
            if !zone.file_name().to_string_lossy().starts_with("thermal_zone") {
                continue;
            }
            if pool.mix_file(&zone.path().join("temp")) {
                count += 1;
            }
        }
    }

    if count > 0 {
        pool.report.add(format!("{count} temperature sensors"));
    }
}

/// Current pointer position, as reported by whatever the session provides.
///
/// Returned as free-form bytes because only their variability matters. Tries
/// the compositor/X server first, then falls back to raw movement deltas from
/// the mouse device (readable only when the user is in the `input` group, which
/// is why it is a fallback rather than the primary path).
fn mouse_position() -> Option<(Vec<u8>, &'static str)> {
    use std::process::{Command, Stdio};

    // Hyprland and Sway expose the cursor through their own IPC clients.
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        for (bin, args) in [("hyprctl", &["cursorpos"][..])] {
            if let Ok(out) = Command::new(bin)
                .args(args)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
            {
                if out.status.success() && !out.stdout.is_empty() {
                    return Some((out.stdout, "mouse position"));
                }
            }
        }
    }

    // X11 (also covers XWayland sessions where DISPLAY is set).
    if std::env::var_os("DISPLAY").is_some() {
        if let Ok(out) = Command::new("xdotool")
            .args(["getmouselocation", "--shell"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        {
            if out.status.success() && !out.stdout.is_empty() {
                return Some((out.stdout, "mouse position"));
            }
        }
    }

    // Raw pointer deltas. Wayland deliberately offers no way to query the
    // cursor position (it would let any client watch the pointer), so outside
    // the compositor-specific paths above this is the only route to real mouse
    // data. Waiting briefly catches the movement of a user who is still holding
    // the mouse — which is the common case right after they trigger a
    // generation — instead of giving up on an empty non-blocking read.
    if let Some(b) = read_bytes_waiting("/dev/input/mice", 32, 50) {
        if !b.is_empty() {
            return Some((b, "mouse movement"));
        }
    }
    None
}

/// Read up to `n` bytes from `path`, waiting at most `timeout_ms` for the first
/// byte to arrive.
///
/// Used for input devices, which only produce data when the user actually moves
/// something. Returns an empty vec (not an error) when nothing arrives in time,
/// so a still mouse never blocks password generation.
fn read_bytes_waiting(path: &str, n: usize, timeout_ms: i32) -> Option<Vec<u8>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .ok()?;

    let mut pfd = libc::pollfd {
        fd: f.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: pfd holds a valid fd owned by `f` for the whole call.
    let ready = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if ready <= 0 {
        return Some(Vec::new());
    }

    let mut buf = vec![0u8; n];
    match f.read(&mut buf) {
        Ok(got) => {
            buf.truncate(got);
            Some(buf)
        }
        Err(_) => Some(Vec::new()),
    }
}

/// Build an entropy pool from every source that is available on this machine.
fn collect_pool() -> Pool {
    let mut pool = Pool::new();

    // Kernel CSPRNG — the load-bearing source. /dev/random is read
    // non-blocking, so on a fully seeded system (the normal case) it returns
    // immediately, and on an unseeded one it contributes nothing rather than
    // hanging.
    pool.mix_device("/dev/urandom", 64, "/dev/urandom");
    pool.mix_device("/dev/random", 32, "/dev/random");

    mix_temperatures(&mut pool);

    if let Some((bytes, label)) = mouse_position() {
        pool.mix(&bytes);
        pool.report.add(label);
    }

    // Memory usage: MemFree/MemAvailable/Dirty shift constantly with real work.
    if pool.mix_file(Path::new("/proc/meminfo")) {
        pool.report.add("RAM usage");
    }

    // Scheduler and interrupt counters — context switches and per-device IRQ
    // totals since boot, which no two machines share.
    if pool.mix_file(Path::new("/proc/stat")) {
        pool.report.add("scheduler counters");
    }
    // Per-device interrupt totals since boot. These move with every keystroke
    // and mouse event, so they carry input timing even when the pointer itself
    // can't be read.
    if pool.mix_file(Path::new("/proc/interrupts")) {
        pool.report.add("interrupt counters");
    }

    // High-resolution clock, down to the nanosecond, plus this process's pid.
    // Milliseconds alone would be far too coarse to matter; nanoseconds capture
    // the exact moment the user pressed the key.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    pool.mix(&nanos.to_le_bytes());
    pool.mix(&std::process::id().to_le_bytes());
    pool.report.add("clock (ns)");

    // A second clock read: the gap between the two samples reflects how long
    // the collection above actually took, which depends on live system load.
    let nanos2 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    pool.mix(&nanos2.to_le_bytes());

    pool
}

/// An unbounded keystream derived from the finalised entropy pool.
///
/// SHA-512 in counter mode: block `i` is `SHA512(seed ‖ i)`. This is the
/// standard hash-based DRBG shape — output blocks reveal nothing about the seed
/// or about each other.
struct KeyStream {
    seed: Zeroizing<Vec<u8>>,
    counter: u64,
    buf: Zeroizing<Vec<u8>>,
    pos: usize,
}

impl KeyStream {
    fn new(seed: Zeroizing<Vec<u8>>) -> Self {
        Self {
            seed,
            counter: 0,
            buf: Zeroizing::new(Vec::new()),
            pos: 0,
        }
    }

    fn next_byte(&mut self) -> u8 {
        if self.pos >= self.buf.len() {
            let mut h = Sha512::new();
            h.update(&*self.seed);
            h.update(self.counter.to_le_bytes());
            self.counter += 1;
            self.buf = Zeroizing::new(h.finalize().to_vec());
            self.pos = 0;
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        b
    }

    /// A uniformly distributed value in `0..n`, free of modulo bias.
    ///
    /// Bytes landing in the final partial window of 0..=255 are discarded and
    /// redrawn, so every outcome is exactly equally likely.
    fn below(&mut self, n: usize) -> usize {
        debug_assert!(n > 0 && n <= 256);
        let limit = 256 - (256 % n);
        loop {
            let b = self.next_byte() as usize;
            if b < limit {
                return b % n;
            }
        }
    }
}

/// Generate a random password of `len` characters, plus a report of which
/// entropy sources contributed.
///
/// The result always contains at least one lowercase letter, one uppercase
/// letter, one digit and one symbol.
pub fn generate_password(len: usize) -> Result<(Zeroizing<String>, SourceReport)> {
    // Four characters are pinned to guarantee one of each class.
    if len < 4 {
        anyhow::bail!("password length must be at least 4 (got {len})");
    }

    let pool = collect_pool();
    let report = pool.report.clone();
    let seed = Zeroizing::new(pool.hasher.finalize().to_vec());
    let mut stream = KeyStream::new(seed);

    let classes: [&[u8]; 4] = [LOWER, UPPER, DIGITS, SYMBOLS];
    let all: Vec<u8> = classes.concat();

    // One character from each class, then fill the rest from the full alphabet.
    let mut chars: Vec<u8> = Vec::with_capacity(len);
    for class in classes {
        let i = stream.below(class.len());
        chars.push(class[i]);
    }
    while chars.len() < len {
        let i = stream.below(all.len());
        chars.push(all[i]);
    }

    // Fisher-Yates, so the guaranteed characters aren't always in front.
    for i in (1..chars.len()).rev() {
        let j = stream.below(i + 1);
        chars.swap(i, j);
    }

    let password = String::from_utf8(chars)
        .context("generated password was not valid UTF-8")?;
    Ok((Zeroizing::new(password), report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn generates_a_password_of_the_requested_length() {
        for len in [4, 8, 32, 64, 128] {
            let (pw, _) = generate_password(len).unwrap();
            assert_eq!(pw.chars().count(), len, "wrong length for {len}");
        }
    }

    #[test]
    fn rejects_lengths_that_cannot_hold_every_class() {
        for len in [0, 1, 2, 3] {
            assert!(generate_password(len).is_err(), "len {len} should fail");
        }
    }

    #[test]
    fn always_includes_every_character_class() {
        // Repeat: a class is only "guaranteed" if the shuffle preserves it.
        for _ in 0..50 {
            let (pw, _) = generate_password(8).unwrap();
            let b = pw.as_bytes();
            assert!(b.iter().any(|c| LOWER.contains(c)), "no lowercase in {}", pw.as_str());
            assert!(b.iter().any(|c| UPPER.contains(c)), "no uppercase in {}", pw.as_str());
            assert!(b.iter().any(|c| DIGITS.contains(c)), "no digit in {}", pw.as_str());
            assert!(b.iter().any(|c| SYMBOLS.contains(c)), "no symbol in {}", pw.as_str());
        }
    }

    #[test]
    fn only_uses_characters_from_the_declared_alphabet() {
        let allowed: HashSet<u8> = [LOWER, UPPER, DIGITS, SYMBOLS].concat().into_iter().collect();
        let (pw, _) = generate_password(256).unwrap();
        for c in pw.as_bytes() {
            assert!(allowed.contains(c), "unexpected character {:?}", *c as char);
        }
    }

    #[test]
    fn successive_passwords_differ() {
        let mut seen = HashSet::new();
        for _ in 0..100 {
            let (pw, _) = generate_password(DEFAULT_LENGTH).unwrap();
            assert!(seen.insert(pw.to_string()), "generated a duplicate password");
        }
    }

    #[test]
    fn reports_at_least_the_kernel_and_clock_sources() {
        let (_, report) = generate_password(16).unwrap();
        let s = report.summary();
        assert!(s.contains("/dev/urandom"), "missing urandom in '{s}'");
        assert!(s.contains("clock"), "missing clock in '{s}'");
    }

    #[test]
    fn keystream_below_is_unbiased_across_the_range() {
        // Every value in 0..n must be reachable; a modulo-bias bug typically
        // starves the tail of the range.
        let mut stream = KeyStream::new(Zeroizing::new(vec![7u8; 64]));
        for n in [3usize, 7, 10, 26, 89] {
            let mut seen = HashSet::new();
            for _ in 0..(n * 200) {
                let v = stream.below(n);
                assert!(v < n, "below({n}) returned {v}");
                seen.insert(v);
            }
            assert_eq!(seen.len(), n, "below({n}) never produced every value");
        }
    }

    #[test]
    fn keystream_is_deterministic_for_a_fixed_seed() {
        let a: Vec<u8> = {
            let mut s = KeyStream::new(Zeroizing::new(vec![1, 2, 3]));
            (0..200).map(|_| s.next_byte()).collect()
        };
        let b: Vec<u8> = {
            let mut s = KeyStream::new(Zeroizing::new(vec![1, 2, 3]));
            (0..200).map(|_| s.next_byte()).collect()
        };
        assert_eq!(a, b);

        // A different seed must give a different stream.
        let c: Vec<u8> = {
            let mut s = KeyStream::new(Zeroizing::new(vec![1, 2, 4]));
            (0..200).map(|_| s.next_byte()).collect()
        };
        assert_ne!(a, c);
    }

    #[test]
    fn keystream_crosses_block_boundaries_cleanly() {
        // SHA-512 blocks are 64 bytes; drawing past several boundaries must not
        // repeat a block.
        let mut s = KeyStream::new(Zeroizing::new(vec![9u8; 32]));
        let bytes: Vec<u8> = (0..256).map(|_| s.next_byte()).collect();
        assert_ne!(bytes[0..64], bytes[64..128]);
        assert_ne!(bytes[64..128], bytes[128..192]);
    }
}
