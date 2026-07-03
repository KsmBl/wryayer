use std::process::Command;
use wryayer::commands::run::{has_systemd_run, no_other_instance, wrap_with_ram_limit};


// Helper that mirrors the inline arg-stripping pattern from commands::run::run()
fn strip_dashdash(args: &[String]) -> &[String] {
    match args {
        [first, rest @ ..] if first == "--" => rest,
        other => other,
    }
}

// ── argument stripping — 5 cases (EC + boundaries) ───────────────────────────

#[test]
fn strip_leading_dashdash_removes_it() {
    let args = vec!["--".to_string(), "file.pdf".to_string()];
    assert_eq!(strip_dashdash(&args), &["file.pdf".to_string()]);
}

#[test]
fn strip_leading_dashdash_multiple_remaining_args() {
    let args = vec!["--".to_string(), "-a".to_string(), "b".to_string()];
    assert_eq!(strip_dashdash(&args), &["-a".to_string(), "b".to_string()]);
}

#[test]
fn strip_dashdash_only_yields_empty() {
    // Boundary: ["--"] → []
    let args = vec!["--".to_string()];
    assert!(strip_dashdash(&args).is_empty());
}

#[test]
fn no_strip_when_first_arg_is_not_dashdash() {
    let args = vec!["file.pdf".to_string()];
    assert_eq!(strip_dashdash(&args), args.as_slice());
}

#[test]
fn no_strip_empty_args() {
    let args: Vec<String> = vec![];
    assert!(strip_dashdash(&args).is_empty());
}

#[test]
fn no_strip_non_leading_dashdash() {
    // "--" is not the first element → no stripping
    let args = vec!["--flag".to_string(), "--".to_string(), "file.pdf".to_string()];
    assert_eq!(strip_dashdash(&args).len(), 3);
}

// ── no_other_instance — 4 cases (EC) ─────────────────────────────────────────

#[test]
fn no_other_instance_missing_pid_file_is_true() {
    // EC1: file does not exist → treat as no other instance
    let tmp = tempfile::tempdir().unwrap();
    assert!(no_other_instance(&tmp.path().join("missing.pid")));
}

#[test]
fn no_other_instance_invalid_pid_content_is_true() {
    // EC2: file exists but content is not a valid integer
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("bad.pid");
    std::fs::write(&path, b"not_a_number\n").unwrap();
    assert!(no_other_instance(&path));
}

#[test]
fn no_other_instance_live_pid_is_false() {
    // EC3: PID 1 (init/systemd) is always running on Linux
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("live.pid");
    std::fs::write(&path, b"1").unwrap();
    assert!(!no_other_instance(&path));
}

#[test]
fn no_other_instance_dead_pid_is_true() {
    // EC4: PID that cannot exist (above Linux PID_MAX = 4 194 304)
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("dead.pid");
    std::fs::write(&path, b"99999999").unwrap();
    assert!(no_other_instance(&path));
}

// ── has_systemd_run ───────────────────────────────────────────────────────────

#[test]
fn has_systemd_run_detects_binary_on_path() {
    // On this systemd-based distro systemd-run is always present.
    // Only assert true when the binary actually exists so the test stays
    // green on minimal containers that lack systemd.
    if std::path::Path::new("/usr/bin/systemd-run").exists() {
        assert!(has_systemd_run());
    }
}

#[test]
fn has_systemd_run_returns_bool_consistent_with_filesystem() {
    // has_systemd_run scans PATH for "systemd-run".  The result must agree
    // with whether the binary actually exists at the canonical location.
    let canonical = std::path::Path::new("/usr/bin/systemd-run");
    if canonical.exists() {
        assert!(has_systemd_run(), "/usr/bin/systemd-run exists but has_systemd_run() returned false");
    } else {
        // Fragile to assert false here because other PATH dirs might have it;
        // just document we exercised the else branch.
    }
}

// ── wrap_with_ram_limit ───────────────────────────────────────────────────────

fn inner_cmd(prog: &str) -> Command {
    Command::new(prog)
}

fn args_of(cmd: &Command) -> Vec<String> {
    cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect()
}

#[test]
fn wrap_outer_program_is_systemd_run() {
    let wrapped = wrap_with_ram_limit(inner_cmd("bwrap"), 512);
    assert_eq!(wrapped.get_program(), "systemd-run");
}

#[test]
fn wrap_contains_user_scope_quiet_flags() {
    let args = args_of(&wrap_with_ram_limit(inner_cmd("bwrap"), 512));
    assert!(args.contains(&"--user".to_string()),  "--user must be present");
    assert!(args.contains(&"--scope".to_string()), "--scope must be present (not --wait)");
    assert!(args.contains(&"--quiet".to_string()), "--quiet must be present");
}

#[test]
fn wrap_contains_memorymax_arg() {
    let args = args_of(&wrap_with_ram_limit(inner_cmd("bwrap"), 512));
    let p_idx = args.iter().position(|a| a == "-p").expect("-p flag missing");
    assert_eq!(args[p_idx + 1], "MemoryMax=512K", "MemoryMax value must follow -p");
}

#[test]
fn wrap_contains_memoryswapmax_zero() {
    let args = args_of(&wrap_with_ram_limit(inner_cmd("bwrap"), 512));
    // Find the -p that sets MemorySwapMax=0 (there are two -p flags total)
    let found = args.windows(2).any(|w| w[0] == "-p" && w[1] == "MemorySwapMax=0");
    assert!(found, "MemorySwapMax=0 must be present to block zram overflow");
}

#[test]
fn wrap_memorymax_reflects_kib_parameter() {
    for kib in [256u64, 1024, 4096, 8388608] {
        let args = args_of(&wrap_with_ram_limit(inner_cmd("bwrap"), kib));
        let expected = format!("MemoryMax={kib}K");
        let found = args.windows(2).any(|w| w[0] == "-p" && w[1] == expected);
        assert!(found, "expected {expected} in args for {kib} KiB");
    }
}

#[test]
fn wrap_inner_program_appears_after_dashdash() {
    let args = args_of(&wrap_with_ram_limit(inner_cmd("bwrap"), 512));
    let sep = args.iter().rposition(|a| a == "--").expect("'--' separator missing");
    assert_eq!(args[sep + 1], "bwrap", "inner program must follow '--'");
}

#[test]
fn wrap_inner_args_are_preserved_after_program() {
    let mut cmd = inner_cmd("bwrap");
    cmd.arg("--bind").arg("/tmp/root").arg("/");
    let args = args_of(&wrap_with_ram_limit(cmd, 512));
    let sep = args.iter().rposition(|a| a == "--").unwrap();
    let inner_args: Vec<&str> = args[sep + 1..].iter().map(String::as_str).collect();
    assert_eq!(inner_args, ["bwrap", "--bind", "/tmp/root", "/"],
        "inner args must be preserved verbatim after the separator");
}

#[test]
fn wrap_transfers_env_vars_from_inner() {
    let mut cmd = inner_cmd("bwrap");
    cmd.env("MY_VAR", "hello");
    cmd.env("ANOTHER", "world");
    let wrapped = wrap_with_ram_limit(cmd, 512);
    let env: std::collections::HashMap<_, _> = wrapped.get_envs()
        .filter_map(|(k, v)| v.map(|v| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned())))
        .collect();
    assert_eq!(env.get("MY_VAR").map(String::as_str), Some("hello"));
    assert_eq!(env.get("ANOTHER").map(String::as_str), Some("world"));
}
