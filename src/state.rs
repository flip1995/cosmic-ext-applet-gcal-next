//! PURE: `(events, now, config) -> DisplayState`.
//!
//! Never calls `Utc::now()`; the caller passes the current time and time zone.

use chrono::{DateTime, Duration, Utc};

use crate::{
    config::{Appearance, Config, ResolvedStyle},
    format,
    source::{Event, Snapshot, Status},
};

/// Which of the configured states is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Upcoming, but no tier matches.
    Far,
    /// Upcoming, in the tier with this `within_min`.
    Tier(u32),
    Ongoing,
    None,
    Error,
}

/// The event the pill refers to (shown in the popup, target of *Join*).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventInfo {
    pub summary: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub link: Option<String>,
}

impl From<&Event> for EventInfo {
    fn from(e: &Event) -> Self {
        Self {
            summary: e.summary.clone(),
            start: e.start,
            end: e.end,
            link: e.meeting_link.clone().or_else(|| e.url.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DisplayState {
    pub kind: Kind,
    pub label: String,
    pub style: ResolvedStyle,
    /// `style.pulse`, unless pulsing is disabled globally.
    pub pulse: bool,
    pub event: Option<EventInfo>,
}

/// The relevant event at `now`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next<'a> {
    /// Started at most `ongoing_grace_min` ago and not over yet.
    Ongoing(&'a Event),
    /// The earliest event starting within the lookahead window.
    Upcoming(&'a Event),
    None,
}

/// Upper bound for `lookahead_min`: one week.
pub const MAX_LOOKAHEAD_MIN: u32 = 7 * 24 * 60;

/// How far ahead meetings count, from `lookahead_min` clamped to
/// `1..=MAX_LOOKAHEAD_MIN`.
pub fn lookahead(cfg: &Config) -> Duration {
    Duration::minutes(i64::from(cfg.lookahead_min.clamp(1, MAX_LOOKAHEAD_MIN)))
}

/// The window events are expanded for: one day back (long-running events)
/// to one day past the maximum lookahead, so a fetched snapshot stays usable
/// for any lookahead setting until the next fetch.
pub fn expansion_window(now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let ahead = Duration::minutes(i64::from(MAX_LOOKAHEAD_MIN)) + Duration::days(1);
    (now - Duration::days(1), now + ahead)
}

/// Whether the event counts as a meeting at all (status and the `skip_*`
/// filters).
fn counts(event: &Event, cfg: &Config) -> bool {
    if cfg.skip_all_day && event.all_day {
        return false;
    }
    if event.status == Status::Cancelled {
        return false;
    }
    if cfg.skip_declined
        && let Some(email) = cfg.my_email.as_deref().map(str::trim)
        && !email.is_empty()
        && event.declined_by(email)
    {
        return false;
    }
    if cfg.skip_transparent && event.transparent {
        return false;
    }
    if cfg.skip_zero_duration && event.end <= event.start {
        return false;
    }
    true
}

pub fn next_event<'a>(events: &'a [Event], now: DateTime<Utc>, cfg: &Config) -> Next<'a> {
    let grace = Duration::minutes(i64::from(cfg.ongoing_grace_min));
    let candidates = || events.iter().filter(|e| counts(e, cfg));

    // If several meetings are in their grace period, the one that started
    // last is the more relevant one.
    let ongoing = candidates()
        .filter(|e| e.start <= now && now < e.end && now - e.start < grace)
        .max_by(|a, b| {
            a.start
                .cmp(&b.start)
                .then_with(|| b.summary.cmp(&a.summary))
        });
    if let Some(event) = ongoing {
        return Next::Ongoing(event);
    }

    candidates()
        .filter(|e| e.start > now && e.start <= now + lookahead(cfg))
        .min_by(|a, b| {
            a.start
                .cmp(&b.start)
                .then_with(|| a.summary.cmp(&b.summary))
        })
        .map_or(Next::None, Next::Upcoming)
}

/// Compute what the pill shows.
///
/// `snapshot` is the last successful fetch (or the disk cache); `None` if
/// there never was one.
pub fn compute(
    snapshot: Option<&Snapshot>,
    now: DateTime<Utc>,
    cfg: &Config,
    appearance: &Appearance,
) -> DisplayState {
    let state = |kind, label: String, style: ResolvedStyle, event: Option<&Event>| DisplayState {
        kind,
        label,
        style,
        pulse: style.pulse && cfg.pulse_enabled,
        event: event.map(EventInfo::from),
    };

    let stale_after = Duration::minutes(i64::from(cfg.stale_after_min));
    let Some(snapshot) =
        snapshot.filter(|s| now - DateTime::<Utc>::from(s.fetched_at) <= stale_after)
    else {
        return state(Kind::Error, "!".into(), appearance.error, None);
    };

    match next_event(&snapshot.events, now, cfg) {
        Next::None => state(Kind::None, cfg.none_label.clone(), appearance.none, None),
        Next::Ongoing(event) => state(
            Kind::Ongoing,
            cfg.ongoing_label.clone(),
            appearance.ongoing,
            Some(event),
        ),
        Next::Upcoming(event) => {
            let remaining = event.start - now;
            let minutes = format::minutes_left(remaining);
            let (kind, style) = appearance
                .tiers
                .iter()
                .find(|(within, _)| minutes <= i64::from(*within))
                .map_or((Kind::Far, appearance.far), |(within, style)| {
                    (Kind::Tier(*within), *style)
                });
            state(kind, format::remaining(remaining), style, Some(event))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::{
        config::{Color, ThemeColor},
        source::{Attendee, PartStat},
    };

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn event(summary: &str, start: &str, minutes: i64) -> Event {
        let start = utc(start);
        Event {
            uid: summary.into(),
            summary: summary.into(),
            start,
            end: start + Duration::minutes(minutes),
            all_day: false,
            status: Status::Confirmed,
            transparent: false,
            attendees: vec![],
            meeting_link: None,
            url: None,
        }
    }

    fn snapshot(events: Vec<Event>, fetched_at: &str) -> Snapshot {
        Snapshot {
            events,
            fetched_at: SystemTime::from(utc(fetched_at)),
        }
    }

    fn show(events: Vec<Event>, now: &str) -> DisplayState {
        show_with(events, now, &Config::default())
    }

    fn show_with(events: Vec<Event>, now: &str, cfg: &Config) -> DisplayState {
        let now = utc(now);
        let snap = snapshot(events, &now.to_rfc3339());
        compute(Some(&snap), now, cfg, &cfg.appearance().0)
    }

    #[test]
    fn tier_boundaries() {
        let now = "2026-11-10T10:00:00+01:00";
        let at = |secs: i64| {
            let start = (utc(now) + Duration::seconds(secs)).to_rfc3339();
            let s = show(vec![event("M", &start, 30)], now);
            (s.kind, s.label, s.pulse)
        };
        let far = |l: &str| (Kind::Far, l.to_owned(), false);
        let tier = |m, l: &str, p| (Kind::Tier(m), l.to_owned(), p);

        // A level applies while the label shows at most its minutes.
        assert_eq!(at(1), tier(1, "1m", true));
        assert_eq!(at(60), tier(1, "1m", true));
        assert_eq!(at(61), tier(5, "2m", false));
        assert_eq!(at(5 * 60), tier(5, "5m", false));
        assert_eq!(at(5 * 60 + 1), tier(10, "6m", false));
        assert_eq!(at(10 * 60), tier(10, "10m", false));
        assert_eq!(at(10 * 60 + 1), tier(30, "11m", false));
        assert_eq!(at(30 * 60), tier(30, "30m", false));
        assert_eq!(at(30 * 60 + 1), tier(60, "31m", false));
        assert_eq!(at(60 * 60), tier(60, "1h", false));
        assert_eq!(at(60 * 60 + 1), far("1h 1m"));
        assert_eq!(at(80 * 60), far("1h 20m"));
    }

    #[test]
    fn colors_follow_tiers() {
        let s = show(
            vec![event("M", "2026-11-10T10:08:00+01:00", 30)],
            "2026-11-10T10:00:00+01:00",
        );
        assert_eq!(s.style.color, Color::Theme(ThemeColor::Success));

        let mut custom = Config::default();
        custom.tiers[2].style.color = crate::config::ColorSpec::Rgba("#F5841F".into());
        let s = show_with(
            vec![event("M", "2026-11-10T10:08:00+01:00", 30)],
            "2026-11-10T10:00:00+01:00",
            &custom,
        );
        assert_eq!(s.style.color, Color::Rgba([0xf5, 0x84, 0x1f, 0xff]));
        let s = show(
            vec![event("M", "2026-11-10T12:00:00+01:00", 30)],
            "2026-11-10T10:00:00+01:00",
        );
        assert_eq!(s.style.color, Color::Theme(ThemeColor::Neutral));
    }

    #[test]
    fn ongoing_grace() {
        let events = || vec![event("M", "2026-11-10T10:00:00+01:00", 30)];
        let at = |now: &str| show(events(), now).kind;
        assert_eq!(at("2026-11-10T10:00:00+01:00"), Kind::Ongoing);
        assert_eq!(at("2026-11-10T10:04:59+01:00"), Kind::Ongoing);
        assert_eq!(at("2026-11-10T10:05:00+01:00"), Kind::None);

        let s = show(events(), "2026-11-10T10:01:00+01:00");
        assert_eq!(s.label, "now");
        // The default "ongoing" style does not pulse.
        assert!(!s.pulse);
        assert_eq!(s.event.unwrap().summary, "M");

        // Short meetings stop being "ongoing" when they end.
        let short = vec![event("S", "2026-11-10T10:00:00+01:00", 2)];
        assert_eq!(show(short, "2026-11-10T10:03:00+01:00").kind, Kind::None);

        // Grace 0 disables the state.
        let cfg = Config {
            ongoing_grace_min: 0,
            ..Config::default()
        };
        assert_eq!(
            show_with(events(), "2026-11-10T10:00:00+01:00", &cfg).kind,
            Kind::None
        );
    }

    #[test]
    fn after_grace_shows_next_meeting() {
        let events = vec![
            event("A", "2026-11-10T10:00:00+01:00", 60),
            event("B", "2026-11-10T10:45:00+01:00", 30),
        ];
        let s = show(events, "2026-11-10T10:20:00+01:00");
        assert_eq!(s.label, "25m");
        assert_eq!(s.event.unwrap().summary, "B");
    }

    #[test]
    fn most_recent_ongoing_wins_over_upcoming() {
        let events = vec![
            event("Early", "2026-11-10T09:57:00+01:00", 30),
            event("Now", "2026-11-10T10:00:00+01:00", 30),
            event("Soon", "2026-11-10T10:01:00+01:00", 30),
        ];
        let s = show(events, "2026-11-10T10:00:30+01:00");
        assert_eq!(s.kind, Kind::Ongoing);
        assert_eq!(s.event.unwrap().summary, "Now");
    }

    #[test]
    fn lookahead_limit() {
        let now = "2026-11-10T10:00:00+01:00";
        let at = |start: &str, cfg: &Config| show_with(vec![event("M", start, 30)], now, cfg);
        let default = Config::default();

        // Default: 2 hours, inclusive.
        let s = at("2026-11-10T12:00:00+01:00", &default);
        assert_eq!((s.kind, s.label.as_str()), (Kind::Far, "2h"));
        let s = at("2026-11-10T12:00:01+01:00", &default);
        assert_eq!((s.kind, s.label.as_str()), (Kind::None, "No meeting"));
        assert!(s.event.is_none());

        // Meetings on the next day count when they are close enough.
        let late = "2026-11-10T23:30:00+01:00";
        let s = show_with(
            vec![event("Early", "2026-11-11T00:15:00+01:00", 30)],
            late,
            &default,
        );
        assert_eq!(s.label, "45m");

        let wide = Config {
            lookahead_min: 24 * 60,
            ..Config::default()
        };
        assert_eq!(at("2026-11-11T09:00:00+01:00", &wide).label, "23h");
        // Out-of-range values are clamped instead of disabling everything.
        let zero = Config {
            lookahead_min: 0,
            ..Config::default()
        };
        assert_eq!(at("2026-11-10T10:00:30+01:00", &zero).label, "1m");
        assert_eq!(at("2026-11-10T10:02:00+01:00", &zero).kind, Kind::None);
    }

    #[test]
    fn expansion_covers_max_lookahead() {
        let now = utc("2026-11-10T10:00:00Z");
        let (from, to) = expansion_window(now);
        assert_eq!(from, utc("2026-11-09T10:00:00Z"));
        assert!(to > now + Duration::minutes(i64::from(MAX_LOOKAHEAD_MIN)));
    }

    #[test]
    fn remaining_time_across_dst_change() {
        // 01:30 CET -> 03:30 CEST is one hour, not two.
        let events = vec![event("M", "2026-03-29T03:30:00+02:00", 30)];
        assert_eq!(show(events, "2026-03-29T01:30:00+01:00").label, "1h");
        // 01:30 CEST -> 02:30 CET (after the clocks go back) is two hours.
        let events = vec![event("M", "2026-10-25T02:30:00+01:00", 30)];
        assert_eq!(show(events, "2026-10-25T01:30:00+02:00").label, "2h");
    }

    #[test]
    fn filters() {
        let now = "2026-11-10T10:00:00+01:00";
        let base = || event("M", "2026-11-10T10:30:00+01:00", 30);
        let kind = |e: Event, cfg: &Config| show_with(vec![e], now, cfg).kind;
        let default = Config::default();

        let all_day = Event {
            all_day: true,
            ..base()
        };
        assert_eq!(kind(all_day.clone(), &default), Kind::None);
        assert_eq!(
            kind(
                all_day,
                &Config {
                    skip_all_day: false,
                    ..Config::default()
                }
            ),
            Kind::Tier(30)
        );

        let cancelled = Event {
            status: Status::Cancelled,
            ..base()
        };
        assert_eq!(kind(cancelled, &default), Kind::None);
        let tentative = Event {
            status: Status::Tentative,
            ..base()
        };
        assert_eq!(kind(tentative, &default), Kind::Tier(30));

        let declined = Event {
            attendees: vec![Attendee {
                email: "Me@Example.com".into(),
                partstat: PartStat::Declined,
            }],
            ..base()
        };
        let with_email = Config {
            my_email: Some("me@example.com".into()),
            ..Config::default()
        };
        assert_eq!(kind(declined.clone(), &default), Kind::Tier(30));
        assert_eq!(kind(declined.clone(), &with_email), Kind::None);
        let keep = Config {
            skip_declined: false,
            ..with_email.clone()
        };
        assert_eq!(kind(declined, &keep), Kind::Tier(30));

        let free = Event {
            transparent: true,
            ..base()
        };
        assert_eq!(kind(free.clone(), &default), Kind::Tier(30));
        assert_eq!(
            kind(
                free,
                &Config {
                    skip_transparent: true,
                    ..Config::default()
                }
            ),
            Kind::None
        );

        let zero = event("Z", "2026-11-10T10:30:00+01:00", 0);
        assert_eq!(kind(zero.clone(), &default), Kind::Tier(30));
        assert_eq!(
            kind(
                zero,
                &Config {
                    skip_zero_duration: true,
                    ..Config::default()
                }
            ),
            Kind::None
        );
    }

    #[test]
    fn error_and_staleness() {
        let cfg = Config::default();
        let appearance = cfg.appearance().0;
        let now = utc("2026-11-10T10:00:00+01:00");
        let s = compute(None, now, &cfg, &appearance);
        assert_eq!((s.kind, s.label.as_str()), (Kind::Error, "!"));
        assert!(s.style.dim);

        let fresh = snapshot(vec![], "2026-11-10T09:00:00+01:00");
        assert_eq!(
            compute(Some(&fresh), now, &cfg, &appearance).kind,
            Kind::None
        );
        let stale = snapshot(vec![], "2026-11-10T08:59:59+01:00");
        assert_eq!(
            compute(Some(&stale), now, &cfg, &appearance).kind,
            Kind::Error
        );
    }

    #[test]
    fn custom_tiers_and_pulse_switch() {
        let now = "2026-11-10T10:00:00+01:00";
        let soon = || vec![event("M", "2026-11-10T10:01:00+01:00", 30)];
        let later = || vec![event("M", "2026-11-10T10:20:00+01:00", 30)];

        let none = Config {
            tiers: vec![],
            ..Config::default()
        };
        assert_eq!(show_with(soon(), now, &none).kind, Kind::Far);

        let mut single = Config::default();
        single.tiers.truncate(1);
        single.tiers[0].within_min = 15;
        assert_eq!(show_with(soon(), now, &single).kind, Kind::Tier(15));
        assert_eq!(show_with(later(), now, &single).kind, Kind::Far);

        let mut unsorted = Config::default();
        unsorted.tiers.reverse();
        assert_eq!(show_with(soon(), now, &unsorted).kind, Kind::Tier(1));

        let calm = Config {
            pulse_enabled: false,
            ..Config::default()
        };
        let s = show_with(soon(), now, &calm);
        assert!(s.style.pulse && !s.pulse);
    }

    #[test]
    fn custom_labels() {
        let cfg = Config {
            none_label: "Free".into(),
            ongoing_label: "Go!".into(),
            ..Config::default()
        };
        let now = "2026-11-10T10:00:00+01:00";
        assert_eq!(show_with(vec![], now, &cfg).label, "Free");
        let events = vec![event("M", "2026-11-10T09:59:00+01:00", 30)];
        assert_eq!(show_with(events, now, &cfg).label, "Go!");
    }
}
