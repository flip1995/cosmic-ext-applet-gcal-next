//! Runs the full `--once` pipeline against the fixture feed, without network.

use std::process::Command;

/// The binary with an empty config/cache home, so the user's real settings
/// (read via cosmic-config) never affect the result.
fn command() -> Command {
    let home = std::env::temp_dir().join(concat!(env!("CARGO_PKG_NAME"), "-cli-test"));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cosmic-ext-applet-gcal-next"));
    cmd.env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_CACHE_HOME", home.join("cache"));
    cmd
}

fn once(now: &str) -> String {
    let output = command()
        .args([
            "--once",
            "--ics-file",
            "tests/fixtures/google.ics",
            "--now",
            now,
        ])
        .env("TZ", "Europe/Berlin")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn once_against_fixture() {
    let cases = [
        (
            "2026-11-10T08:00:00+01:00",
            "tier<=60m 1h | Focus time | 2026-11-10 09:00",
        ),
        (
            "2026-11-10T09:59:30+01:00",
            "tier<=1m 1m | Daily standup | 2026-11-10 10:00",
        ),
        (
            "2026-11-10T10:03:00+01:00",
            "ongoing now | Daily standup | 2026-11-10 10:00",
        ),
        // Moved instance of a recurring event; the cancelled 1:1 at 11:00 is
        // gone. Exactly at the 2 h lookahead limit.
        (
            "2026-11-10T11:00:00+01:00",
            "far 2h | Lunch with team (moved) | 2026-11-10 13:00",
        ),
        // Beyond the 2 h lookahead.
        ("2026-11-10T10:30:00+01:00", "none No meeting"),
        // Event in America/New_York.
        (
            "2026-11-10T14:30:00+01:00",
            "tier<=30m 30m | Customer call | 2026-11-10 15:00",
        ),
        // The cancelled 17:00 sync is skipped.
        ("2026-11-10T16:40:00+01:00", "none No meeting"),
        (
            "2026-11-10T21:45:00+01:00",
            "far 1h 45m | Late deploy | 2026-11-10 23:30",
        ),
        // Tomorrow's 08:00 meeting is more than 2 h away.
        ("2026-11-10T23:40:00+01:00", "none No meeting"),
    ];
    for (now, expected) in cases {
        assert_eq!(once(now), expected, "at {now}");
    }
}

#[test]
fn missing_file_is_error_state() {
    let output = command()
        .args(["--once", "--ics-file", "tests/fixtures/does-not-exist.ics"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "error !");
}
