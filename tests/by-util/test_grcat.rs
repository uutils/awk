// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::process::{Command, Stdio};

use crate::{TESTS_BINARY, ucmd};

#[cfg_attr(
    not(target_os = "linux"),
    ignore = "grcat tests require Linux NSS via getent"
)]
#[test]
fn grcat_outputs_group_database_format() {
    let result = ucmd().succeeds();
    let stdout = result.stdout_str();

    assert!(
        !stdout.is_empty(),
        "grcat produced no output; group database may be unavailable in this environment"
    );

    for line in stdout.lines().filter(|line| !line.is_empty()) {
        let fields: Vec<&str> = line.split(':').collect();
        assert!(
            fields.len() >= 4,
            "expected at least 4 colon-separated fields, got {} in line: {line}",
            fields.len()
        );
        assert!(
            fields[2].chars().all(|ch| ch.is_ascii_digit()),
            "expected numeric gid in line: {line}"
        );
    }
}

// Regression test for gawk compatibility: grcat must match the group database
// format consumed by gawk library routines (see group.awk).
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "grcat tests require Linux NSS via getent"
)]
#[test]
fn grcat_matches_getent_group() {
    let getent = Command::new("getent")
        .arg("group")
        .output()
        .expect("failed to spawn getent; install it or skip this host explicitly");
    assert!(
        getent.status.success(),
        "getent group is required for this test (exit {:?}): {}",
        getent.status.code(),
        String::from_utf8_lossy(&getent.stderr)
    );

    let grcat = ucmd().succeeds();
    assert_eq!(
        getent.stdout.as_slice(),
        grcat.stdout(),
        "grcat output should match getent group"
    );
}

#[cfg_attr(
    not(target_os = "linux"),
    ignore = "grcat tests require Linux NSS via getent"
)]
#[test]
fn grcat_ignores_broken_pipe() {
    let mut child = Command::new(TESTS_BINARY)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn grcat");

    // Close the read end immediately so the next write hits EPIPE/BrokenPipe.
    drop(child.stdout.take());

    let status = child.wait().expect("failed to wait for grcat");
    assert!(
        status.success(),
        "grcat should treat broken pipe as success, got {status}"
    );
}
