//! Calendar data sources and the event model shared by all of them.

pub mod fetch;
pub mod ics;
pub mod links;

use std::{path::PathBuf, time::SystemTime};

use anyhow::Result;
use chrono::{DateTime, Utc};

/// A single, already expanded event instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub uid: String,
    pub summary: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// `DTSTART` was a `DATE`, not a `DATE-TIME`.
    pub all_day: bool,
    pub status: Status,
    /// `TRANSP:TRANSPARENT` ("free" in Google Calendar).
    pub transparent: bool,
    pub attendees: Vec<Attendee>,
    /// Video meeting link (Meet/Zoom/Teams), if one could be found.
    pub meeting_link: Option<String>,
    /// The event's own `URL` property, if any.
    pub url: Option<String>,
}

impl Event {
    /// Whether the attendee with the given e-mail address declined this event.
    pub fn declined_by(&self, email: &str) -> bool {
        self.attendees
            .iter()
            .any(|a| a.partstat == PartStat::Declined && a.email.eq_ignore_ascii_case(email))
    }

    /// Whether `start < to` and the event is not over at `from`.
    ///
    /// Zero-duration events count if they start inside `[from, to)`.
    pub fn overlaps(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> bool {
        self.start < to && (self.end > from || self.start >= from)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Confirmed,
    Tentative,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attendee {
    /// E-mail address without the `mailto:` prefix.
    pub email: String,
    pub partstat: PartStat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PartStat {
    #[default]
    NeedsAction,
    Accepted,
    Declined,
    Tentative,
    Delegated,
    Other,
}

/// Where the calendar comes from.
#[derive(Clone)]
pub enum Location {
    /// The secret iCal address. Never log this.
    Url(String),
    /// A local `.ics` file (for testing and `--ics-file`).
    File(PathBuf),
}

impl std::fmt::Debug for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The URL is a bearer secret, keep it out of any debug output.
            Self::Url(_) => f.write_str("Url(<redacted>)"),
            Self::File(path) => f.debug_tuple("File").field(path).finish(),
        }
    }
}

/// A source of calendar events.
pub trait EventSource {
    /// Return all event instances overlapping `[from, to)`, already expanded.
    async fn events(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<Event>>;
}

/// The result of a successful fetch.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub events: Vec<Event>,
    /// When the data was fetched (for the disk cache: its modification time).
    pub fetched_at: SystemTime,
}

/// [`EventSource`] backed by an ICS feed (Google's secret iCal address).
pub struct IcsSource {
    fetcher: fetch::Fetcher,
    local: rrule::Tz,
}

impl IcsSource {
    pub fn new(fetcher: fetch::Fetcher, local: rrule::Tz) -> Self {
        Self { fetcher, local }
    }

    /// Events from the on-disk cache of the last successful fetch, if any.
    pub async fn cached(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Option<Snapshot> {
        let (body, fetched_at) = self.fetcher.load_cache().await?;
        snapshot_from_body(body, fetched_at, self.local, from, to).await
    }
}

impl std::fmt::Debug for IcsSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IcsSource").finish_non_exhaustive()
    }
}

/// Events from the cache file at `path`, without needing the feed URL.
pub async fn cached_snapshot(
    path: &std::path::Path,
    local: rrule::Tz,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Option<Snapshot> {
    let (body, fetched_at) = fetch::load_cache(path).await?;
    snapshot_from_body(body, fetched_at, local, from, to).await
}

async fn snapshot_from_body(
    body: String,
    fetched_at: SystemTime,
    local: rrule::Tz,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Option<Snapshot> {
    match parse_and_expand(body, local, from, to).await {
        Ok(events) => Some(Snapshot { events, fetched_at }),
        Err(err) => {
            tracing::warn!("ignoring the cache: {err:#}");
            None
        }
    }
}

impl EventSource for IcsSource {
    async fn events(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<Event>> {
        let body = self.fetcher.fetch().await?;
        let events = parse_and_expand(body.clone(), self.local, from, to).await?;
        // Only cache feeds that parsed, so a broken response never replaces a
        // good cache.
        self.fetcher.store_cache(&body).await;
        Ok(events)
    }
}

/// Parse and expand off the async executor: feeds can be large.
async fn parse_and_expand(
    body: String,
    local: rrule::Tz,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<Event>> {
    tokio::task::spawn_blocking(move || {
        let calendar = ics::Calendar::parse(&body, local)?;
        Ok(calendar.expand(from, to))
    })
    .await?
}
