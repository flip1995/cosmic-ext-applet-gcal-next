//! Settings (non-secret, `cosmic-config`) and the secret feed URL.

use std::{
    fmt,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use cosmic::cosmic_config::{self, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

pub const APP_ID: &str = "io.github.flip1995.CosmicExtAppletGcalNext";
/// Directory name below `$XDG_CONFIG_HOME` / `$XDG_CACHE_HOME`.
pub const DIR_NAME: &str = env!("CARGO_PKG_NAME");

/// Settings. Every field has a default, missing or broken keys fall back to
/// it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, CosmicConfigEntry)]
#[version = 1]
#[serde(default)]
pub struct Config {
    pub refresh_interval_secs: u64,
    pub ongoing_grace_min: u32,
    /// Only meetings starting within this many minutes count (1 min–1 week).
    pub lookahead_min: u32,
    pub skip_all_day: bool,
    pub skip_declined: bool,
    /// For the `PARTSTAT=DECLINED` filter; without it, nothing is filtered.
    pub my_email: Option<String>,
    /// Skip events marked as "free" (`TRANSP:TRANSPARENT`).
    pub skip_transparent: bool,
    /// Skip events whose end equals their start.
    pub skip_zero_duration: bool,
    /// Show the error state when the last successful fetch is older than this.
    pub stale_after_min: u32,

    // --- appearance: times, colors, pulsing ---
    pub tiers: Vec<Tier>,
    pub far: Style,
    pub ongoing: Style,
    pub none: Style,
    pub error: Style,
    pub none_label: String,
    pub ongoing_label: String,
    /// Global kill switch for pulsing, overrides every tier.
    pub pulse_enabled: bool,
    pub pulse_period_ms: u32,
    pub pulse_min_alpha: f32,
}

impl Default for Config {
    fn default() -> Self {
        let theme = |c: ThemeColor, pulse: bool, dim: bool| Style {
            color: ColorSpec::Theme(ThemeName::from(c)),
            pulse,
            dim,
        };
        Self {
            refresh_interval_secs: 120,
            ongoing_grace_min: 5,
            lookahead_min: 120,
            skip_all_day: true,
            skip_declined: true,
            my_email: None,
            skip_transparent: false,
            skip_zero_duration: false,
            stale_after_min: 60,
            tiers: [
                (60, ThemeColor::Neutral, false),
                (30, ThemeColor::Accent, false),
                (10, ThemeColor::Success, false),
                (5, ThemeColor::Warning, false),
                (1, ThemeColor::Destructive, true),
            ]
            .into_iter()
            .map(|(within_min, color, pulse)| Tier {
                within_min,
                style: theme(color, pulse, false),
            })
            .collect(),
            far: theme(ThemeColor::Neutral, false, false),
            ongoing: theme(ThemeColor::Destructive, false, false),
            none: theme(ThemeColor::Neutral, false, true),
            error: theme(ThemeColor::Neutral, false, true),
            none_label: "No meeting".into(),
            ongoing_label: "now".into(),
            pulse_enabled: true,
            pulse_period_ms: 1200,
            pulse_min_alpha: 0.35,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tier {
    /// Applies while the pill shows at most this many minutes (the label's
    /// rounded-up value, so `1` is the last minute before the start).
    pub within_min: u32,
    pub style: Style,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Style {
    pub color: ColorSpec,
    #[serde(default)]
    pub pulse: bool,
    /// Draw the pill with reduced opacity.
    #[serde(default)]
    pub dim: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ColorSpec {
    /// A color of the active COSMIC theme.
    Theme(ThemeName),
    /// A fixed color, `#RRGGBB` or `#RRGGBBAA`.
    Rgba(String),
}

/// A theme color token as written in the config. Unknown tokens deserialize
/// fine (so one bad entry does not discard the whole list) and are rejected in
/// [`Config::appearance`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeName(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeColor {
    Neutral,
    Accent,
    Success,
    Warning,
    Destructive,
}

impl ThemeColor {
    pub const ALL: [Self; 5] = [
        Self::Neutral,
        Self::Accent,
        Self::Success,
        Self::Warning,
        Self::Destructive,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Neutral => "Neutral",
            Self::Accent => "Accent",
            Self::Success => "Success",
            Self::Warning => "Warning",
            Self::Destructive => "Destructive",
        }
    }
}

impl From<ThemeColor> for ThemeName {
    fn from(c: ThemeColor) -> Self {
        Self(c.name().to_owned())
    }
}

impl ThemeName {
    pub fn resolve(&self) -> Option<ThemeColor> {
        ThemeColor::ALL
            .into_iter()
            .find(|c| c.name().eq_ignore_ascii_case(&self.0))
    }
}

impl Serialize for ThemeName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Serialize known tokens as bare identifiers (`Theme(Warning)` in RON).
        match self.resolve() {
            Some(c) => serializer.serialize_unit_variant("ThemeColor", c as u32, c.name()),
            None => serializer.serialize_str(&self.0),
        }
    }
}

impl<'de> Deserialize<'de> for ThemeName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = ThemeName;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a theme color name")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<ThemeName, E> {
                Ok(ThemeName(v.to_owned()))
            }
        }
        deserializer.deserialize_identifier(Visitor)
    }
}

/// A validated color.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Color {
    Theme(ThemeColor),
    Rgba([u8; 4]),
}

/// A validated style.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedStyle {
    pub color: Color,
    pub pulse: bool,
    pub dim: bool,
}

/// The validated appearance settings: tiers sorted ascending by `within_min`,
/// invalid entries removed.
#[derive(Debug, Clone, PartialEq)]
pub struct Appearance {
    pub tiers: Vec<(u32, ResolvedStyle)>,
    pub far: ResolvedStyle,
    pub ongoing: ResolvedStyle,
    pub none: ResolvedStyle,
    pub error: ResolvedStyle,
}

impl Style {
    fn resolve(&self) -> Result<ResolvedStyle, String> {
        let color = match &self.color {
            ColorSpec::Theme(name) => Color::Theme(
                name.resolve()
                    .ok_or_else(|| format!("unknown theme color `{}`", name.0))?,
            ),
            ColorSpec::Rgba(hex) => {
                Color::Rgba(parse_hex(hex).ok_or_else(|| format!("invalid color `{hex}`"))?)
            }
        };
        Ok(ResolvedStyle {
            color,
            pulse: self.pulse,
            dim: self.dim,
        })
    }
}

/// Log the problems of a cosmic-config read. Missing keys are normal (the
/// default is used) and not reported.
pub fn log_errors(errors: &[cosmic_config::Error]) {
    for err in errors.iter().filter(|e| e.is_err()) {
        tracing::warn!("config: {err}");
    }
}

impl Config {
    /// Read the settings from cosmic-config, falling back to the defaults for
    /// missing or broken keys.
    pub fn load() -> Self {
        let handler = match cosmic_config::Config::new(APP_ID, Self::VERSION) {
            Ok(handler) => handler,
            Err(err) => {
                tracing::warn!("config: {err}");
                return Self::default();
            }
        };
        Self::get_entry(&handler).unwrap_or_else(|(errors, config)| {
            log_errors(&errors);
            config
        })
    }

    /// Validate the appearance settings. Invalid entries are skipped (fixed
    /// states fall back to their default) and reported in the returned
    /// warnings.
    pub fn appearance(&self) -> (Appearance, Vec<String>) {
        let defaults = Config::default();
        let mut warnings = Vec::new();

        let mut tiers: Vec<(u32, ResolvedStyle)> = Vec::new();
        for (i, tier) in self.tiers.iter().enumerate() {
            if tier.within_min == 0 {
                warnings.push(format!(
                    "tier {i}: `within_min` must be at least 1, skipped"
                ));
                continue;
            }
            if tiers.iter().any(|(min, _)| *min == tier.within_min) {
                warnings.push(format!(
                    "tier {i}: duplicate `within_min: {}`, skipped",
                    tier.within_min
                ));
                continue;
            }
            match tier.style.resolve() {
                Ok(style) => tiers.push((tier.within_min, style)),
                Err(err) => warnings.push(format!("tier {i}: {err}, skipped")),
            }
        }
        tiers.sort_by_key(|(min, _)| *min);

        let mut fixed = |name: &str, style: &Style, default: &Style| {
            style.resolve().unwrap_or_else(|err| {
                warnings.push(format!("`{name}`: {err}, using the default"));
                default.resolve().expect("default styles are valid")
            })
        };
        let appearance = Appearance {
            far: fixed("far", &self.far, &defaults.far),
            ongoing: fixed("ongoing", &self.ongoing, &defaults.ongoing),
            none: fixed("none", &self.none, &defaults.none),
            error: fixed("error", &self.error, &defaults.error),
            tiers,
        };
        (appearance, warnings)
    }
}

/// Parse `#RRGGBB` or `#RRGGBBAA` (the `#` is optional).
pub fn parse_hex(s: &str) -> Option<[u8; 4]> {
    let s = s.trim();
    let hex = s.strip_prefix('#').unwrap_or(s);
    if !matches!(hex.len(), 6 | 8) || !hex.is_ascii() {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    let alpha = if hex.len() == 8 { byte(6)? } else { 0xff };
    Some([byte(0)?, byte(2)?, byte(4)?, alpha])
}

fn xdg_dir(var: &str, fallback: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::home_dir().map(|home| home.join(fallback)))
}

/// `$XDG_CONFIG_HOME/cosmic-ext-applet-gcal-next/ical-url`
pub fn url_path() -> Option<PathBuf> {
    xdg_dir("XDG_CONFIG_HOME", ".config").map(|d| d.join(DIR_NAME).join("ical-url"))
}

/// `$XDG_CACHE_HOME/cosmic-ext-applet-gcal-next/calendar.ics`
pub fn cache_path() -> Option<PathBuf> {
    xdg_dir("XDG_CACHE_HOME", ".cache").map(|d| d.join(DIR_NAME).join("calendar.ics"))
}

/// Attributes of the keyring item holding the URL.
const KEYRING_ATTRIBUTES: [(&str, &str); 2] = [("application", APP_ID), ("type", "ical-url")];

/// The secret iCal URL: from the `ical-url` file if it exists, otherwise from
/// the Secret Service keyring (`--set-url`).
pub async fn load_secret_url() -> Result<String> {
    let path = url_path().context("cannot determine the config directory")?;
    if path.exists() {
        return load_url(&path);
    }
    match load_url_from_keyring().await {
        Ok(Some(url)) => Ok(url),
        Ok(None) => bail!(
            "no iCal URL configured: run `{} --set-url` or create {} (mode 600)",
            env!("CARGO_PKG_NAME"),
            path.display()
        ),
        Err(err) => Err(err.context(format!(
            "no {} and the keyring is not available",
            path.display()
        ))),
    }
}

async fn load_url_from_keyring() -> Result<Option<String>> {
    let keyring = oo7::Keyring::new().await?;
    let Some(item) = keyring
        .search_items(&KEYRING_ATTRIBUTES)
        .await?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    if item.is_locked().await? {
        item.unlock().await?;
    }
    let secret = item.secret().await?;
    let url = std::str::from_utf8(&secret)
        .context("the keyring item is not valid UTF-8")?
        .trim()
        .to_owned();
    validate_url(&url, "the keyring item")?;
    Ok(Some(url))
}

/// Store the URL in the Secret Service keyring, replacing an existing one.
pub async fn store_url_in_keyring(url: &str) -> Result<()> {
    let url = url.trim();
    validate_url(url, "the URL")?;
    let keyring = oo7::Keyring::new().await?;
    keyring.unlock().await?;
    keyring
        .create_item(
            "Google Calendar secret iCal address (cosmic-ext-applet-gcal-next)",
            &KEYRING_ATTRIBUTES,
            url,
            true,
        )
        .await?;
    Ok(())
}

fn validate_url(url: &str, what: &str) -> Result<()> {
    if url.is_empty() {
        bail!("{what} is empty");
    }
    // The URL is a secret: plain HTTP would leak it.
    if !url.starts_with("https://") {
        bail!("{what} must be an https:// URL");
    }
    Ok(())
}

/// Read the secret iCal URL. Refuses files that are readable by group or
/// others.
pub fn load_url(path: &Path) -> Result<String> {
    let meta = std::fs::metadata(path)
        .with_context(|| format!("cannot read the iCal URL file {}", path.display()))?;
    let mode = meta.permissions().mode();
    if mode & 0o077 != 0 {
        bail!(
            "{} is accessible by other users (mode {:o}); run `chmod 600` on it",
            path.display(),
            mode & 0o777
        );
    }
    let url = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read the iCal URL file {}", path.display()))?;
    let url = url.trim();
    validate_url(url, &path.display().to_string())?;
    Ok(url.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style(color: ColorSpec) -> Style {
        Style {
            color,
            pulse: false,
            dim: false,
        }
    }

    fn theme(name: &str) -> ColorSpec {
        ColorSpec::Theme(ThemeName(name.into()))
    }

    #[test]
    fn defaults_are_valid_and_sorted() {
        let (appearance, warnings) = Config::default().appearance();
        assert!(warnings.is_empty(), "{warnings:?}");
        let mins: Vec<u32> = appearance.tiers.iter().map(|(m, _)| *m).collect();
        assert_eq!(mins, [1, 5, 10, 30, 60]);
        assert_eq!(
            appearance.tiers[1].1.color,
            Color::Theme(ThemeColor::Warning)
        );
        assert!(appearance.tiers[0].1.pulse);
        assert!(appearance.tiers[1..].iter().all(|(_, s)| !s.pulse));
        assert!(!appearance.ongoing.pulse);
        assert!(appearance.none.dim);
    }

    #[test]
    fn hex_colors() {
        assert_eq!(parse_hex("#F5841F"), Some([0xf5, 0x84, 0x1f, 0xff]));
        assert_eq!(parse_hex("f5841f80"), Some([0xf5, 0x84, 0x1f, 0x80]));
        assert_eq!(parse_hex("#F5841"), None);
        assert_eq!(parse_hex("#GG841F"), None);
        assert_eq!(parse_hex("#ä5841F"), None);
    }

    #[test]
    fn invalid_tiers_are_skipped() {
        let config = Config {
            tiers: vec![
                Tier {
                    within_min: 30,
                    style: style(theme("Warning")),
                },
                Tier {
                    within_min: 5,
                    style: style(theme("Purple")),
                },
                Tier {
                    within_min: 10,
                    style: style(ColorSpec::Rgba("nope".into())),
                },
                Tier {
                    within_min: 30,
                    style: style(theme("Accent")),
                },
                Tier {
                    within_min: 0,
                    style: style(theme("Accent")),
                },
                Tier {
                    within_min: 2,
                    style: style(theme("destructive")),
                },
            ],
            far: style(theme("Nope")),
            ..Config::default()
        };
        let (appearance, warnings) = config.appearance();
        assert_eq!(
            appearance.tiers,
            [
                (
                    2,
                    ResolvedStyle {
                        color: Color::Theme(ThemeColor::Destructive),
                        pulse: false,
                        dim: false
                    }
                ),
                (
                    30,
                    ResolvedStyle {
                        color: Color::Theme(ThemeColor::Warning),
                        pulse: false,
                        dim: false
                    }
                ),
            ]
        );
        assert_eq!(appearance.far, Config::default().appearance().0.far);
        assert_eq!(warnings.len(), 5, "{warnings:?}");
    }

    #[test]
    fn empty_tiers() {
        let config = Config {
            tiers: vec![],
            ..Config::default()
        };
        let (appearance, warnings) = config.appearance();
        assert!(appearance.tiers.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn ron_round_trip_and_lenient_tokens() {
        let tiers: Vec<Tier> = ron::from_str(
            r##"[
                (within_min: 30, style: (color: Theme(Warning), pulse: false)),
                (within_min: 10, style: (color: Rgba("#F5841F"), pulse: true)),
                (within_min: 5,  style: (color: Theme(Purple))),
                (within_min: 2,  style: (color: Theme(Destructive), pulse: true, dim: true)),
            ]"##,
        )
        .unwrap();
        assert_eq!(tiers.len(), 4);
        assert_eq!(tiers[2].style.color, theme("Purple"));
        assert!(tiers[3].style.dim);

        let config = Config::default();
        let text = ron::to_string(&config).unwrap();
        assert!(text.contains("Theme(Accent)"), "{text}");
        let back: Config = ron::from_str(&text).unwrap();
        assert_eq!(back, config);

        // Missing fields fall back to the defaults.
        let partial: Config = ron::from_str("(ongoing_grace_min: 3)").unwrap();
        assert_eq!(partial.ongoing_grace_min, 3);
        assert_eq!(partial.tiers, config.tiers);
    }

    #[test]
    fn url_file_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ical-url");
        std::fs::write(&path, "https://calendar.google.com/x/basic.ics\n").unwrap();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = load_url(&path).unwrap_err().to_string();
        assert!(err.contains("chmod 600"), "{err}");
        assert!(!err.contains("calendar.google.com"), "{err}");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            load_url(&path).unwrap(),
            "https://calendar.google.com/x/basic.ics"
        );

        std::fs::write(&path, "http://calendar.google.com/x/basic.ics").unwrap();
        assert!(load_url(&path).is_err());
    }
}
