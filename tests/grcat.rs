// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

#[cfg_attr(
    not(target_os = "linux"),
    ignore = "grcat tests require Linux NSS via getent"
)]
#[test]
fn grcat_outputs_group_database_format() {
    use std::process::Command;

    let output = Command::new(env!("CARGO_BIN_EXE_grcat"))
        .output()
        .expect("failed to run grcat");

    assert!(
        output.status.success(),
        "grcat failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
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
    use std::process::Command;

    let getent = Command::new("getent")
        .arg("group")
        .output()
        .expect("failed to run getent");
    if !getent.status.success() {
        return;
    }

    let grcat = Command::new(env!("CARGO_BIN_EXE_grcat"))
        .output()
        .expect("failed to run grcat");
    assert!(
        grcat.status.success(),
        "grcat failed: stderr={}",
        String::from_utf8_lossy(&grcat.stderr)
    );

    assert_eq!(
        getent.stdout, grcat.stdout,
        "grcat output should match getent group"
    );
}
