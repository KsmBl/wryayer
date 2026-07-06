//! Host-side portal listener for cross-container app binding.
//!
//! `wryayer run <app>` spawns this (hidden) subcommand when the app's config
//! lists `bound_apps`. It listens on an AF_UNIX socket that is reachable from
//! inside the sandbox (it lives under the app's isolated XDG_RUNTIME_DIR, which
//! is bind-mounted through `/run`). Each connection carries a NUL-delimited
//! record — the target app name followed by its arguments — written by the
//! static portal client (`csrc/portal_client.c`) that stands in for each bound
//! app inside the sandbox. For every request naming an allowed app it launches
//! `wryayer run <app> -- <args>` on the host, i.e. in that app's own container.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::io::Read;
use std::os::unix::net::{UnixListener, UnixStream};

pub fn run(socket: &str, allowed_csv: &str) -> Result<()> {
    let allowed: HashSet<String> = allowed_csv
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    // A stale socket from a previous run would make bind() fail.
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("failed to bind portal socket {socket}"))?;
    // Only the owning user may connect.
    let _ = std::fs::set_permissions(
        socket,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
    );

    // Signal readiness so the launcher doesn't race the socket's creation.
    let _ = std::fs::write(format!("{socket}.ready"), b"");

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let allowed = allowed.clone();
        std::thread::spawn(move || handle(stream, &allowed));
    }
    Ok(())
}

fn handle(mut stream: UnixStream, allowed: &HashSet<String>) {
    let mut buf = Vec::new();
    if stream.read_to_end(&mut buf).is_err() {
        return;
    }

    // NUL-delimited fields: app name, then each argument. The client terminates
    // every field with a NUL, so drop the trailing empty element it leaves.
    let mut fields: Vec<String> = buf
        .split(|&b| b == 0)
        .map(|f| String::from_utf8_lossy(f).into_owned())
        .collect();
    if fields.last().is_some_and(|s| s.is_empty()) {
        fields.pop();
    }
    if fields.is_empty() {
        return;
    }
    let app = fields.remove(0);
    if !allowed.contains(&app) {
        eprintln!("wryayer portal: ignoring request for unbound app '{app}'");
        return;
    }
    let args = fields;

    let Ok(exe) = std::env::current_exe() else { return };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("run").arg(&app).arg("--").args(&args);
    match cmd.spawn() {
        Ok(mut child) => {
            let _ = child.wait();
        }
        Err(e) => eprintln!("wryayer portal: failed to launch {app}: {e}"),
    }
}
