//! A setting no restart can fix exits 78 (EX_CONFIG), so the units can stop
//! instead of restarting forever; anything a restart might fix exits 1.

use std::process::Command;

fn status(args: &[&str]) -> i32 {
    let dir = std::env::temp_dir().join(format!("timberfs-exit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("app.log");
    std::fs::write(&src, "2026-10-03T10:00:00Z hello\n").unwrap();
    let into = dir.join("s.log");
    let out = Command::new(env!("CARGO_BIN_EXE_timberfs"))
        .args(args)
        .arg(&src)
        .arg("--into")
        .arg(&into)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    out.status.code().unwrap()
}

#[test]
fn a_regex_that_does_not_compile_is_permanent() {
    assert_eq!(
        status(&[
            "import",
            "--timestamp-regex",
            "(",
            "--timestamp-format",
            "%s"
        ]),
        78
    );
}

#[test]
fn a_regex_without_a_capture_group_is_permanent() {
    assert_eq!(
        status(&[
            "import",
            "--timestamp-regex",
            "x",
            "--timestamp-format",
            "%s"
        ]),
        78
    );
}

#[test]
fn a_retention_that_does_not_parse_is_permanent_for_a_follower() {
    assert_eq!(
        status(&["import", "--follow", "--retain", "soon", "--quiet"]),
        78
    );
}

#[test]
fn a_missing_source_is_not_permanent() {
    let out = Command::new(env!("CARGO_BIN_EXE_timberfs"))
        .args(["import", "/nonexistent/app.log", "--into", "/tmp/never.log"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
}
