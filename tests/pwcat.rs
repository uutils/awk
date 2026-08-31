// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

#[cfg_attr(
    not(target_os = "linux"),
    ignore = "pwcat tests require Linux NSS via getent"
)]
#[test]
fn pwcat_outputs_passwd_database_format() {
    use std::process::Command;

    let output = Command::new(env!("CARGO_BIN_EXE_pwcat"))
        .output()
        .expect("failed to run pwcat");

    assert!(
        output.status.success(),
        "pwcat failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
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
    use std::process::Command;

    let getent = Command::new("getent")
        .arg("passwd")
        .output()
        .expect("failed to run getent");
    if !getent.status.success() {
        return;
    }

    let pwcat = Command::new(env!("CARGO_BIN_EXE_pwcat"))
        .output()
        .expect("failed to run pwcat");
    assert!(
        pwcat.status.success(),
        "pwcat failed: stderr={}",
        String::from_utf8_lossy(&pwcat.stderr)
    );

    assert_eq!(
        getent.stdout, pwcat.stdout,
        "pwcat output should match getent passwd"
    );
}
