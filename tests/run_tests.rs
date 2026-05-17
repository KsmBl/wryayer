use wryayer::commands::run::no_other_instance;

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
