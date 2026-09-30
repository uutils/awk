// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::process::{Command, Stdio};

use crate::{TESTS_BINARY, ucmd};

#[cfg_attr(
    not(target_os = "linux"),
    ignore = "pwcat tests require Linux NSS via getent"
)]
#[test]
fn pwcat_outputs_passwd_database_format() {
    let result = ucmd().succeeds();
    let stdout = result.stdout_str();

    assert!(
        !stdout.is_empty(),
        "pwcat produced no output; password database may be unavailable in this environment"
    );

    for line in stdout.lines().filter(|line| !line.is_empty()) {
        let fields: Vec<&str> = line.split(':').collect();
        assert_eq!(
            fields.len(),
            7,
            "expected 7 colon-separated fields in line: {line}"
        );
        assert!(
            fields[2].chars().all(|ch| ch.is_ascii_digit()),
            "expected numeric uid in line: {line}"
        );
        assert!(
            fields[3].chars().all(|ch| ch.is_ascii_digit()),
            "expected numeric gid in line: {line}"
        );
    }
}

// Regression test for gawk compatibility: pwcat must match the password database
// format consumed by gawk library routines (see passwd.awk).
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "pwcat tests require Linux NSS via getent"
)]
#[test]
fn pwcat_matches_getent_passwd() {
    let getent = Command::new("getent")
        .arg("passwd")
        .output()
        .expect("failed to spawn getent; install it or skip this host explicitly");
    assert!(
        getent.status.success(),
        "getent passwd is required for this test (exit {:?}): {}",
        getent.status.code(),
        String::from_utf8_lossy(&getent.stderr)
    );

    let pwcat = ucmd().succeeds();
    assert_eq!(
        getent.stdout.as_slice(),
        pwcat.stdout(),
        "pwcat output should match getent passwd"
    );
}

#[cfg_attr(
    not(target_os = "linux"),
    ignore = "pwcat tests require Linux NSS via getent"
)]
#[test]
fn pwcat_ignores_broken_pipe() {
    let mut child = Command::new(TESTS_BINARY)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn pwcat");

    // Close the read end immediately so the next write hits EPIPE/BrokenPipe.
    drop(child.stdout.take());

    let status = child.wait().expect("failed to wait for pwcat");
    assert!(
        status.success(),
        "pwcat should treat broken pipe as success, got {status}"
    );
}
