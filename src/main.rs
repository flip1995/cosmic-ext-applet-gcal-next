//! COSMIC panel applet showing the time until the next Google Calendar
//! meeting.

mod app;
mod config;
mod format;
mod ironbar;
mod settings;
mod settings_window;
mod source;
mod state;
mod style;

use std::{io::BufRead, path::PathBuf, process::ExitCode, time::SystemTime};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, Utc};

use cosmic::cosmic_config::CosmicConfigEntry;

use crate::{
    config::Config,
    source::{EventSource, IcsSource, Location, Snapshot, fetch::Fetcher},
};

const USAGE: &str = "\
Usage: cosmic-ext-applet-gcal-next [OPTIONS]

Without options, runs as a COSMIC panel applet.

Options:
  --once                   Fetch once and print `state label | title | start`
  --ironbar                Keep running and print the pill as Pango markup
                           whenever it changes (Ironbar `script` module in
                           watch mode)
  --refresh                Make running `--ironbar` processes fetch now
  --join                   Open the next meeting's link (from the cache)
  --details                Print the next meeting's title and time (from the
                           cache), e.g. for a tooltip
  --ics-file <PATH>        Read the calendar from a local .ics file instead of
                           the secret iCal URL (also for the applet)
  --set-url                Read the secret iCal URL from stdin and store it in
                           the Secret Service keyring
  --settings               Open the settings in a window (for bars without the
                           COSMIC popup)
  --get <KEY>              Print a setting
  --set <KEY> <VALUE>      Change a setting (applies live); an empty value
                           unsets `my_email`
  --step <N>               With --set: round the number to a multiple of N
  --toggle <KEY>           Flip an on/off setting
  --now <RFC3339>          Pretend it is this time (with --once, --join,
                           --details)
  --print-default-config   Print the default settings (one cosmic-config key
                           per block)
  -V, --version            Print the version
  -h, --help               Print this help
";

#[derive(Debug, Default)]
struct Args {
    once: bool,
    ironbar: bool,
    settings: bool,
    refresh: bool,
    join: bool,
    details: bool,
    set_url: bool,
    print_default_config: bool,
    version: bool,
    help: bool,
    get: Option<String>,
    set: Option<(String, String)>,
    step: Option<u32>,
    toggle: Option<String>,
    ics_file: Option<PathBuf>,
    now: Option<DateTime<Utc>>,
}

impl Args {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let mut out = Self::default();
        while let Some(arg) = args.next() {
            let mut value =
                |name: &str| args.next().with_context(|| format!("{name} needs a value"));
            match arg.as_str() {
                "--once" => out.once = true,
                ironbar::FLAG => out.ironbar = true,
                "--settings" => out.settings = true,
                "--refresh" => out.refresh = true,
                "--join" => out.join = true,
                "--details" => out.details = true,
                "--set-url" => out.set_url = true,
                "--print-default-config" => out.print_default_config = true,
                "-V" | "--version" => out.version = true,
                "-h" | "--help" => out.help = true,
                "--ics-file" => out.ics_file = Some(value("--ics-file")?.into()),
                "--get" => out.get = Some(value("--get")?),
                "--toggle" => out.toggle = Some(value("--toggle")?),
                "--step" => {
                    let step = value("--step")?;
                    out.step = Some(
                        step.parse()
                            .with_context(|| format!("invalid --step `{step}`"))?,
                    );
                }
                "--set" => {
                    let key = value("--set")?;
                    out.set = Some((key, value("--set")?));
                }
                "--now" => {
                    let now = value("--now")?;
                    let now = DateTime::parse_from_rfc3339(&now)
                        .with_context(|| format!("invalid --now `{now}`"))?;
                    out.now = Some(now.with_timezone(&Utc));
                }
                other => bail!("unknown argument `{other}`\n\n{USAGE}"),
            }
        }
        Ok(out)
    }
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        // Ironbar and journald capture stderr: no escape codes there.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args = Args::parse(std::env::args().skip(1))?;
    if args.help {
        print!("{USAGE}");
    } else if args.version {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    } else if args.print_default_config {
        print_default_config()?;
    } else if args.once {
        runtime()?.block_on(once(&args))?;
    } else if args.ironbar {
        runtime()?.block_on(ironbar::run(args.ics_file))?;
    } else if args.settings {
        settings_window::run()?;
    } else if args.refresh {
        ironbar::refresh()?;
    } else if args.join {
        let now = args.now.unwrap_or_else(Utc::now);
        runtime()?.block_on(ironbar::join(args.ics_file.as_deref(), now))?;
    } else if args.details {
        let now = args.now.unwrap_or_else(Utc::now);
        println!(
            "{}",
            runtime()?.block_on(ironbar::details(args.ics_file.as_deref(), now))
        );
    } else if let Some(key) = &args.get {
        println!("{}", config::get_key(key)?);
    } else if let Some((key, value)) = &args.set {
        config::set_key(key, value, args.step)?;
    } else if let Some(key) = &args.toggle {
        println!("{}", config::toggle_key(key)?);
    } else if args.set_url {
        runtime()?.block_on(set_url())?;
    } else {
        let flags = app::Flags {
            ics_file: args.ics_file,
        };
        cosmic::applet::run::<app::App>(flags)?;
    }
    Ok(())
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

async fn location(args: &Args) -> Result<Location> {
    if let Some(path) = &args.ics_file {
        return Ok(Location::File(path.clone()));
    }
    Ok(Location::Url(config::load_secret_url().await?))
}

/// `--set-url`: store the URL (read from stdin, so it does not end up in the
/// shell history) in the keyring.
async fn set_url() -> Result<()> {
    eprintln!("Paste the secret iCal address and press Enter:");
    let mut url = String::new();
    std::io::stdin().lock().read_line(&mut url)?;
    config::store_url_in_keyring(&url).await?;
    eprintln!("Stored in the keyring.");
    if let Some(path) = config::url_path().filter(|p| p.exists()) {
        eprintln!(
            "Note: {} exists and takes precedence over the keyring.",
            path.display()
        );
    }
    Ok(())
}

/// `--once`: fetch, compute and print the state.
async fn once(args: &Args) -> Result<()> {
    let cfg = Config::load();
    let (appearance, warnings) = cfg.appearance();
    for warning in warnings {
        tracing::warn!("config: {warning}");
    }

    let now = args.now.unwrap_or_else(Utc::now);
    let tz = rrule::Tz::LOCAL;
    let (from, to) = state::expansion_window(now);

    // Never touch the real cache when reading a local file.
    let cache = args.ics_file.is_none().then(config::cache_path).flatten();
    let source = IcsSource::new(Fetcher::new(location(args).await?, cache)?, tz);
    let snapshot = match source.events(from, to).await {
        Ok(events) => Some(Snapshot {
            events,
            fetched_at: args.now.map_or_else(SystemTime::now, SystemTime::from),
        }),
        Err(err) => {
            tracing::warn!("fetch failed: {err:#}");
            source.cached(from, to).await
        }
    };

    let display = state::compute(snapshot.as_ref(), now, &cfg, &appearance);
    let kind = match display.kind {
        state::Kind::Tier(min) => format!("tier<={min}m"),
        other => format!("{other:?}").to_lowercase(),
    };
    match &display.event {
        Some(event) => println!(
            "{kind} {} | {} | {}",
            display.label,
            event.summary,
            event.start.with_timezone(&Local).format("%Y-%m-%d %H:%M"),
        ),
        None => println!("{kind} {}", display.label),
    }
    Ok(())
}

fn print_default_config() -> Result<()> {
    let cfg = Config::default();
    let pretty = ron::ser::PrettyConfig::new();
    macro_rules! keys {
        ($($field:ident: $ty:ty),* $(,)?) => {
            [$((stringify!($field), ron::ser::to_string_pretty(&cfg.$field, pretty.clone())?)),*]
        };
    }
    let keys = config::for_each_key!(keys);
    let dir = format!("~/.config/cosmic/{}/v{}", config::APP_ID, Config::VERSION);
    for (key, value) in keys {
        println!("// {dir}/{key}\n{value}\n");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args> {
        Args::parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn arg_parsing() {
        let args = parse(&[
            "--once",
            "--ics-file",
            "x.ics",
            "--now",
            "2026-11-10T10:00:00+01:00",
        ])
        .unwrap();
        assert!(args.once);
        assert_eq!(args.ics_file, Some(PathBuf::from("x.ics")));
        assert_eq!(args.now.unwrap().to_rfc3339(), "2026-11-10T09:00:00+00:00");

        let args = parse(&["--set", "lookahead_min", "90"]).unwrap();
        assert_eq!(args.set, Some(("lookahead_min".into(), "90".into())));
        assert!(parse(&["--set", "lookahead_min"]).is_err());
        assert!(parse(&["--ics-file"]).is_err());
        assert!(parse(&["--now", "yesterday"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
    }
}
