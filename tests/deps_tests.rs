use wryayer::package::deps::*;

// ── strip_version_constraint — 7 operator ECs + no-constraint + empty ────────

#[test]
fn strip_no_constraint_unchanged() {
    assert_eq!(strip_version_constraint("firefox"), "firefox");
}

#[test]
fn strip_ge_operator() {
    assert_eq!(strip_version_constraint("glibc>=2.36"), "glibc");
}

#[test]
fn strip_gt_operator() {
    assert_eq!(strip_version_constraint("glibc>2.36"), "glibc");
}

#[test]
fn strip_le_operator() {
    assert_eq!(strip_version_constraint("glibc<=2.36"), "glibc");
}

#[test]
fn strip_lt_operator() {
    assert_eq!(strip_version_constraint("glibc<2.36"), "glibc");
}

#[test]
fn strip_eq_operator() {
    assert_eq!(strip_version_constraint("glibc=2.36"), "glibc");
}

#[test]
fn strip_ne_operator() {
    assert_eq!(strip_version_constraint("glibc!=2.36"), "glibc");
}

#[test]
fn strip_empty_string() {
    // Boundary: split("").next() on empty string returns Some("")
    assert_eq!(strip_version_constraint(""), "");
}

// ── is_soname_dep — 5 equivalence classes ────────────────────────────────────

#[test]
fn soname_plain_so_suffix() {
    // EC1: name ends exactly with ".so"
    assert!(is_soname_dep("libasound.so"));
    assert!(is_soname_dep("libz.so"));
}

#[test]
fn soname_with_version_eq() {
    // EC2: contains ".so="
    assert!(is_soname_dep("libreadline.so=8-64"));
    assert!(is_soname_dep("libpng.so=16"));
}

#[test]
fn soname_with_dotted_version() {
    // EC3: contains ".so."
    assert!(is_soname_dep("libz.so.2"));
    assert!(is_soname_dep("libz.so.2.1.0"));
}

#[test]
fn not_soname_regular_packages() {
    // EC4: no ".so" substring at all
    assert!(!is_soname_dep("firefox"));
    assert!(!is_soname_dep("glibc"));
    assert!(!is_soname_dep("base-devel"));
}

#[test]
fn not_soname_empty_string() {
    // Boundary: empty string
    assert!(!is_soname_dep(""));
}

#[test]
fn not_soname_sock_extension() {
    // EC5: has ".so" substring but none of the three sub-patterns
    // ".sock" contains ".so" but is not ".so", ".so=", or ".so."
    assert!(!is_soname_dep("control.sock"));
}

// ── parse_pacman_field — 3 ECs ────────────────────────────────────────────────

#[test]
fn parse_field_found_returns_trimmed_value() {
    let output = "Name           : firefox\nVersion        : 130.0-1\n";
    assert_eq!(parse_pacman_field(output, "Version"), Some("130.0-1".to_string()));
}

#[test]
fn parse_field_not_found_returns_none() {
    let output = "Name           : firefox\n";
    assert_eq!(parse_pacman_field(output, "Version"), None);
}

#[test]
fn parse_field_empty_input_returns_none() {
    assert_eq!(parse_pacman_field("", "Version"), None);
}

#[test]
fn parse_field_prefix_does_not_match_different_key() {
    // "Depends On" must not match a line starting with "Make Depends"
    let output = "Make Depends   : cmake\nDepends On     : dep1\n";
    assert_eq!(parse_pacman_field(output, "Depends On"), Some("dep1".to_string()));
}

// ── parse_pacman_depends — 5 ECs ─────────────────────────────────────────────

#[test]
fn parse_depends_none_returns_empty() {
    // EC1: explicit "None" value
    assert!(parse_pacman_depends("Depends On     : None\n").is_empty());
}

#[test]
fn parse_depends_no_field_returns_empty() {
    // EC2: no "Depends On" line present
    assert!(parse_pacman_depends("Name           : firefox\nVersion : 1.0\n").is_empty());
}

#[test]
fn parse_depends_single_dep() {
    // EC3: one package, no constraint
    assert_eq!(parse_pacman_depends("Depends On     : glibc\n"), vec!["glibc"]);
}

#[test]
fn parse_depends_multiple_deps() {
    // EC4: multiple space-separated packages
    let deps = parse_pacman_depends("Depends On     : glibc gcc-libs libpng\n");
    assert_eq!(deps, vec!["glibc", "gcc-libs", "libpng"]);
}

#[test]
fn parse_depends_strips_version_constraints() {
    // EC5: constraints are stripped from all deps
    let deps = parse_pacman_depends("Depends On     : glibc>=2.36 libpng>1.0 zlib=1.3 libssl<4\n");
    assert_eq!(deps, vec!["glibc", "libpng", "zlib", "libssl"]);
}

#[test]
fn parse_depends_empty_value_returns_empty() {
    // Boundary: "Depends On :" with no value after colon
    assert!(parse_pacman_depends("Depends On     : \n").is_empty());
}
