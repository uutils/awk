use crate::ucmd;

#[test]
fn empty_program_succeeds() {
    ucmd().arg("").succeeds();
}

#[test]
fn print_first_field() {
    ucmd().args(&["{ print $1 }"]).succeeds();
}

#[test]
fn no_args_fails_code_one() {
    ucmd().fails_with_code(1);
}

// Regression test for issue #5: writing to /dev/full must not panic.
#[cfg(target_os = "linux")]
#[test]
fn write_to_dev_full_does_not_panic() {
    use std::fs::OpenOptions;
    use std::process::{Command, Stdio};

    let dev_full = match OpenOptions::new().write(true).open("/dev/full") {
        Ok(f) => f,
        Err(_) => return, // /dev/full not available; skip.
    };
    let output = Command::new(super::TESTS_BINARY)
        .arg("BEGIN { print 1 }")
        .stdout(Stdio::from(dev_full))
        .stderr(Stdio::piped())
        .output()
        .expect("failed to spawn awk");
    // Must not panic (panic exits with code 2).
    assert_ne!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked"),
        "awk panicked on write to /dev/full: stderr={stderr}"
    );
}
