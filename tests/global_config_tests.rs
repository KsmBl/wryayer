// Tests intentionally build a Default config and tweak individual fields.
#![allow(clippy::field_reassign_with_default)]

use std::sync::Mutex;
use wryayer::config::{
    read_global_config, write_global_config, AppConfig, TempMode,
};

// Tests in this binary mutate the process-global $HOME and run in parallel
// threads by default; without this lock two tests racing on HOME read each
// other's temp config. Same guard the other HOME-touching test files use.
static HOME_LOCK: Mutex<()> = Mutex::new(());

fn with_temp_home<F: FnOnce(&std::path::Path)>(f: F) {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let old = std::env::var_os("HOME");
    std::env::set_var("HOME", tmp.path());
    f(tmp.path());
    match old {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }
}

// ── read_global_config falls back gracefully ─────────────────────────────────

#[test]
fn read_global_config_returns_default_when_no_file() {
    with_temp_home(|_root| {
        let cfg = read_global_config();
        let default = AppConfig::default();
        assert_eq!(cfg.network, default.network);
        assert_eq!(cfg.temp_mode, default.temp_mode);
        assert_eq!(cfg.ram_limit, default.ram_limit);
    });
}

// ── write_global_config + read_global_config round-trip ─────────────────────

#[test]
fn write_then_read_global_config_round_trips() {
    with_temp_home(|_root| {
        let mut cfg = AppConfig::default();
        cfg.network = false;
        cfg.temp_mode = TempMode::Ramdisk;
        cfg.ram_limit = Some(2048);

        write_global_config(&cfg).unwrap();
        let loaded = read_global_config();

        assert!(!loaded.network);
        assert_eq!(loaded.temp_mode, TempMode::Ramdisk);
        assert_eq!(loaded.ram_limit, Some(2048));
    });
}
