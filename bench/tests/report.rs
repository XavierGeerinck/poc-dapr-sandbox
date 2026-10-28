use sandbox_bench::report::generate;
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn out_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sandbox-bench-test-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn gate_pass_at_90ms() {
    let o = out_dir("pass");
    let r = generate(&fixture("pass"), &o).unwrap();
    assert!(r.pass, "{}", r.verdict);
    assert!(r.verdict.starts_with("GATE PASS"), "{}", r.verdict);
    assert!(r.warnings.is_empty());
    assert!(o.join("summary.md").exists());
    assert!(o.join("rate-sweep.svg").exists());
}

#[test]
fn gate_fail_at_120ms() {
    let r = generate(&fixture("fail"), &out_dir("fail")).unwrap();
    assert!(!r.pass);
    assert!(r.verdict.starts_with("GATE FAIL"), "{}", r.verdict);
}

#[test]
fn restore_rows_warn_and_are_excluded() {
    let o = out_dir("restore");
    let r = generate(&fixture("restore"), &o).unwrap();
    // The 300 ms RESTORE rows would break the gate if they were included.
    assert!(r.pass, "{}", r.verdict);
    assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
    assert!(r.warnings[0].contains("5.0%"), "{:?}", r.warnings);
    let s = std::fs::read_to_string(o.join("summary.md")).unwrap();
    assert!(s.contains("acquire_path != POOL: 10"), "{s}");
}

#[test]
fn missing_rate_20_fails() {
    let r = generate(&fixture("norate"), &out_dir("norate")).unwrap();
    assert!(!r.pass);
    assert!(r.verdict.contains("no rate=20 rows"), "{}", r.verdict);
}

#[test]
fn non_200_rows_are_counted() {
    let o = out_dir("errors");
    let r = generate(&fixture("errors"), &o).unwrap();
    assert!(r.pass, "{}", r.verdict);
    let s = std::fs::read_to_string(o.join("summary.md")).unwrap();
    assert!(s.contains("non-200 rows: 3"), "{s}");
}
