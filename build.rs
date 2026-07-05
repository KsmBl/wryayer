//! Compile the CPUID-spoofing LD_PRELOAD shim (csrc/cpuid_spoof.c) into a shared
//! object embedded in the wryayer binary. If no C compiler is available the
//! build still succeeds — an empty blob is emitted and the runtime treats CPUID
//! spoofing as unavailable (falling back to /proc/cpuinfo binding only).

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=csrc/cpuid_spoof.c");
    println!("cargo:rerun-if-env-changed=CC");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let so = format!("{out_dir}/libcpuidspoof.so");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());

    let built = Command::new(&cc)
        // NB: default symbol visibility — sigaction()/signal() must be exported
        // so they interpose the app's calls (that is how we keep our handler).
        .args(["-shared", "-fPIC", "-O2", "-o", &so, "csrc/cpuid_spoof.c"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !built {
        // Emit an empty file so include_bytes! still resolves; the runtime skips
        // the shim when the blob is empty.
        let _ = std::fs::write(&so, b"");
        println!(
            "cargo:warning=could not build the CPUID spoof shim (no C compiler?); \
             CPU-name spoofing via CPUID is disabled, /proc/cpuinfo binding still works"
        );
    }
}
