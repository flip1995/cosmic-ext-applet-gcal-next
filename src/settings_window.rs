//! `--settings`: the settings page in a window of its own, for bars without
//! the COSMIC popup (Ironbar's popup can't host text inputs or color
//! pickers). Same page and config files as the applet; changes apply live
//! everywhere through the cosmic-config watchers.

use cosmic::{
    ApplicationExt as _, Element,
    app::{Core, Task},
    iced::{Length, Size, Subscription, window},
    widget,
};

use crate::{
    config::{self, APP_ID, Config},
    settings::{Setting, SettingsState},
};

/// Its own app ID, so window rules can target it (e.g. float it on
/// Hyprland).
pub const WINDOW_APP_ID: &str = "io.github.flip1995.CosmicExtAppletGcalNext.Settings";

pub struct SettingsWindow {
    core: Core,
    settings: SettingsState,
}

#[derive(Debug, Clone)]
pub enum Message {
    Setting(Setting),
    Config(Box<Config>),
    /// Window closing: save fields still being edited.
    SaveDrafts,
}

pub fn run() -> anyhow::Result<()> {
    // A second click on *Settings…* must not open a second window.
    if !crate::ironbar::processes_with("--settings")?.is_empty() {
        tracing::info!("the settings window is already open");
        return Ok(());
    }
    let settings = cosmic::app::Settings::default()
        .size(Size::new(460.0, 760.0))
        .exit_on_close(true);
    cosmic::app::run::<SettingsWindow>(settings, ())?;
    Ok(())
}

impl cosmic::Application for SettingsWindow {
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = WINDOW_APP_ID;

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, (): ()) -> (Self, Task<Message>) {
        let mut window = Self {
            core,
            settings: SettingsState::new(Config::load()),
        };
        let title = match window.core.main_window_id() {
            Some(id) => window.set_window_title("Next Meeting settings".to_owned(), id),
            None => Task::none(),
        };
        (window, title)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Setting(setting) => {
                self.settings.change(setting);
            }
            Message::Config(config) => self.settings.config = *config,
            Message::SaveDrafts => {
                self.settings.save_drafts();
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::active().cosmic().spacing;
        widget::scrollable(
            widget::container(self.settings.view().map(Message::Setting))
                .padding([spacing.space_s, spacing.space_m])
                .width(Length::Fill),
        )
        .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        self.core.watch_config::<Config>(APP_ID).map(|update| {
            config::log_errors(&update.errors);
            Message::Config(Box::new(update.config))
        })
    }

    fn on_close_requested(&self, _id: window::Id) -> Option<Message> {
        Some(Message::SaveDrafts)
    }

    fn on_app_exit(&mut self) -> Option<Message> {
        // Synchronously: the window is about to go.
        self.settings.save_drafts();
        None
    }
}
