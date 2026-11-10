//! The COSMIC panel applet: view, update, subscriptions.

mod settings;

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, LazyLock},
    time::{Duration, Instant, SystemTime},
};

use chrono::{DateTime, Local, Utc};
use cosmic::{
    Element,
    app::{Core, Task},
    cosmic_config::{self, CosmicConfigEntry},
    iced::{self, Alignment, Border, Length, Rectangle, Subscription, window},
    surface::action::{app_popup, destroy_popup},
    widget::{self, autosize, button, container, text},
};

pub use self::settings::Setting;
use crate::{
    config::{self, APP_ID, Appearance, Config},
    format,
    source::{
        self, EventSource, IcsSource, Location, Snapshot,
        fetch::{Backoff, Fetcher},
    },
    state::{self, DisplayState, Kind},
    style,
};

static AUTOSIZE_MAIN_ID: LazyLock<widget::Id> = LazyLock::new(|| widget::Id::new("autosize-main"));

/// UI tick: recompute the state.
const TICK: Duration = Duration::from_secs(1);
/// Animation frame interval while pulsing (~30 fps).
const FRAME: Duration = Duration::from_millis(33);
/// A wall-clock jump larger than this between two ticks (relative to the
/// monotonic clock, which stops during suspend) means we were suspended or
/// the clock was changed: refetch.
const CLOCK_JUMP: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default)]
pub struct Flags {
    /// Read the calendar from this file instead of the secret URL (testing).
    pub ics_file: Option<PathBuf>,
}

pub struct App {
    core: Core,
    flags: Flags,
    config: Config,
    appearance: Appearance,
    tz: rrule::Tz,

    /// Built lazily, so creating the URL file (or `--set-url`) works without
    /// restarting the panel.
    source: Option<Arc<IcsSource>>,
    /// Last successful fetch (or the disk cache).
    snapshot: Option<Snapshot>,
    last_error: Option<String>,
    backoff: Backoff,
    /// Only the most recently scheduled fetch timer is honoured.
    fetch_generation: u64,
    fetching: bool,

    display: DisplayState,
    last_tick: Option<(Instant, SystemTime)>,
    pulse_start: Instant,
    pulse_alpha: f32,
    popup: Option<window::Id>,

    /// Writes settings changed in the popup; `None` if cosmic-config is
    /// unavailable (changes then only last until the panel restarts).
    config_handler: Option<cosmic_config::Config>,
    /// The e-mail field while it is being edited.
    email_draft: String,
    /// Custom color fields while they are being edited.
    hex_drafts: HashMap<settings::Target, String>,
    /// The popup shows the settings page instead of the meeting details.
    show_settings: bool,
}

#[derive(Debug, Clone)]
pub enum Message {
    Tick,
    Frame,
    FetchNow(u64),
    Fetched(Option<Arc<IcsSource>>, Result<Snapshot, String>),
    CacheLoaded(Option<Snapshot>),
    Config(Box<Config>),
    /// The popup's *Join* button: open the meeting link.
    Join,
    /// Click on the pill: toggle the popup, anchored to the pill's bounds
    /// (relative to the applet window).
    TogglePopup(Rectangle),
    PopupClosed(window::Id),
    Refresh,
    Setting(Setting),
    ShowSettings(bool),
}

impl App {
    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn set_config(&mut self, config: Config) {
        let (appearance, warnings) = config.appearance();
        for warning in warnings {
            tracing::warn!("config: {warning}");
        }
        self.config = config;
        self.appearance = appearance;
    }

    /// Take over a changed config (from the watcher or the popup).
    fn apply_config(&mut self, config: Config) -> Task<Message> {
        if config == self.config {
            return Task::none();
        }
        let interval_changed = config.refresh_interval_secs != self.config.refresh_interval_secs;
        self.set_config(config);
        self.recompute();
        // While failing, the backoff decides; the interval applies again after
        // the next success.
        if interval_changed && !self.fetching && self.last_error.is_none() {
            return self.schedule_fetch(self.refresh_interval());
        }
        Task::none()
    }

    fn refresh_interval(&self) -> Duration {
        Duration::from_secs(self.config.refresh_interval_secs.max(30))
    }

    fn recompute(&mut self) {
        let display = state::compute(
            self.snapshot.as_ref(),
            Self::now(),
            &self.config,
            &self.appearance,
        );
        if display.pulse && !self.display.pulse {
            self.pulse_start = Instant::now();
        }
        if !display.pulse {
            self.pulse_alpha = 1.0;
        }
        self.display = display;
    }

    /// Fetch as soon as possible (startup, resume, "Refresh now").
    fn fetch_now(&mut self) -> Task<Message> {
        self.fetch_generation += 1;
        cosmic::task::message(Message::FetchNow(self.fetch_generation))
    }

    fn schedule_fetch(&mut self, after: Duration) -> Task<Message> {
        self.fetch_generation += 1;
        let generation = self.fetch_generation;
        cosmic::task::future(async move {
            tokio::time::sleep(after).await;
            Message::FetchNow(generation)
        })
    }

    fn start_fetch(&mut self) -> Task<Message> {
        self.fetching = true;
        let source = self.source.clone();
        let ics_file = self.flags.ics_file.clone();
        let tz = self.tz;
        cosmic::task::future(fetch(source, ics_file, tz))
    }

    fn join(&self) -> Task<Message> {
        let Some(event) = &self.display.event else {
            return Task::none();
        };
        let url = event.link.clone().unwrap_or_else(|| {
            // No video link: the day in Google Calendar's web UI.
            let day = event.start.with_timezone(&Local);
            format!(
                "https://calendar.google.com/calendar/r/day/{}",
                day.format("%Y/%m/%d")
            )
        });
        if let Err(err) = open::that_detached(&url) {
            tracing::warn!("failed to open the meeting link: {err}");
        }
        Task::none()
    }

    fn no_meeting_text(&self) -> String {
        format!(
            "No meeting in the next {}",
            format::remaining(state::lookahead(&self.config))
        )
    }

    fn pill(&self) -> Element<'_, Message> {
        let applet = &self.core.applet;
        let (icon_w, icon_h) = applet.suggested_size(true);
        let (pad_major, pad_minor) = applet.suggested_padding(true);
        let style = self.display.style;
        let alpha = self.pulse_alpha;
        let pill_style = move |theme: &cosmic::Theme| {
            let colors = style::pill_colors(&style, theme, alpha);
            container::Style {
                text_color: Some(colors.text),
                background: Some(colors.background.into()),
                border: Border {
                    radius: 999.0.into(),
                    ..Border::default()
                },
                ..container::Style::default()
            }
        };

        if applet.is_horizontal() {
            let thickness = f32::from(icon_h + 2 * pad_minor);
            let pill = container(applet.text(self.display.label.clone()))
                .padding([0, pad_major.max(8)])
                .center_y(Length::Fixed(f32::from(icon_h + pad_minor)))
                .style(pill_style);
            container(pill)
                .center_y(Length::Fixed(thickness))
                .padding([0, pad_major / 2])
                .into()
        } else {
            let thickness = f32::from(icon_w + 2 * pad_minor);
            let diameter = (f32::from(icon_w) * 0.6).round();
            let dot = container(
                widget::Space::new()
                    .width(Length::Fixed(diameter))
                    .height(Length::Fixed(diameter)),
            )
            .style(pill_style);
            container(dot)
                .center_x(Length::Fixed(thickness))
                .padding([pad_major / 2, 0])
                .into()
        }
    }

    fn popup_view(&self) -> Element<'_, Message> {
        if self.show_settings {
            return self
                .core
                .applet
                .popup_container(self.settings_page())
                .into();
        }
        let spacing = cosmic::theme::active().cosmic().spacing;
        let mut content = widget::Column::new()
            .spacing(spacing.space_xs)
            .padding([spacing.space_s, spacing.space_m]);

        match &self.display.event {
            Some(event) => {
                let start = event.start.with_timezone(&Local);
                let end = event.end.with_timezone(&Local);
                let when = if self.display.kind == Kind::Ongoing {
                    format!("{} – {} · now", start.format("%H:%M"), end.format("%H:%M"))
                } else {
                    format!(
                        "{} – {} · in {}",
                        start.format("%H:%M"),
                        end.format("%H:%M"),
                        format::remaining(event.start - Self::now())
                    )
                };
                let summary = if event.summary.is_empty() {
                    "(no title)".to_owned()
                } else {
                    event.summary.clone()
                };
                content = content.push(text::title4(summary)).push(text::body(when));
            }
            None if self.display.kind == Kind::Error => {
                content = content.push(text::title4("Calendar not synced"));
            }
            None => content = content.push(text::title4(self.no_meeting_text())),
        }

        let mut buttons = widget::Row::new()
            .spacing(spacing.space_xs)
            .align_y(Alignment::Center);
        if let Some(event) = &self.display.event {
            let label = if event.link.is_some() {
                "Join"
            } else {
                "Open calendar"
            };
            buttons = buttons.push(button::suggested(label).on_press(Message::Join));
        }
        let refresh = button::standard(if self.fetching {
            "Refreshing…"
        } else {
            "Refresh now"
        });
        buttons = buttons.push(if self.fetching {
            refresh
        } else {
            refresh.on_press(Message::Refresh)
        });
        content = content.push(buttons);

        let sync = match (&self.snapshot, &self.last_error) {
            (Some(s), None) => format!(
                "Last sync: {}",
                DateTime::<Local>::from(s.fetched_at).format("%H:%M")
            ),
            (Some(s), Some(err)) => format!(
                "Sync failed: {err}\nShowing data from {}",
                DateTime::<Local>::from(s.fetched_at).format("%H:%M")
            ),
            (None, Some(err)) => format!("Sync failed: {err}"),
            (None, None) => "Not synced yet".to_owned(),
        };
        content = content.push(text::caption(sync)).push(
            button::text("Settings…")
                .leading_icon(widget::icon::from_name("emblem-system-symbolic"))
                .on_press(Message::ShowSettings(true)),
        );

        self.core.applet.popup_container(content).into()
    }
}

/// Fetch and expand for [`state::expansion_window`]; the state is recomputed
/// from that every tick.
async fn fetch(
    source: Option<Arc<IcsSource>>,
    ics_file: Option<PathBuf>,
    tz: rrule::Tz,
) -> Message {
    let source = match source {
        Some(source) => source,
        None => match make_source(ics_file, tz).await {
            Ok(source) => Arc::new(source),
            Err(err) => return Message::Fetched(None, Err(format!("{err:#}"))),
        },
    };
    let (from, to) = state::expansion_window(Utc::now());
    let result = source
        .events(from, to)
        .await
        .map(|events| Snapshot {
            events,
            fetched_at: SystemTime::now(),
        })
        .map_err(|err| format!("{err:#}"));
    Message::Fetched(Some(source), result)
}

async fn make_source(ics_file: Option<PathBuf>, tz: rrule::Tz) -> anyhow::Result<IcsSource> {
    let (location, cache) = match ics_file {
        // Never touch the real cache when reading a local file.
        Some(path) => (Location::File(path), None),
        None => (
            Location::Url(config::load_secret_url().await?),
            config::cache_path(),
        ),
    };
    Ok(IcsSource::new(Fetcher::new(location, cache)?, tz))
}

impl cosmic::Application for App {
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = Flags;
    type Message = Message;
    const APP_ID: &'static str = APP_ID;

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, flags: Flags) -> (Self, Task<Message>) {
        let config = Config::load();
        let (appearance, _) = config.appearance();
        let tz = rrule::Tz::LOCAL;
        let display = state::compute(None, Utc::now(), &config, &appearance);
        let mut app = Self {
            core,
            flags,
            config: Config::default(),
            appearance,
            tz,
            source: None,
            snapshot: None,
            last_error: None,
            backoff: Backoff::default(),
            fetch_generation: 0,
            fetching: false,
            display,
            last_tick: None,
            pulse_start: Instant::now(),
            pulse_alpha: 1.0,
            popup: None,
            config_handler: cosmic_config::Config::new(APP_ID, Config::VERSION)
                .inspect_err(|err| tracing::warn!("settings cannot be saved: {err}"))
                .ok(),
            email_draft: String::new(),
            hex_drafts: HashMap::new(),
            show_settings: false,
        };
        app.set_config(config);

        let mut tasks = vec![app.fetch_now()];
        if app.flags.ics_file.is_none()
            && let Some(path) = config::cache_path()
        {
            let now = Utc::now();
            tasks.push(cosmic::task::future(async move {
                let (from, to) = state::expansion_window(now);
                Message::CacheLoaded(source::cached_snapshot(&path, tz, from, to).await)
            }));
        }
        (app, Task::batch(tasks))
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tick => {
                let (mono, wall) = (Instant::now(), SystemTime::now());
                let mut task = Task::none();
                if let Some((last_mono, last_wall)) = self.last_tick {
                    let mono_elapsed = mono.duration_since(last_mono);
                    let wall_elapsed = wall.duration_since(last_wall).unwrap_or_default();
                    if wall_elapsed > mono_elapsed + CLOCK_JUMP {
                        tracing::info!("clock jumped (resume from suspend?), refreshing");
                        task = self.fetch_now();
                    }
                }
                self.last_tick = Some((mono, wall));
                self.recompute();
                return task;
            }
            Message::Frame => {
                let elapsed = self.pulse_start.elapsed().as_millis();
                self.pulse_alpha = style::pulse_alpha(
                    u64::try_from(elapsed).unwrap_or(u64::MAX),
                    self.config.pulse_period_ms,
                    self.config.pulse_min_alpha,
                );
            }
            Message::FetchNow(generation) => {
                if generation == self.fetch_generation && !self.fetching {
                    return self.start_fetch();
                }
            }
            Message::Fetched(source, result) => {
                self.fetching = false;
                if source.is_some() {
                    self.source = source;
                }
                let next = match result {
                    Ok(snapshot) => {
                        self.snapshot = Some(snapshot);
                        self.last_error = None;
                        self.backoff.success();
                        self.refresh_interval()
                    }
                    Err(err) => {
                        tracing::warn!("fetch failed: {err}");
                        self.last_error = Some(err);
                        self.backoff.failure()
                    }
                };
                self.recompute();
                return self.schedule_fetch(next);
            }
            Message::CacheLoaded(snapshot) => {
                // A fetch may have finished first; it is newer.
                if self.snapshot.is_none() && snapshot.is_some() {
                    self.snapshot = snapshot;
                    self.recompute();
                }
            }
            Message::Config(config) => return self.apply_config(*config),
            Message::Setting(setting) => return self.change_setting(setting),
            Message::ShowSettings(show) => {
                self.show_settings = show;
                if !show {
                    return self.save_drafts();
                }
            }
            Message::Join => return self.join(),
            Message::Refresh => return self.fetch_now(),
            Message::TogglePopup(anchor) => {
                if let Some(id) = self.popup.take() {
                    let save = self.save_drafts();
                    let close = cosmic::task::message(cosmic::Action::Surface(destroy_popup(id)));
                    return Task::batch([save, close]);
                }
                self.email_draft = self.config.my_email.clone().unwrap_or_default();
                self.hex_drafts.clear();
                self.show_settings = false;
                return cosmic::task::message(cosmic::Action::Surface(app_popup::<App>(
                    |_| Default::default(),
                    move |app: &mut App| {
                        let id = window::Id::unique();
                        app.popup = Some(id);
                        let mut settings = app.core.applet.get_popup_settings(
                            app.core.main_window_id().unwrap_or(window::Id::RESERVED),
                            id,
                            None,
                            None,
                            None,
                        );
                        // Center the popup on the pill instead of the window's
                        // left edge.
                        settings.positioner.anchor_rect = Rectangle {
                            x: anchor.x.max(0.0) as i32,
                            y: anchor.y.max(0.0) as i32,
                            width: anchor.width.max(1.0) as i32,
                            height: anchor.height.max(1.0) as i32,
                        };
                        settings
                    },
                    None,
                )));
            }
            Message::PopupClosed(id) => {
                if self.popup == Some(id) {
                    self.popup = None;
                    return self.save_drafts();
                }
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let button = button::custom(self.pill())
            .padding(0)
            .on_press_with_rectangle(|offset, bounds| {
                Message::TogglePopup(Rectangle {
                    x: bounds.x - offset.x,
                    y: bounds.y - offset.y,
                    ..bounds
                })
            })
            .class(cosmic::theme::Button::AppletIcon);
        autosize::autosize(button, AUTOSIZE_MAIN_ID.clone()).into()
    }

    fn view_window(&self, _id: window::Id) -> Element<'_, Message> {
        self.popup_view()
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![
            iced::time::every(TICK).map(|_| Message::Tick),
            self.core.watch_config::<Config>(APP_ID).map(|update| {
                config::log_errors(&update.errors);
                Message::Config(Box::new(update.config))
            }),
        ];
        // No animation subscription (and no redraws) unless pulsing.
        if self.display.pulse {
            subscriptions.push(iced::time::every(FRAME).map(|_| Message::Frame));
        }
        Subscription::batch(subscriptions)
    }

    fn style(&self) -> Option<iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}
