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

/// `pwcat` must match `getent passwd` so awk helpers that consume NSS dumps see
/// the same colon-separated records.
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

/// Force `EPIPE` by letting `head` close after one byte (closing an unread pipe
/// immediately can race if the whole dump fits in the pipe buffer).
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "pwcat tests require Linux NSS via getent"
)]
#[test]
fn pwcat_ignores_broken_pipe() {
    let mut head = Command::new("head")
        .args(["-c", "1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn head; install coreutils or skip this host explicitly");

    let mut child = Command::new(TESTS_BINARY)
        .stdout(head.stdin.take().expect("head stdin piped"))
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn pwcat");

    let status = child.wait().expect("failed to wait for pwcat");
    let _ = head.wait();

    assert!(
        status.success(),
        "pwcat should treat broken pipe as success, got {status}"
    );
}

#[cfg(not(unix))]
#[test]
fn pwcat_without_passwd_database_prints_nothing_and_succeeds() {
    ucmd().succeeds().no_output();
}
