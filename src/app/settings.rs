//! The popup's settings page and how its changes are saved.
//!
//! Every change is written to cosmic-config right away (the same files that
//! can be edited by hand) and applied through [`App::apply_config`].

use chrono::TimeDelta;
use cosmic::{
    Element,
    app::Task,
    cosmic_config::ConfigSet,
    iced::{Alignment, Border, Length, Padding},
    widget::{self, button, container, icon, mouse_area, settings, spin_button, text, toggler},
};

use super::{App, Message};
use crate::{
    config::{self, ColorSpec, Config, ResolvedStyle, Style, ThemeColor, ThemeName, Tier},
    format, state, style,
};

/// Step and upper bound of the "Look ahead" control. The config file accepts
/// up to a week.
const LOOKAHEAD_STEP_MIN: u32 = 30;
const LOOKAHEAD_UI_MAX_MIN: u32 = 24 * 60;

/// Thresholds offered for warning levels (minutes). Values set by hand in the
/// config file are kept until changed here.
const TIER_STEPS: &[u32] = &[1, 2, 3, 5, 10, 15, 20, 25, 30, 45, 60, 90, 120, 180, 240];

/// Thresholds tried, in order, for a new warning level.
const NEW_TIER_CANDIDATES: &[u32] = &[5, 10, 15, 30, 45, 60, 90, 120];

/// Whose style a control edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    /// Index into `Config::tiers` (as stored, not sorted).
    Tier(usize),
    Far,
    Ongoing,
}

/// A setting changed in the popup.
#[derive(Debug, Clone)]
pub enum Setting {
    EmailInput(String),
    /// Enter pressed, field unfocused or popup closed.
    SaveEmail,
    RefreshMinutes(u64),
    LookaheadMinutes(u32),
    SkipAllDay(bool),
    SkipDeclined(bool),
    SkipTransparent(bool),
    PulseEnabled(bool),
    /// Warning level `.0` gets the threshold `TIER_STEPS[.1]`.
    TierStep(usize, usize),
    ThemeColor(Target, ThemeColor),
    HexInput(Target, String),
    /// Enter pressed, field unfocused or popup closed.
    SaveHex(Target),
    Pulse(Target, bool),
    AddTier,
    RemoveTier(usize),
    ResetAppearance,
}

fn style_of(config: &Config, target: Target) -> Option<&Style> {
    match target {
        Target::Tier(i) => config.tiers.get(i).map(|t| &t.style),
        Target::Far => Some(&config.far),
        Target::Ongoing => Some(&config.ongoing),
    }
}

fn style_mut(config: &mut Config, target: Target) -> Option<&mut Style> {
    match target {
        Target::Tier(i) => config.tiers.get_mut(i).map(|t| &mut t.style),
        Target::Far => Some(&mut config.far),
        Target::Ongoing => Some(&mut config.ongoing),
    }
}

/// `#RRGGBB`, or `#RRGGBBAA` if not opaque.
fn hex_string([r, g, b, a]: [u8; 4]) -> String {
    if a == 0xff {
        format!("#{r:02X}{g:02X}{b:02X}")
    } else {
        format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
    }
}

/// Index of the offered threshold closest to `minutes`.
fn nearest_step(minutes: u32) -> usize {
    (0..TIER_STEPS.len())
        .min_by_key(|&i| TIER_STEPS[i].abs_diff(minutes))
        .unwrap_or(0)
}

/// The threshold for warning level `index` when the control asks for
/// `TIER_STEPS[step]`: thresholds used by another level are skipped in the
/// direction of the change. `None` if there is no free value that way.
fn free_threshold(tiers: &[Tier], index: usize, step: usize) -> Option<u32> {
    let current = tiers.get(index)?.within_min;
    let wanted = *TIER_STEPS.get(step)?;
    let taken = |v: u32| {
        tiers
            .iter()
            .enumerate()
            .any(|(j, t)| j != index && t.within_min == v)
    };
    if !taken(wanted) {
        return Some(wanted);
    }
    if wanted > current {
        TIER_STEPS
            .iter()
            .copied()
            .filter(|&v| v > wanted)
            .find(|&v| !taken(v))
    } else {
        TIER_STEPS
            .iter()
            .rev()
            .copied()
            .filter(|&v| v < wanted)
            .find(|&v| !taken(v))
    }
}

fn new_tier_threshold(tiers: &[Tier]) -> u32 {
    let taken = |v: u32| tiers.iter().any(|t| t.within_min == v);
    NEW_TIER_CANDIDATES
        .iter()
        .copied()
        .find(|&v| !taken(v))
        .unwrap_or_else(|| tiers.iter().map(|t| t.within_min).max().unwrap_or(0) + 30)
}

impl App {
    /// Persist one key; the watcher's echo of it is a no-op because
    /// `apply_config` already took the new value.
    fn write_key<T: serde::Serialize>(&self, key: &str, value: T) {
        let Some(handler) = &self.config_handler else {
            return;
        };
        if let Err(err) = handler.set(key, value) {
            tracing::warn!("failed to save `{key}`: {err}");
        }
    }

    fn write_style(&self, config: &Config, target: Target) {
        match target {
            Target::Tier(_) => self.write_key("tiers", &config.tiers),
            Target::Far => self.write_key("far", &config.far),
            Target::Ongoing => self.write_key("ongoing", &config.ongoing),
        }
    }

    /// Save text fields that are still being edited (popup closed or page
    /// left without pressing Enter).
    pub(super) fn save_drafts(&mut self) -> Task<Message> {
        let mut tasks = vec![self.change_setting(Setting::SaveEmail)];
        let targets: Vec<Target> = self.hex_drafts.keys().copied().collect();
        for target in targets {
            tasks.push(self.change_setting(Setting::SaveHex(target)));
        }
        Task::batch(tasks)
    }

    pub(super) fn change_setting(&mut self, setting: Setting) -> Task<Message> {
        let mut config = self.config.clone();
        match setting {
            Setting::EmailInput(value) => {
                self.email_draft = value;
                return Task::none();
            }
            Setting::SaveEmail => {
                let email = Some(self.email_draft.trim().to_owned()).filter(|e| !e.is_empty());
                if email == config.my_email {
                    return Task::none();
                }
                self.write_key("my_email", &email);
                config.my_email = email;
            }
            Setting::RefreshMinutes(minutes) => {
                let secs = minutes.clamp(1, 60) * 60;
                self.write_key("refresh_interval_secs", secs);
                config.refresh_interval_secs = secs;
            }
            Setting::LookaheadMinutes(minutes) => {
                let minutes = minutes.clamp(LOOKAHEAD_STEP_MIN, state::MAX_LOOKAHEAD_MIN);
                self.write_key("lookahead_min", minutes);
                config.lookahead_min = minutes;
            }
            Setting::SkipAllDay(value) => {
                self.write_key("skip_all_day", value);
                config.skip_all_day = value;
            }
            Setting::SkipDeclined(value) => {
                self.write_key("skip_declined", value);
                config.skip_declined = value;
            }
            Setting::SkipTransparent(value) => {
                self.write_key("skip_transparent", value);
                config.skip_transparent = value;
            }
            Setting::PulseEnabled(value) => {
                self.write_key("pulse_enabled", value);
                config.pulse_enabled = value;
            }
            Setting::TierStep(index, step) => {
                let Some(minutes) = free_threshold(&config.tiers, index, step) else {
                    return Task::none();
                };
                config.tiers[index].within_min = minutes;
                self.write_key("tiers", &config.tiers);
            }
            Setting::ThemeColor(target, color) => {
                let Some(style) = style_mut(&mut config, target) else {
                    return Task::none();
                };
                style.color = ColorSpec::Theme(ThemeName::from(color));
                self.hex_drafts.remove(&target);
                self.write_style(&config, target);
            }
            Setting::HexInput(target, value) => {
                self.hex_drafts.insert(target, value);
                return Task::none();
            }
            Setting::SaveHex(target) => {
                let Some(draft) = self.hex_drafts.get(&target) else {
                    return Task::none();
                };
                // Invalid or empty input stays in the field, nothing is saved.
                let Some(rgba) = config::parse_hex(draft) else {
                    return Task::none();
                };
                self.hex_drafts.remove(&target);
                let Some(style) = style_mut(&mut config, target) else {
                    return Task::none();
                };
                style.color = ColorSpec::Rgba(hex_string(rgba));
                self.write_style(&config, target);
            }
            Setting::Pulse(target, value) => {
                let Some(style) = style_mut(&mut config, target) else {
                    return Task::none();
                };
                style.pulse = value;
                self.write_style(&config, target);
            }
            Setting::AddTier => {
                config.tiers.push(Tier {
                    within_min: new_tier_threshold(&config.tiers),
                    style: Style {
                        color: ColorSpec::Theme(ThemeName::from(ThemeColor::Warning)),
                        pulse: false,
                        dim: false,
                    },
                });
                self.write_key("tiers", &config.tiers);
            }
            Setting::RemoveTier(index) => {
                if index >= config.tiers.len() {
                    return Task::none();
                }
                config.tiers.remove(index);
                // Draft keys are tier indices, which just shifted.
                self.hex_drafts.clear();
                self.write_key("tiers", &config.tiers);
            }
            Setting::ResetAppearance => {
                let defaults = Config::default();
                config.tiers = defaults.tiers;
                config.far = defaults.far;
                config.ongoing = defaults.ongoing;
                self.hex_drafts.clear();
                self.write_key("tiers", &config.tiers);
                self.write_key("far", &config.far);
                self.write_key("ongoing", &config.ongoing);
            }
        }
        self.apply_config(config)
    }

    /// The settings page of the popup.
    pub(super) fn settings_page(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::active().cosmic().spacing;
        let header = widget::Row::new()
            .spacing(spacing.space_xs)
            .align_y(Alignment::Center)
            .push(
                button::icon(icon::from_name("go-previous-symbolic"))
                    .on_press(Message::ShowSettings(false)),
            )
            .push(text::title4("Settings"));

        let body = widget::Column::new()
            .spacing(spacing.space_m)
            // Room for the scroll bar.
            .padding(Padding {
                right: f32::from(spacing.space_xxs),
                ..Padding::ZERO
            })
            .push(self.general_section())
            .push(self.levels_section())
            .push(self.states_section())
            .push(
                button::standard("Reset levels and colors")
                    .on_press(Message::Setting(Setting::ResetAppearance)),
            );

        widget::Column::new()
            .spacing(spacing.space_s)
            .padding([spacing.space_s, spacing.space_m])
            .push(header)
            .push(container(widget::scrollable(body)).max_height(560.0))
            .into()
    }

    fn general_section(&self) -> Element<'_, Message> {
        let cfg = &self.config;
        let minutes = (cfg.refresh_interval_secs.max(30) + 30) / 60;
        let lookahead = cfg.lookahead_min.clamp(1, state::MAX_LOOKAHEAD_MIN);

        let email = widget::Column::new()
            .spacing(4)
            .push(text::body("Your e-mail (to skip declined meetings)"))
            .push(
                widget::text_input("you@example.com", self.email_draft.as_str())
                    .on_input(|v| Message::Setting(Setting::EmailInput(v)))
                    .on_submit(|_| Message::Setting(Setting::SaveEmail))
                    .on_unfocus(Message::Setting(Setting::SaveEmail)),
            );

        settings::section()
            .title("General")
            .add(email)
            .add(settings::item(
                "Refresh every",
                spin_button(format!("{minutes} min"), minutes.max(1), 1, 1, 60, |m| {
                    Message::Setting(Setting::RefreshMinutes(m))
                }),
            ))
            .add(settings::item(
                "Look ahead",
                spin_button(
                    format::remaining(TimeDelta::minutes(i64::from(lookahead))),
                    lookahead,
                    LOOKAHEAD_STEP_MIN,
                    LOOKAHEAD_STEP_MIN,
                    LOOKAHEAD_UI_MAX_MIN.max(lookahead),
                    |m| Message::Setting(Setting::LookaheadMinutes(m)),
                ),
            ))
            .add(settings::item(
                "Skip all-day events",
                toggler(cfg.skip_all_day).on_toggle(|v| Message::Setting(Setting::SkipAllDay(v))),
            ))
            .add(settings::item(
                "Skip declined meetings",
                toggler(cfg.skip_declined)
                    .on_toggle(|v| Message::Setting(Setting::SkipDeclined(v))),
            ))
            .add(settings::item(
                "Skip events marked free",
                toggler(cfg.skip_transparent)
                    .on_toggle(|v| Message::Setting(Setting::SkipTransparent(v))),
            ))
            .add(settings::item(
                "Pulsing (all levels)",
                toggler(cfg.pulse_enabled)
                    .on_toggle(|v| Message::Setting(Setting::PulseEnabled(v))),
            ))
            .into()
    }

    fn levels_section(&self) -> Element<'_, Message> {
        let tiers = &self.config.tiers;
        let mut order: Vec<usize> = (0..tiers.len()).collect();
        order.sort_by_key(|&i| tiers[i].within_min);

        let mut section = settings::section().title("Warning levels");
        for index in order {
            section = section.add(self.tier_editor(index));
        }
        section
            .add(
                button::text("Add level")
                    .leading_icon(icon::from_name("list-add-symbolic"))
                    .on_press(Message::Setting(Setting::AddTier)),
            )
            .into()
    }

    fn tier_editor(&self, index: usize) -> Element<'_, Message> {
        let tier = &self.config.tiers[index];
        let threshold = spin_button(
            format!(
                "≤ {}",
                format::remaining(TimeDelta::minutes(i64::from(tier.within_min)))
            ),
            nearest_step(tier.within_min),
            1,
            0,
            TIER_STEPS.len() - 1,
            move |step| Message::Setting(Setting::TierStep(index, step)),
        );
        // Tight spacing: this row nearly fills the popup's fixed width.
        let top = widget::Row::new()
            .spacing(4)
            .align_y(Alignment::Center)
            .push(threshold)
            .push(widget::Space::new().width(Length::Fill))
            .push(self.pulse_toggle(Target::Tier(index), tier.style.pulse))
            .push(
                button::icon(icon::from_name("user-trash-symbolic"))
                    .on_press(Message::Setting(Setting::RemoveTier(index))),
            );
        widget::Column::new()
            .spacing(6)
            .push(top)
            .push(self.color_row(Target::Tier(index)))
            .into()
    }

    fn states_section(&self) -> Element<'_, Message> {
        let state_editor = |label: &'static str, target: Target, pulse: bool| {
            widget::Column::new()
                .spacing(6)
                .push(
                    widget::Row::new()
                        .spacing(8)
                        .align_y(Alignment::Center)
                        .push(text::body(label))
                        .push(widget::Space::new().width(Length::Fill))
                        .push(self.pulse_toggle(target, pulse)),
                )
                .push(self.color_row(target))
        };
        settings::section()
            .title("Other states")
            .add(state_editor(
                "Later (no level matches)",
                Target::Far,
                self.config.far.pulse,
            ))
            .add(state_editor(
                "Started (\"now\")",
                Target::Ongoing,
                self.config.ongoing.pulse,
            ))
            .into()
    }

    fn pulse_toggle(&self, target: Target, pulse: bool) -> Element<'_, Message> {
        widget::Row::new()
            .spacing(4)
            .align_y(Alignment::Center)
            .push(text::caption("Pulse"))
            .push(toggler(pulse).on_toggle(move |v| Message::Setting(Setting::Pulse(target, v))))
            .into()
    }

    /// Theme color swatches plus a field for a custom `#RRGGBB` color.
    fn color_row(&self, target: Target) -> Element<'_, Message> {
        let Some(style) = style_of(&self.config, target) else {
            return widget::Space::new().into();
        };
        let selected = match &style.color {
            ColorSpec::Theme(name) => name.resolve(),
            ColorSpec::Rgba(_) => None,
        };
        let mut row = widget::Row::new().spacing(6).align_y(Alignment::Center);
        for color in ThemeColor::ALL {
            row = row.push(swatch(
                color,
                selected == Some(color),
                Message::Setting(Setting::ThemeColor(target, color)),
            ));
        }
        let hex = self
            .hex_drafts
            .get(&target)
            .cloned()
            .unwrap_or_else(|| match &style.color {
                ColorSpec::Rgba(hex) => hex.clone(),
                ColorSpec::Theme(_) => String::new(),
            });
        row.push(
            widget::text_input("#RRGGBB", hex)
                .width(Length::Fixed(96.0))
                .on_input(move |v| Message::Setting(Setting::HexInput(target, v)))
                .on_submit(move |_| Message::Setting(Setting::SaveHex(target)))
                .on_unfocus(Message::Setting(Setting::SaveHex(target))),
        )
        .into()
    }
}

/// A round, clickable sample of a theme color; the selected one is ringed.
fn swatch<'a>(color: ThemeColor, selected: bool, on_press: Message) -> Element<'a, Message> {
    let style = ResolvedStyle {
        color: config::Color::Theme(color),
        pulse: false,
        dim: false,
    };
    let dot = container(
        widget::Space::new()
            .width(Length::Fixed(20.0))
            .height(Length::Fixed(20.0)),
    )
    .style(move |theme: &cosmic::Theme| {
        let colors = style::pill_colors(&style, theme, 1.0);
        let cosmic = theme.cosmic();
        container::Style {
            background: Some(colors.background.into()),
            border: Border {
                radius: 999.0.into(),
                width: if selected { 2.5 } else { 1.0 },
                color: if selected {
                    cosmic.on_bg_color().into()
                } else {
                    cosmic.bg_divider().into()
                },
            },
            ..container::Style::default()
        }
    });
    widget::tooltip(
        mouse_area(dot).on_press(on_press),
        text::caption(color.name()),
        widget::tooltip::Position::Top,
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiers(mins: &[u32]) -> Vec<Tier> {
        Config::default()
            .tiers
            .into_iter()
            .cycle()
            .zip(mins)
            .map(|(t, &within_min)| Tier { within_min, ..t })
            .collect()
    }

    fn step(minutes: u32) -> usize {
        TIER_STEPS.iter().position(|&v| v == minutes).unwrap()
    }

    #[test]
    fn thresholds_skip_other_levels() {
        let t = tiers(&[2, 10, 15, 30]);
        // 10 -> 15 is taken, so up goes on to 20.
        assert_eq!(free_threshold(&t, 1, step(15)), Some(20));
        // 10 -> 5 is free.
        assert_eq!(free_threshold(&t, 1, step(5)), Some(5));
        // 15 -> 10 is taken, so down goes on to 5.
        assert_eq!(free_threshold(&t, 2, step(10)), Some(5));
        // 2 -> 1 is free.
        assert_eq!(free_threshold(&t, 0, step(1)), Some(1));
        let t = tiers(&[1, 2]);
        assert_eq!(free_threshold(&t, 1, step(1)), None);
        assert_eq!(free_threshold(&t, 9, 0), None);
    }

    #[test]
    fn nearest_offered_threshold() {
        assert_eq!(TIER_STEPS[nearest_step(10)], 10);
        assert_eq!(TIER_STEPS[nearest_step(7)], 5);
        assert_eq!(TIER_STEPS[nearest_step(1000)], 240);
        assert_eq!(TIER_STEPS[nearest_step(0)], 1);
    }

    #[test]
    fn new_levels_get_a_free_threshold() {
        assert_eq!(new_tier_threshold(&[]), 5);
        assert_eq!(new_tier_threshold(&tiers(&[2, 5, 10, 30, 60])), 15);
        let all: Vec<u32> = NEW_TIER_CANDIDATES.to_vec();
        assert_eq!(new_tier_threshold(&tiers(&all)), 150);
    }

    #[test]
    fn hex_formatting() {
        assert_eq!(hex_string([0xf5, 0x84, 0x1f, 0xff]), "#F5841F");
        assert_eq!(hex_string([0, 0, 0, 0x80]), "#00000080");
    }
}
