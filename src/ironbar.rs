//! Ironbar instead of the COSMIC panel (see `resources/ironbar.toml`).
//!
//! - `--ironbar`: one background process per Ironbar. A hidden `script`
//!   module starts it; Ironbar stops it on reload, and since script modules
//!   with the same command replace each other, there is one even with a bar
//!   per monitor. Same config, fetch timing, disk cache and colors as the
//!   applet. It publishes the pill (Pango markup) and the popup's texts as
//!   ironvars (`gcal_next_*`) over Ironbar's IPC socket; every bar shows
//!   them. The pulse is emulated by publishing ~10 pills per second.
//! - `--refresh`: makes the running `--ironbar` fetch now (SIGUSR1).
//! - `--join` / `--details`: for buttons and tooltips; they read the disk
//!   cache only, never the network.
//! - `--settings` (see `settings_window`): the full settings page.

use std::{
    ffi::OsStr,
    os::unix::{ffi::OsStrExt as _, fs::MetadataExt as _},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Local, Utc};
use cosmic::{
    cosmic_config::{self, CosmicConfigEntry as _},
    iced::Color,
};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
    net::UnixStream,
    signal::unix::{SignalKind, signal},
    sync::mpsc,
};

use crate::{
    config::{self, APP_ID, Appearance, Config, ResolvedStyle, ThemeColor},
    format,
    source::{self, EventSource as _, IcsSource, Location, Snapshot, fetch},
    state::{self, DisplayState, Kind},
    style,
};

/// The flag that starts the background process (also how `--refresh` finds
/// it).
pub const FLAG: &str = "--ironbar";

/// Recompute the state this often.
const TICK: Duration = Duration::from_secs(1);
/// Pill update interval while pulsing.
const FRAME: Duration = Duration::from_millis(100);
/// See `app::CLOCK_JUMP`: a larger wall-clock jump means resume.
const CLOCK_JUMP: Duration = Duration::from_secs(5);

/// Prefix of the published ironvars.
const VAR_PREFIX: &str = "gcal_next_";

/// The pill as Pango markup: the label padded with spaces on a colored
/// background (Pango has no rounded corners or padding).
pub fn markup(display: &DisplayState, theme: &cosmic::Theme, pulse: f32) -> String {
    span(
        &display.style,
        theme,
        pulse,
        &format!(" {} ", display.label),
    )
}

/// A button label that looks like a button: the text on a theme color.
fn chip(text: &str, color: ThemeColor, theme: &cosmic::Theme) -> String {
    let style = ResolvedStyle {
        color: config::Color::Theme(color),
        pulse: false,
        dim: false,
    };
    span(&style, theme, 1.0, &format!("  {text}  "))
}

fn span(style: &ResolvedStyle, theme: &cosmic::Theme, pulse: f32, text: &str) -> String {
    let colors = style::pill_colors(style, theme, pulse);
    let (bg, bg_alpha) = hex(colors.background);
    let (fg, fg_alpha) = hex(colors.text);
    format!(
        "<span background=\"{bg}\" bgalpha=\"{bg_alpha}%\" foreground=\"{fg}\" \
         fgalpha=\"{fg_alpha}%\">{}</span>",
        escape(text)
    )
}

/// `#rrggbb` and the alpha in percent (Pango rejects 0).
fn hex(color: Color) -> (String, u8) {
    let [r, g, b, _] = color.into_rgba8();
    let alpha = (color.a.clamp(0.0, 1.0) * 100.0).round() as u8;
    (format!("#{r:02x}{g:02x}{b:02x}"), alpha.max(1))
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// The sync status shown in the popup.
#[derive(Debug, Clone, Default)]
pub struct Sync {
    pub fetched_at: Option<SystemTime>,
    pub error: Option<String>,
    pub fetching: bool,
}

/// The popup's texts, as `(ironvar name without prefix, value)`. Values with
/// `<span` are markup (Ironbar shows them verbatim); others are plain text
/// (Ironbar escapes them).
pub fn popup_vars(
    display: &DisplayState,
    config: &Config,
    sync: &Sync,
    now: DateTime<Utc>,
    theme: &cosmic::Theme,
) -> Vec<(&'static str, String)> {
    let bold = |text: &str| format!("<span weight=\"bold\">{}</span>", escape(text));
    let (title, time) = match &display.event {
        Some(event) => {
            let start = event.start.with_timezone(&Local);
            let end = event.end.with_timezone(&Local);
            let when = if display.kind == Kind::Ongoing {
                "now".to_owned()
            } else {
                format!("in {}", format::remaining(event.start - now))
            };
            let summary = if event.summary.is_empty() {
                "(no title)"
            } else {
                &event.summary
            };
            (
                bold(summary),
                format!(
                    "{} – {} · {when}",
                    start.format("%H:%M"),
                    end.format("%H:%M")
                ),
            )
        }
        None if display.kind == Kind::Error => (bold("Calendar not synced"), String::new()),
        None => (bold(&state::no_meeting_text(config)), String::new()),
    };
    let join = match &display.event {
        Some(event) if event.link.is_some() => "Join",
        _ => "Open calendar",
    };
    let time_of = |at: SystemTime| DateTime::<Local>::from(at).format("%H:%M").to_string();
    let sync_text = match (sync.fetching, sync.fetched_at, &sync.error) {
        (true, _, _) => "Refreshing…".to_owned(),
        (false, Some(at), None) => format!("Last sync: {}", time_of(at)),
        (false, Some(at), Some(err)) => {
            format!("Sync failed: {err}\nShowing data from {}", time_of(at))
        }
        (false, None, Some(err)) => format!("Sync failed: {err}"),
        (false, None, None) => "Not synced yet".to_owned(),
    };
    let refresh = if sync.fetching {
        "Refreshing…"
    } else {
        "Refresh now"
    };
    vec![
        ("title", title),
        ("time", time),
        ("sync", sync_text),
        ("join", chip(join, ThemeColor::Accent, theme)),
        ("refresh", chip(refresh, ThemeColor::Neutral, theme)),
        ("settings", chip("Settings…", ThemeColor::Neutral, theme)),
    ]
}

/// Ironbar's IPC socket.
fn ipc_socket() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|dir| PathBuf::from(dir).join("ironbar-ipc.sock"))
}

/// `ironbar var set`, without spawning the CLI (the pulse sets ~10/s).
async fn set_var(socket: &Path, key: &str, value: &str) -> Result<()> {
    let request = serde_json::json!({
        "command": "var",
        "subcommand": "set",
        "key": key,
        "value": value,
    });
    let mut stream = UnixStream::connect(socket).await?;
    stream.write_all(format!("{request}\n").as_bytes()).await?;
    stream.shutdown().await?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).await?;
    if !response.contains("\"ok\"") {
        bail!("unexpected response {}", response.trim());
    }
    Ok(())
}

/// Sets ironvars in order and only on change; while Ironbar is slow, only
/// the latest value per key is sent.
struct Ironvars {
    tx: mpsc::UnboundedSender<(&'static str, String)>,
    last: std::collections::HashMap<&'static str, String>,
}

impl Ironvars {
    fn new() -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<(&'static str, String)>();
        tokio::spawn(async move {
            let Some(socket) = ipc_socket() else {
                tracing::warn!("XDG_RUNTIME_DIR is not set, cannot reach Ironbar");
                return;
            };
            let mut warned = false;
            while let Some(first) = rx.recv().await {
                let mut batch = vec![first];
                while let Ok(next) = rx.try_recv() {
                    match batch.iter_mut().find(|(key, _)| *key == next.0) {
                        Some(pending) => pending.1 = next.1,
                        None => batch.push(next),
                    }
                }
                for (key, value) in batch {
                    let name = format!("{VAR_PREFIX}{key}");
                    if let Err(err) = set_var(&socket, &name, &value).await
                        && !std::mem::replace(&mut warned, true)
                    {
                        tracing::warn!("cannot set Ironbar variables: {err:#}");
                    }
                }
            }
        });
        Self {
            tx,
            last: std::collections::HashMap::new(),
        }
    }

    fn publish(&mut self, vars: impl IntoIterator<Item = (&'static str, String)>) {
        for (key, value) in vars {
            if self.last.get(key) != Some(&value) {
                let _ = self.tx.send((key, value.clone()));
                self.last.insert(key, value);
            }
        }
    }
}

/// Whether nobody reads our stdout any more (Ironbar exited). Ironbar keeps
/// it open otherwise; we never write to it.
fn stdout_closed() -> bool {
    let mut fd = libc::pollfd {
        fd: libc::STDOUT_FILENO,
        events: 0,
        revents: 0,
    };
    // SAFETY: one valid pollfd, no timeout.
    let ready = unsafe { libc::poll(&mut fd, 1, 0) };
    ready > 0 && fd.revents & (libc::POLLERR | libc::POLLHUP) != 0
}

fn appearance(config: &Config) -> Appearance {
    let (appearance, warnings) = config.appearance();
    for warning in warnings {
        tracing::warn!("config: {warning}");
    }
    appearance
}

/// `--ironbar`: publish the pill and popup texts until Ironbar stops us.
pub async fn run(ics_file: Option<PathBuf>) -> Result<()> {
    // First, so an early `--refresh` does not kill us (SIGUSR1's default).
    let mut refresh = signal(SignalKind::user_defined1())?;

    let tz = rrule::Tz::LOCAL;
    let mut config = Config::load();
    let mut appearance = appearance(&config);
    // The COSMIC theme if one is configured, else COSMIC's default dark one.
    let mut theme = cosmic::theme::system_preference();

    // Live settings changes. The watcher must stay alive.
    let (config_tx, mut config_rx) = mpsc::unbounded_channel();
    let _watcher = cosmic_config::Config::new(APP_ID, Config::VERSION)
        .and_then(|handler| {
            handler.watch(move |_, _| {
                let _ = config_tx.send(());
            })
        })
        .inspect_err(|err| tracing::warn!("settings changes need a restart: {err}"))
        .ok();

    let mut snapshot: Option<Snapshot> = None;
    if ics_file.is_none()
        && let Some(path) = config::cache_path()
    {
        let (from, to) = state::expansion_window(Utc::now());
        snapshot = source::cached_snapshot(&path, tz, from, to).await;
    }

    let mut source: Option<Arc<IcsSource>> = None;
    let mut fetching: Option<tokio::task::JoinHandle<_>> = None;
    let mut next_fetch = tokio::time::Instant::now();
    let mut last_error: Option<String> = None;
    let mut fetched_once = false;
    let mut backoff = fetch::Backoff::default();
    let mut last_tick = (Instant::now(), SystemTime::now());
    let mut pulse_start: Option<Instant> = None;
    let mut vars = Ironvars::new();

    loop {
        let now = Utc::now();
        let display = state::compute(snapshot.as_ref(), now, &config, &appearance);
        let alpha = if display.pulse {
            let elapsed = pulse_start.get_or_insert_with(Instant::now).elapsed();
            style::pulse_alpha(
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                config.pulse_period_ms,
                config.pulse_min_alpha,
            )
        } else {
            pulse_start = None;
            1.0
        };
        // Before the first fetch without a cache, `!` would only flash.
        if snapshot.is_some() || fetched_once {
            vars.publish([("pill", markup(&display, &theme, alpha))]);
        }
        let sync = Sync {
            fetched_at: snapshot.as_ref().map(|s| s.fetched_at),
            error: last_error.clone(),
            fetching: fetching.is_some(),
        };
        vars.publish(popup_vars(&display, &config, &sync, now, &theme));

        let wait = if display.pulse { FRAME } else { TICK };
        tokio::select! {
            () = tokio::time::sleep(wait) => {
                if stdout_closed() {
                    return Ok(());
                }
                let now = (Instant::now(), SystemTime::now());
                let mono = now.0.duration_since(last_tick.0);
                let wall = now.1.duration_since(last_tick.1).unwrap_or_default();
                if wall > mono + CLOCK_JUMP {
                    tracing::info!("clock jumped (resume from suspend?), refreshing");
                    next_fetch = tokio::time::Instant::now();
                }
                last_tick = now;
            }
            () = tokio::time::sleep_until(next_fetch), if fetching.is_none() => {
                fetching = Some(tokio::spawn(source::fetch_snapshot(
                    source.clone(),
                    ics_file.clone(),
                    tz,
                )));
            }
            joined = async { fetching.as_mut().expect("guarded").await }, if fetching.is_some() => {
                fetching = None;
                fetched_once = true;
                let (new_source, result) =
                    joined.unwrap_or_else(|err| (None, Err(format!("fetch task failed: {err}"))));
                if new_source.is_some() {
                    source = new_source;
                }
                let delay = match result {
                    Ok(new) => {
                        snapshot = Some(new);
                        last_error = None;
                        backoff.success();
                        config.refresh_interval()
                    }
                    Err(err) => {
                        tracing::warn!("fetch failed: {err}");
                        last_error = Some(err);
                        backoff.failure()
                    }
                };
                next_fetch = tokio::time::Instant::now() + delay;
            }
            Some(()) = config_rx.recv() => {
                config = Config::load();
                appearance = self::appearance(&config);
                theme = cosmic::theme::system_preference();
                // A shorter interval applies right away (not while failing:
                // the backoff decides then).
                if last_error.is_none() {
                    next_fetch = next_fetch.min(tokio::time::Instant::now() + config.refresh_interval());
                }
            }
            Some(()) = refresh.recv() => {
                tracing::info!("refresh requested");
                next_fetch = tokio::time::Instant::now();
            }
        }
    }
}

/// The state at `now` from the disk cache (or `ics_file`), without network.
async fn offline_display(ics_file: Option<&Path>, now: DateTime<Utc>) -> (Config, DisplayState) {
    let config = Config::load();
    let appearance = appearance(&config);
    let tz = rrule::Tz::LOCAL;
    let (from, to) = state::expansion_window(now);
    let snapshot = match ics_file {
        Some(path) => match fetch::Fetcher::new(Location::File(path.to_owned()), None) {
            Ok(fetcher) => IcsSource::new(fetcher, tz)
                .events(from, to)
                .await
                .inspect_err(|err| tracing::warn!("{err:#}"))
                .ok()
                .map(|events| Snapshot {
                    events,
                    fetched_at: now.into(),
                }),
            Err(err) => {
                tracing::warn!("{err:#}");
                None
            }
        },
        None => match config::cache_path() {
            Some(path) => source::cached_snapshot(&path, tz, from, to).await,
            None => None,
        },
    };
    let display = state::compute(snapshot.as_ref(), now, &config, &appearance);
    (config, display)
}

/// `--details`: plain text for a tooltip.
pub async fn details(ics_file: Option<&Path>, now: DateTime<Utc>) -> String {
    let (config, display) = offline_display(ics_file, now).await;
    match &display.event {
        Some(event) => {
            let summary = if event.summary.is_empty() {
                "(no title)"
            } else {
                &event.summary
            };
            format!(
                "{summary}\n{} – {}",
                event.start.with_timezone(&Local).format("%H:%M"),
                event.end.with_timezone(&Local).format("%H:%M"),
            )
        }
        None if display.kind == Kind::Error => "Calendar not synced".to_owned(),
        None => state::no_meeting_text(&config),
    }
}

/// `--join`: open the meeting link, or today in Google Calendar if there is
/// no meeting.
pub async fn join(ics_file: Option<&Path>, now: DateTime<Utc>) -> Result<()> {
    let (_, display) = offline_display(ics_file, now).await;
    let url = match &display.event {
        Some(event) => state::join_url(event),
        None => state::calendar_day_url(now),
    };
    open::that_detached(&url).context("failed to open the browser")
}

/// `--refresh`: signal the user's `--ironbar` process(es).
pub fn refresh() -> Result<()> {
    let mut signalled = 0;
    for pid in processes_with(FLAG)? {
        if let Ok(pid) = libc::pid_t::try_from(pid)
            // SAFETY: plain syscall; the pid was just checked to be ours.
            && unsafe { libc::kill(pid, libc::SIGUSR1) } == 0
        {
            signalled += 1;
        }
    }
    if signalled == 0 {
        bail!("no running `{} {FLAG}` found", env!("CARGO_PKG_NAME"));
    }
    Ok(())
}

/// Other processes of this user running this binary with `flag`.
pub fn processes_with(flag: &str) -> Result<Vec<u32>> {
    let me = std::process::id();
    let uid = std::fs::metadata("/proc/self")?.uid();
    let mut pids = Vec::new();
    for entry in std::fs::read_dir("/proc")?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == me || entry.metadata().ok().map(|m| m.uid()) != Some(uid) {
            continue;
        }
        if let Ok(cmdline) = std::fs::read(entry.path().join("cmdline"))
            && runs_with(&cmdline, flag)
        {
            pids.push(pid);
        }
    }
    Ok(pids)
}

/// Whether a `/proc/<pid>/cmdline` is `…/cosmic-ext-applet-gcal-next … <flag> …`
/// (not a shell whose command line merely mentions it).
fn runs_with(cmdline: &[u8], flag: &str) -> bool {
    let mut args = cmdline.split(|&b| b == 0);
    let Some(argv0) = args.next() else {
        return false;
    };
    Path::new(OsStr::from_bytes(argv0)).file_name() == Some(OsStr::new(env!("CARGO_PKG_NAME")))
        && args.any(|arg| arg == flag.as_bytes())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::{ResolvedStyle, ThemeColor};

    fn display(label: &str, color: config::Color, dim: bool) -> DisplayState {
        DisplayState {
            kind: Kind::None,
            label: label.to_owned(),
            style: ResolvedStyle {
                color,
                pulse: false,
                dim,
            },
            pulse: false,
            event: None,
        }
    }

    #[test]
    fn markup_colors_and_escaping() {
        let theme = cosmic::Theme::dark();
        let custom = display(
            "<A & B>",
            config::Color::Rgba([0x20, 0x20, 0x60, 0xff]),
            false,
        );
        assert_eq!(
            markup(&custom, &theme, 1.0),
            "<span background=\"#202060\" bgalpha=\"100%\" foreground=\"#ffffff\" \
             fgalpha=\"100%\"> &lt;A &amp; B&gt; </span>"
        );
        // Pulse and dim scale the alpha; never 0 (Pango rejects it).
        assert!(markup(&custom, &theme, 0.5).contains("bgalpha=\"50%\""));
        assert!(markup(&custom, &theme, 0.0).contains("bgalpha=\"1%\""));
        let dim = display("x", config::Color::Rgba([0, 0, 0, 0xff]), true);
        assert!(markup(&dim, &theme, 1.0).contains("fgalpha=\"55%\""));
        // Theme colors resolve too.
        let warning = display("5m", config::Color::Theme(ThemeColor::Warning), false);
        assert!(markup(&warning, &theme, 1.0).starts_with("<span background=\"#"));
    }

    #[test]
    fn popup_texts() {
        use chrono::TimeZone as _;

        let theme = cosmic::Theme::dark();
        let now = Utc.with_ymd_and_hms(2026, 11, 10, 9, 55, 0).unwrap();
        let config = Config::default();
        let mut meeting = display("5m", config::Color::Theme(ThemeColor::Warning), false);
        meeting.kind = Kind::Tier(5);
        meeting.event = Some(state::EventInfo {
            summary: "A & B".to_owned(),
            start: now + chrono::Duration::minutes(5),
            end: now + chrono::Duration::minutes(35),
            link: Some("https://meet.google.com/x".to_owned()),
        });
        let sync = Sync {
            fetched_at: Some(SystemTime::UNIX_EPOCH),
            error: Some("timeout".to_owned()),
            fetching: false,
        };
        let vars: HashMap<_, _> = popup_vars(&meeting, &config, &sync, now, &theme)
            .into_iter()
            .collect();
        assert_eq!(vars["title"], "<span weight=\"bold\">A &amp; B</span>");
        assert!(vars["time"].ends_with(" · in 5m"), "{}", vars["time"]);
        assert!(vars["sync"].starts_with("Sync failed: timeout\nShowing data from "));
        // Buttons are colored chips.
        assert!(vars["join"].starts_with("<span background=\"#"));
        assert!(vars["join"].ends_with(">  Join  </span>"));
        assert!(vars["refresh"].ends_with(">  Refresh now  </span>"));
        assert!(vars["settings"].ends_with(">  Settings…  </span>"));

        let none = display(
            "No meeting",
            config::Color::Theme(ThemeColor::Neutral),
            true,
        );
        let fetching = Sync {
            fetching: true,
            ..Sync::default()
        };
        let vars: HashMap<_, _> = popup_vars(&none, &config, &fetching, now, &theme)
            .into_iter()
            .collect();
        assert_eq!(
            vars["title"],
            "<span weight=\"bold\">No meeting in the next 2h</span>"
        );
        assert_eq!(vars["time"], "");
        assert!(vars["join"].ends_with(">  Open calendar  </span>"));
        assert_eq!(vars["sync"], "Refreshing…");
        assert!(vars["refresh"].ends_with(">  Refreshing…  </span>"));
    }

    #[test]
    fn finds_watch_processes() {
        assert!(runs_with(
            b"/usr/bin/cosmic-ext-applet-gcal-next\0--ironbar\0",
            FLAG
        ));
        assert!(runs_with(
            b"cosmic-ext-applet-gcal-next\0--ics-file\0x.ics\0--ironbar\0",
            FLAG
        ));
        // The COSMIC applet, other modes, and shells mentioning the flag.
        assert!(!runs_with(b"/usr/bin/cosmic-ext-applet-gcal-next\0", FLAG));
        assert!(!runs_with(
            b"cosmic-ext-applet-gcal-next\0--refresh\0",
            FLAG
        ));
        assert!(!runs_with(
            b"sh\0-c\0cosmic-ext-applet-gcal-next --ironbar\0",
            FLAG
        ));
        assert!(!runs_with(b"", FLAG));
    }
}
