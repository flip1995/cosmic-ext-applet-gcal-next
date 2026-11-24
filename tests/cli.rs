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

#[test]
fn details_against_fixture() {
    let details = |now: &str| {
        let output = command()
            .args([
                "--details",
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
    };
    assert_eq!(
        details("2026-11-10T09:55:00+01:00"),
        "Daily standup\n10:00 – 10:15"
    );
    assert_eq!(
        details("2026-11-10T16:40:00+01:00"),
        "No meeting in the next 2h"
    );
}

/// `--ironbar` publishes the pill and the popup texts over Ironbar's IPC
/// socket (a fake one here).
#[test]
fn ironbar_publishes_variables() {
    use std::{
        collections::HashMap,
        io::{BufRead, BufReader, Write},
        os::unix::net::UnixListener,
        time::{Duration, Instant},
    };

    let runtime = std::env::temp_dir().join(concat!(env!("CARGO_PKG_NAME"), "-ipc-test"));
    let _ = std::fs::remove_dir_all(&runtime);
    std::fs::create_dir_all(&runtime).unwrap();
    let listener = UnixListener::bind(runtime.join("ironbar-ipc.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();

    let mut child = command()
        .args(["--ironbar", "--ics-file", "tests/fixtures/google.ics"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let mut vars = HashMap::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !(vars.contains_key("gcal_next_pill") && vars.contains_key("gcal_next_settings")) {
        assert!(Instant::now() < deadline, "timed out, got {vars:?}");
        let Ok((stream, _)) = listener.accept() else {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        };
        stream.set_nonblocking(false).unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        (&stream).write_all(b"{\"type\":\"ok\"}\n").unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["command"], "var");
        assert_eq!(request["subcommand"], "set");
        vars.insert(
            request["key"].as_str().unwrap().to_owned(),
            request["value"].as_str().unwrap().to_owned(),
        );
    }
    child.kill().unwrap();
    child.wait().unwrap();

    let pill = &vars["gcal_next_pill"];
    assert!(
        pill.starts_with("<span background=\"#") && pill.ends_with(" </span>"),
        "{pill}"
    );
    assert!(vars["gcal_next_settings"].contains("Settings…"));
    let _ = std::fs::remove_dir_all(&runtime);
}
