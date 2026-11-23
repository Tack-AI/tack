//! OS-backed credential locks must exclude independent processes, not only
//! Tokio tasks in one test process.
#![allow(clippy::unwrap_used)]

use std::io::{BufRead as _, Write as _};

#[test]
fn lock_holder_process_entry() {
    let Ok(target) = std::env::var("TACK_TEST_LOCK_TARGET") else {
        return;
    };
    let _guard = tack_app::file_lock::strict_lock(
        std::path::Path::new(&target),
        std::time::Duration::ZERO,
        std::time::Duration::ZERO,
    )
    .unwrap();
    println!("TACK_TEST_LOCK_READY");
    std::io::stdout().flush().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(400));
}

#[test]
fn strict_lock_excludes_an_independent_process() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("auth.json");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lock_holder_process_entry", "--nocapture"])
        .env("TACK_TEST_LOCK_TARGET", &target)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        if line.contains("TACK_TEST_LOCK_READY") {
            break;
        }
    }
    let error = tack_app::file_lock::strict_lock(
        &target,
        std::time::Duration::ZERO,
        std::time::Duration::ZERO,
    )
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(child.wait().unwrap().success());
    let guard = tack_app::file_lock::strict_lock(
        &target,
        std::time::Duration::ZERO,
        std::time::Duration::ZERO,
    )
    .unwrap();
    drop(guard);
}
