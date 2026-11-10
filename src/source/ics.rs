//! Minimal, forgiving ICS (RFC 5545) parser plus recurrence expansion.
//!
//! Only the parts needed to find the next meeting are parsed: `VEVENT`s and
//! their timing, status and attendee properties. Malformed lines and events are
//! skipped (and counted) instead of failing the whole feed.
//!
//! Nothing in here logs event contents or the feed URL; warnings only contain
//! counts and the kind of problem.

use std::{collections::HashMap, str::FromStr};

use anyhow::{Result, bail};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use rrule::{RRule, RRuleSet, Tz, Unvalidated};

use super::{Attendee, Event, PartStat, Status, links};

/// Upper bound of generated instances per recurring event and query window.
const MAX_INSTANCES: u16 = 1000;

/// A parsed calendar, ready to be expanded for arbitrary windows.
#[derive(Debug, Default)]
pub struct Calendar {
    /// Events without `RECURRENCE-ID`: single events and recurrence masters.
    masters: Vec<VEvent>,
    /// Events with `RECURRENCE-ID`: modified (or cancelled) instances.
    overrides: Vec<VEvent>,
    /// `(uid, recurrence-id)` of all overrides, used to suppress the generated
    /// instance they replace.
    overridden: HashMap<String, Vec<DateTime<Utc>>>,
    /// Number of `VEVENT`s that were skipped because they were malformed.
    pub skipped: usize,
}

#[derive(Debug)]
struct VEvent {
    /// The first instance (`DTSTART`); all fields besides start/end are copied
    /// to every generated instance.
    template: Event,
    dtstart: DateTime<Tz>,
    /// `DTEND - DTSTART`, or `DURATION`.
    duration: Duration,
    rrules: Vec<String>,
    rdates: Vec<DateTime<Tz>>,
    exdates: Vec<DateTime<Tz>>,
    recurrence_id: Option<DateTime<Utc>>,
}

impl Calendar {
    /// Parse an ICS document. Floating times and dates are interpreted in
    /// `local`.
    ///
    /// Fails only if the input is not an iCalendar document at all; individual
    /// broken events are skipped.
    pub fn parse(input: &str, local: Tz) -> Result<Self> {
        let lines = unfold(input);
        if !lines
            .iter()
            .any(|l| l.trim_end().eq_ignore_ascii_case("BEGIN:VCALENDAR"))
        {
            bail!("response is not an iCalendar document");
        }

        let mut calendar = Calendar::default();
        let mut bad_lines = 0usize;
        // Properties of the VEVENT being parsed, `None` outside of VEVENTs.
        let mut current: Option<Vec<Property>> = None;
        // Nesting depth of sub-components (VALARM) inside the current VEVENT.
        let mut nested = 0usize;

        for line in &lines {
            let Some(prop) = Property::parse(line) else {
                if !line.trim().is_empty() {
                    bad_lines += 1;
                }
                continue;
            };
            match (prop.name.as_str(), &mut current) {
                ("BEGIN", None) if prop.value.eq_ignore_ascii_case("VEVENT") => {
                    current = Some(Vec::new());
                    nested = 0;
                }
                ("BEGIN", Some(_)) => nested += 1,
                ("END", Some(_)) if nested > 0 => nested -= 1,
                ("END", Some(_)) => {
                    let props = current.take().unwrap_or_default();
                    match VEvent::from_properties(&props, local) {
                        Ok(event) => calendar.push(event),
                        Err(err) => {
                            calendar.skipped += 1;
                            tracing::debug!("skipping malformed event: {err}");
                        }
                    }
                }
                (_, Some(props)) if nested == 0 => props.push(prop),
                _ => {}
            }
        }

        if bad_lines > 0 {
            tracing::warn!("ignored {bad_lines} malformed ICS line(s)");
        }
        if calendar.skipped > 0 {
            tracing::warn!("skipped {} malformed event(s)", calendar.skipped);
        }
        Ok(calendar)
    }

    fn push(&mut self, event: VEvent) {
        match event.recurrence_id {
            Some(rid) => {
                self.overridden
                    .entry(event.template.uid.clone())
                    .or_default()
                    .push(rid);
                self.overrides.push(event);
            }
            None => self.masters.push(event),
        }
    }

    /// All event instances overlapping `[from, to)`, sorted by start time.
    ///
    /// Cancelled overrides remove their instance; other cancelled events are
    /// kept (with [`Status::Cancelled`]) and filtered later.
    pub fn expand(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<Event> {
        let mut out = Vec::new();
        for master in &self.masters {
            let overridden = self
                .overridden
                .get(&master.template.uid)
                .map(Vec::as_slice)
                .unwrap_or_default();
            if let Err(err) = master.expand_into(from, to, overridden, &mut out) {
                tracing::warn!("failed to expand a recurring event: {err}");
            }
        }
        // Overrides stand on their own: they may have been moved into (or out
        // of) the window, and Google also emits them without a master when you
        // are invited to a single instance of a series.
        for ov in &self.overrides {
            if ov.template.status == Status::Cancelled {
                continue;
            }
            if ov.template.overlaps(from, to) {
                out.push(ov.template.clone());
            }
        }
        out.sort_by(|a, b| {
            a.start
                .cmp(&b.start)
                .then_with(|| a.summary.cmp(&b.summary))
        });
        out
    }
}

impl VEvent {
    fn from_properties(props: &[Property], local: Tz) -> Result<Self> {
        let get = |name: &str| props.iter().find(|p| p.name == name);

        let Some(dtstart_prop) = get("DTSTART") else {
            bail!("missing DTSTART");
        };
        let start = parse_date_time(dtstart_prop, &dtstart_prop.value, local)?;
        let all_day = start.is_date;
        let dtstart = start.value;

        let duration = if let Some(end) = get("DTEND") {
            let end = parse_date_time(end, &end.value, local)?.value;
            end.signed_duration_since(dtstart)
        } else if let Some(duration) = get("DURATION") {
            parse_duration(&duration.value)?
        } else if all_day {
            // RFC 5545 3.6.1: a DATE start without end lasts one day.
            Duration::days(1)
        } else {
            Duration::zero()
        };
        if duration < Duration::zero() {
            bail!("event ends before it starts");
        }

        let mut rrules = Vec::new();
        let mut rdates = Vec::new();
        let mut exdates = Vec::new();
        let mut attendees = Vec::new();
        for prop in props {
            match prop.name.as_str() {
                "RRULE" => rrules.push(normalize_rrule(&prop.value, dtstart, local)?),
                "RDATE" | "EXDATE" => {
                    let list = if prop.name == "RDATE" {
                        &mut rdates
                    } else {
                        &mut exdates
                    };
                    for value in prop.value.split(',') {
                        // RDATE;VALUE=PERIOD: only the start matters.
                        let value = value.split('/').next().unwrap_or(value);
                        list.push(parse_date_time(prop, value, local)?.value);
                    }
                }
                "ATTENDEE" => attendees.push(Attendee {
                    email: strip_mailto(&prop.value).to_owned(),
                    partstat: prop
                        .param("PARTSTAT")
                        .map_or(PartStat::NeedsAction, parse_partstat),
                }),
                _ => {}
            }
        }

        let recurrence_id = match get("RECURRENCE-ID") {
            Some(rid) => Some(
                parse_date_time(rid, &rid.value, local)?
                    .value
                    .with_timezone(&Utc),
            ),
            None => None,
        };

        let text = |name: &str| get(name).map(|p| unescape_text(&p.value));
        let location = text("LOCATION");
        let description = text("DESCRIPTION");
        let conference = get("X-GOOGLE-CONFERENCE").map(|p| p.value.trim().to_owned());
        let meeting_link = links::meeting_link(
            conference.as_deref(),
            location.as_deref(),
            description.as_deref(),
        );

        let start_utc = dtstart.with_timezone(&Utc);
        let template = Event {
            uid: get("UID").map(|p| p.value.clone()).unwrap_or_default(),
            summary: text("SUMMARY").unwrap_or_default(),
            start: start_utc,
            end: start_utc + duration,
            all_day,
            status: match get("STATUS")
                .map(|p| p.value.to_ascii_uppercase())
                .as_deref()
            {
                Some("CANCELLED") => Status::Cancelled,
                Some("TENTATIVE") => Status::Tentative,
                _ => Status::Confirmed,
            },
            transparent: get("TRANSP").is_some_and(|p| p.value.eq_ignore_ascii_case("TRANSPARENT")),
            attendees,
            meeting_link,
            url: get("URL").map(|p| p.value.trim().to_owned()),
        };

        Ok(Self {
            template,
            dtstart,
            duration,
            rrules,
            rdates,
            exdates,
            recurrence_id,
        })
    }

    fn is_recurring(&self) -> bool {
        !self.rrules.is_empty() || !self.rdates.is_empty()
    }

    fn instance(&self, start: DateTime<Tz>) -> Event {
        let start_utc = start.with_timezone(&Utc);
        let end = if self.template.all_day {
            // Keep all-day events aligned to local midnight across DST changes.
            let days = self.duration.num_days();
            let date = start.date_naive() + Duration::days(days);
            resolve_local(&start.timezone(), date.and_time(NaiveTime::MIN)).with_timezone(&Utc)
        } else {
            start_utc + self.duration
        };
        Event {
            start: start_utc,
            end,
            ..self.template.clone()
        }
    }

    fn expand_into(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        overridden: &[DateTime<Utc>],
        out: &mut Vec<Event>,
    ) -> Result<()> {
        if !self.is_recurring() {
            if self.template.overlaps(from, to) {
                out.push(self.template.clone());
            }
            return Ok(());
        }

        let mut set = RRuleSet::new(self.dtstart);
        for rule in &self.rrules {
            set = set.rrule(RRule::<Unvalidated>::from_str(rule)?.validate(self.dtstart)?);
        }
        if self.rrules.is_empty() {
            // Without an RRULE the set only yields RDATEs, but DTSTART is
            // always the first instance.
            set = set.rdate(self.dtstart);
        }
        for rdate in &self.rdates {
            set = set.rdate(*rdate);
        }
        for exdate in &self.exdates {
            set = set.exdate(*exdate);
        }

        // Instances that started before `from` may still be running.
        let tz = self.dtstart.timezone();
        let after = (from - self.duration - Duration::seconds(1)).with_timezone(&tz);
        let before = to.with_timezone(&tz);
        let result = set.after(after).before(before).all(MAX_INSTANCES);
        if result.limited {
            tracing::warn!("recurrence expansion hit the instance limit");
        }

        for start in result.dates {
            if overridden.contains(&start.with_timezone(&Utc)) {
                continue;
            }
            let instance = self.instance(start);
            if instance.overlaps(from, to) {
                out.push(instance);
            }
        }
        Ok(())
    }
}

/// One content line: `NAME;PARAM=value;…:value`.
#[derive(Debug, PartialEq, Eq)]
struct Property {
    /// Upper-cased.
    name: String,
    /// Keys upper-cased, values unquoted.
    params: Vec<(String, String)>,
    value: String,
}

impl Property {
    fn parse(line: &str) -> Option<Self> {
        // The name ends at the first ';' or ':'; parameter values may contain
        // both when quoted.
        let name_end = line.find([';', ':'])?;
        let name = line[..name_end].trim();
        if name.is_empty() {
            return None;
        }

        let mut params = Vec::new();
        let mut rest = &line[name_end..];
        while let Some(stripped) = rest.strip_prefix(';') {
            let eq = stripped.find('=')?;
            let key = stripped[..eq].trim().to_ascii_uppercase();
            let mut after = &stripped[eq + 1..];
            let mut value = String::new();
            // A parameter can have several comma-separated (quoted) values.
            loop {
                if let Some(quoted) = after.strip_prefix('"') {
                    let close = quoted.find('"')?;
                    value.push_str(&quoted[..close]);
                    after = &quoted[close + 1..];
                } else {
                    let end = after.find([',', ';', ':'])?;
                    value.push_str(&after[..end]);
                    after = &after[end..];
                }
                match after.strip_prefix(',') {
                    Some(next) => {
                        value.push(',');
                        after = next;
                    }
                    None => break,
                }
            }
            params.push((key, value));
            rest = after;
        }
        let value = rest.strip_prefix(':')?;

        Some(Self {
            name: name.to_ascii_uppercase(),
            params,
            value: value.to_owned(),
        })
    }

    fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Split into logical lines, joining folded continuation lines (RFC 5545 3.1).
fn unfold(input: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in input.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        match (raw.strip_prefix([' ', '\t']), lines.last_mut()) {
            (Some(cont), Some(last)) => last.push_str(cont),
            _ => lines.push(raw.to_owned()),
        }
    }
    lines
}

/// Undo TEXT escaping (RFC 5545 3.3.11).
fn unescape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn strip_mailto(value: &str) -> &str {
    let value = value.trim();
    match value.get(..7) {
        Some(prefix) if prefix.eq_ignore_ascii_case("mailto:") => &value[7..],
        _ => value,
    }
}

fn parse_partstat(value: &str) -> PartStat {
    match value.to_ascii_uppercase().as_str() {
        "NEEDS-ACTION" => PartStat::NeedsAction,
        "ACCEPTED" => PartStat::Accepted,
        "DECLINED" => PartStat::Declined,
        "TENTATIVE" => PartStat::Tentative,
        "DELEGATED" => PartStat::Delegated,
        _ => PartStat::Other,
    }
}

struct ParsedTime {
    value: DateTime<Tz>,
    is_date: bool,
}

/// Parse a `DATE` or `DATE-TIME` value, honouring `VALUE=DATE` and `TZID`.
fn parse_date_time(prop: &Property, value: &str, local: Tz) -> Result<ParsedTime> {
    let value = value.trim();
    let is_date = prop
        .param("VALUE")
        .is_some_and(|v| v.eq_ignore_ascii_case("DATE"))
        || (value.len() == 8 && !value.contains('T'));

    if is_date {
        let date = NaiveDate::parse_from_str(value, "%Y%m%d")?;
        let value = resolve_local(&local, date.and_time(NaiveTime::MIN));
        return Ok(ParsedTime { value, is_date });
    }

    if let Some(utc) = value.strip_suffix(['Z', 'z']) {
        let naive = NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S")?;
        return Ok(ParsedTime {
            value: Utc.from_utc_datetime(&naive).with_timezone(&Tz::UTC),
            is_date,
        });
    }

    let naive = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")?;
    let tz = prop
        .param("TZID")
        .map_or(local, |tzid| resolve_tzid(tzid, local));
    Ok(ParsedTime {
        value: resolve_local(&tz, naive),
        is_date,
    })
}

/// Convert a local wall-clock time to an instant. Ambiguous times (DST end)
/// use the earlier instant, non-existent ones (DST start) are moved forward by
/// the gap, like most calendar apps do.
pub(crate) fn resolve_local<T: TimeZone>(tz: &T, naive: NaiveDateTime) -> DateTime<T> {
    let mut probe = naive;
    for _ in 0..4 {
        if let Some(dt) = tz.from_local_datetime(&probe).earliest() {
            return dt;
        }
        probe += Duration::minutes(30);
    }
    tz.from_utc_datetime(&naive)
}

/// Map a `TZID` to a time zone: IANA names, prefixed IANA names
/// (`/mozilla.org/…/Europe/Berlin`) and a few common Windows names. Unknown
/// zones fall back to `local`.
fn resolve_tzid(tzid: &str, local: Tz) -> Tz {
    let tzid = tzid.trim();
    if let Ok(tz) = chrono_tz::Tz::from_str(tzid) {
        return tz.into();
    }
    // Try the last one or two path segments of prefixed IDs.
    let segments: Vec<&str> = tzid.split('/').filter(|s| !s.is_empty()).collect();
    for n in [2, 1] {
        if segments.len() >= n {
            let candidate = segments[segments.len() - n..].join("/");
            if let Ok(tz) = chrono_tz::Tz::from_str(&candidate) {
                return tz.into();
            }
        }
    }
    if let Some(iana) = windows_zone(tzid)
        && let Ok(tz) = chrono_tz::Tz::from_str(iana)
    {
        return tz.into();
    }
    tracing::debug!("unknown TZID, using the local time zone");
    local
}

fn windows_zone(name: &str) -> Option<&'static str> {
    Some(match name {
        "UTC" | "Coordinated Universal Time" => "UTC",
        "GMT Standard Time" => "Europe/London",
        "W. Europe Standard Time" => "Europe/Berlin",
        "Central Europe Standard Time" => "Europe/Budapest",
        "Central European Standard Time" => "Europe/Warsaw",
        "Romance Standard Time" => "Europe/Paris",
        "E. Europe Standard Time" => "Europe/Chisinau",
        "FLE Standard Time" => "Europe/Kiev",
        "GTB Standard Time" => "Europe/Bucharest",
        "Russian Standard Time" => "Europe/Moscow",
        "Eastern Standard Time" => "America/New_York",
        "Central Standard Time" => "America/Chicago",
        "Mountain Standard Time" => "America/Denver",
        "Pacific Standard Time" => "America/Los_Angeles",
        "India Standard Time" => "Asia/Kolkata",
        "China Standard Time" => "Asia/Shanghai",
        "Tokyo Standard Time" => "Asia/Tokyo",
        "Singapore Standard Time" => "Asia/Singapore",
        "AUS Eastern Standard Time" => "Australia/Sydney",
        _ => return None,
    })
}

/// Rewrite `UNTIL` to UTC. `rrule` insists on a UTC `UNTIL` when `DTSTART` has
/// a `TZID`, but feeds also contain floating or date-only values.
fn normalize_rrule(rule: &str, dtstart: DateTime<Tz>, local: Tz) -> Result<String> {
    let rule = rule.trim();
    let rule = rule.strip_prefix("RRULE:").unwrap_or(rule);
    let parts = rule.split(';').filter(|p| !p.is_empty()).map(|part| {
        let Some((key, value)) = part.split_once('=') else {
            return Ok(part.to_owned());
        };
        if !key.eq_ignore_ascii_case("UNTIL") || value.ends_with(['Z', 'z']) {
            return Ok(part.to_owned());
        }
        let tz = if dtstart.timezone().is_local() {
            local
        } else {
            dtstart.timezone()
        };
        let naive = if value.len() == 8 {
            // Date-only UNTIL is inclusive: allow instances during that day.
            let date = NaiveDate::parse_from_str(value, "%Y%m%d")?;
            date.and_hms_opt(23, 59, 59).unwrap_or_default()
        } else {
            NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")?
        };
        let until = resolve_local(&tz, naive).with_timezone(&Utc);
        Ok(format!("UNTIL={}", until.format("%Y%m%dT%H%M%SZ")))
    });
    Ok(parts.collect::<Result<Vec<_>>>()?.join(";"))
}

/// Parse an RFC 5545 `DURATION` value (`P1DT2H`, `PT15M`, `P1W`, `-PT5M`).
fn parse_duration(value: &str) -> Result<Duration> {
    let value = value.trim();
    let (negative, value) = match value.as_bytes().first() {
        Some(b'-') => (true, &value[1..]),
        Some(b'+') => (false, &value[1..]),
        _ => (false, value),
    };
    let Some(mut rest) = value.strip_prefix('P') else {
        bail!("invalid DURATION");
    };
    let mut total = Duration::zero();
    let mut in_time = false;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('T') {
            in_time = true;
            rest = r;
            continue;
        }
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if digits == 0 || digits == rest.len() {
            bail!("invalid DURATION");
        }
        let n: i64 = rest[..digits].parse()?;
        total += match (&rest[digits..=digits], in_time) {
            ("W", false) => Duration::weeks(n),
            ("D", false) => Duration::days(n),
            ("H", true) => Duration::hours(n),
            ("M", true) => Duration::minutes(n),
            ("S", true) => Duration::seconds(n),
            _ => bail!("invalid DURATION"),
        };
        rest = &rest[digits + 1..];
    }
    Ok(if negative { -total } else { total })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BERLIN: Tz = Tz::Europe__Berlin;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn calendar(body: &str) -> Calendar {
        let ics = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{body}END:VCALENDAR\r\n");
        Calendar::parse(&ics, BERLIN).unwrap()
    }

    fn starts(events: &[Event]) -> Vec<String> {
        events.iter().map(|e| e.start.to_rfc3339()).collect()
    }

    #[test]
    fn property_parsing() {
        let p =
            Property::parse(r#"DTSTART;TZID="Europe/Berlin";X-A=a,"b:c":20261110T103000"#).unwrap();
        assert_eq!(p.name, "DTSTART");
        assert_eq!(p.param("TZID"), Some("Europe/Berlin"));
        assert_eq!(p.param("X-A"), Some("a,b:c"));
        assert_eq!(p.value, "20261110T103000");

        let p = Property::parse("description:a:b;c").unwrap();
        assert_eq!(p.name, "DESCRIPTION");
        assert_eq!(p.value, "a:b;c");

        assert_eq!(Property::parse("no colon here"), None);
        assert_eq!(Property::parse(":value"), None);
        assert_eq!(Property::parse(r#"X;P="unterminated:v"#), None);
    }

    #[test]
    fn unfolding() {
        let lines = unfold("SUMMARY:Long\r\n  title\r\n\tcontinued\r\nUID:1\n");
        assert_eq!(lines[0], "SUMMARY:Long title\tcontinued".replace('\t', ""));
        assert_eq!(lines[1], "UID:1");
    }

    #[test]
    fn text_unescaping() {
        assert_eq!(unescape_text(r"a\, b\; c\\d\nnext"), "a, b; c\\d\nnext");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("PT1H30M").unwrap(), Duration::minutes(90));
        assert_eq!(parse_duration("P1DT2H").unwrap(), Duration::hours(26));
        assert_eq!(parse_duration("P2W").unwrap(), Duration::days(14));
        assert_eq!(parse_duration("-PT5M").unwrap(), Duration::minutes(-5));
        assert_eq!(parse_duration("PT0S").unwrap(), Duration::zero());
        assert!(parse_duration("1H").is_err());
        assert!(parse_duration("PT5").is_err());
        assert!(parse_duration("P5H").is_err());
    }

    #[test]
    fn tzid_resolution() {
        assert_eq!(
            resolve_tzid("America/New_York", BERLIN).name(),
            "America/New_York"
        );
        assert_eq!(
            resolve_tzid("/mozilla.org/20050126_1/Europe/Paris", BERLIN).name(),
            "Europe/Paris"
        );
        assert_eq!(
            resolve_tzid("Pacific Standard Time", BERLIN).name(),
            "America/Los_Angeles"
        );
        assert_eq!(
            resolve_tzid("Mars/Olympus_Mons", BERLIN).name(),
            "Europe/Berlin"
        );
    }

    #[test]
    fn rejects_non_ics() {
        assert!(Calendar::parse("<html>Sign in</html>", BERLIN).is_err());
    }

    #[test]
    fn single_events_and_time_zones() {
        let cal = calendar(
            "BEGIN:VEVENT\r\nUID:a\r\nSUMMARY:Berlin\r\nDTSTART;TZID=Europe/Berlin:20261110T103000\r\n\
             DTEND;TZID=Europe/Berlin:20261110T110000\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:b\r\nSUMMARY:UTC\r\nDTSTART:20261110T120000Z\r\n\
             DURATION:PT45M\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:c\r\nSUMMARY:NY\r\nDTSTART;TZID=America/New_York:20261110T090000\r\n\
             DTEND;TZID=America/New_York:20261110T093000\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:d\r\nSUMMARY:Floating\r\nDTSTART:20261110T160000\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:e\r\nSUMMARY:Other day\r\nDTSTART:20261111T120000Z\r\nEND:VEVENT\r\n",
        );
        let events = cal.expand(
            utc("2026-11-10T00:00:00+01:00"),
            utc("2026-11-11T00:00:00+01:00"),
        );
        let summary: Vec<_> = events.iter().map(|e| e.summary.as_str()).collect();
        assert_eq!(summary, ["Berlin", "UTC", "NY", "Floating"]);
        assert_eq!(
            starts(&events),
            [
                "2026-11-10T09:30:00+00:00",
                "2026-11-10T12:00:00+00:00",
                "2026-11-10T14:00:00+00:00",
                "2026-11-10T15:00:00+00:00",
            ]
        );
        assert_eq!(events[0].end, utc("2026-11-10T10:00:00Z"));
        assert_eq!(events[1].end, utc("2026-11-10T12:45:00Z"));
        assert_eq!(events[3].end, events[3].start);
    }

    #[test]
    fn all_day_events() {
        let cal = calendar(
            "BEGIN:VEVENT\r\nUID:a\r\nSUMMARY:Holiday\r\nDTSTART;VALUE=DATE:20261110\r\n\
             DTEND;VALUE=DATE:20261111\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:b\r\nSUMMARY:No end\r\nDTSTART;VALUE=DATE:20261110\r\nEND:VEVENT\r\n",
        );
        let events = cal.expand(utc("2026-11-10T12:00:00Z"), utc("2026-11-10T13:00:00Z"));
        assert_eq!(events.len(), 2);
        for e in &events {
            assert!(e.all_day);
            assert_eq!(e.start, utc("2026-11-10T00:00:00+01:00"));
            assert_eq!(e.end, utc("2026-11-11T00:00:00+01:00"));
        }
    }

    #[test]
    fn cross_midnight_event_overlaps_both_days() {
        let cal = calendar(
            "BEGIN:VEVENT\r\nUID:a\r\nSUMMARY:Late\r\nDTSTART;TZID=Europe/Berlin:20261110T233000\r\n\
             DTEND;TZID=Europe/Berlin:20261111T003000\r\nEND:VEVENT\r\n",
        );
        let day1 = cal.expand(
            utc("2026-11-10T00:00:00+01:00"),
            utc("2026-11-11T00:00:00+01:00"),
        );
        let day2 = cal.expand(
            utc("2026-11-11T00:00:00+01:00"),
            utc("2026-11-12T00:00:00+01:00"),
        );
        assert_eq!(day1.len(), 1);
        assert_eq!(day2.len(), 1);
    }

    const WEEKLY: &str = "BEGIN:VEVENT\r\nUID:standup\r\nSUMMARY:Standup\r\n\
        DTSTART;TZID=Europe/Berlin:20260105T100000\r\nDTEND;TZID=Europe/Berlin:20260105T101500\r\n\
        RRULE:FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR\r\n\
        EXDATE;TZID=Europe/Berlin:20261111T100000\r\nEND:VEVENT\r\n";

    #[test]
    fn recurrence_with_exdate_across_dst() {
        let cal = calendar(WEEKLY);
        // Mon 2026-11-09 .. Fri 2026-11-13; Wednesday is excluded.
        let events = cal.expand(utc("2026-11-09T00:00:00Z"), utc("2026-11-14T00:00:00Z"));
        assert_eq!(
            starts(&events),
            [
                "2026-11-09T09:00:00+00:00",
                "2026-11-10T09:00:00+00:00",
                "2026-11-12T09:00:00+00:00",
                "2026-11-13T09:00:00+00:00",
            ]
        );
        assert!(
            events
                .iter()
                .all(|e| e.end - e.start == Duration::minutes(15))
        );

        // Summer time: same wall-clock time, different UTC offset.
        let events = cal.expand(utc("2026-07-01T00:00:00Z"), utc("2026-07-02T00:00:00Z"));
        assert_eq!(starts(&events), ["2026-07-01T08:00:00+00:00"]);
    }

    #[test]
    fn overrides_move_and_cancel_instances() {
        let cal = calendar(&format!(
            "{WEEKLY}\
             BEGIN:VEVENT\r\nUID:standup\r\nSUMMARY:Standup (moved)\r\n\
             RECURRENCE-ID;TZID=Europe/Berlin:20261110T100000\r\n\
             DTSTART;TZID=Europe/Berlin:20261110T140000\r\nDTEND;TZID=Europe/Berlin:20261110T141500\r\n\
             END:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:standup\r\nSUMMARY:Standup\r\nSTATUS:CANCELLED\r\n\
             RECURRENCE-ID;TZID=Europe/Berlin:20261112T100000\r\n\
             DTSTART;TZID=Europe/Berlin:20261112T100000\r\nDTEND;TZID=Europe/Berlin:20261112T101500\r\n\
             END:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:standup\r\nSUMMARY:Standup (to next week)\r\n\
             RECURRENCE-ID:20261113T090000Z\r\n\
             DTSTART;TZID=Europe/Berlin:20261116T150000\r\nDTEND;TZID=Europe/Berlin:20261116T151500\r\n\
             END:VEVENT\r\n"
        ));
        let events = cal.expand(utc("2026-11-09T00:00:00Z"), utc("2026-11-14T00:00:00Z"));
        let got: Vec<_> = events
            .iter()
            .map(|e| (e.start.to_rfc3339(), e.summary.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("2026-11-09T09:00:00+00:00".to_owned(), "Standup"),
                ("2026-11-10T13:00:00+00:00".to_owned(), "Standup (moved)"),
            ]
        );

        // The instance moved into the next week shows up there.
        let events = cal.expand(utc("2026-11-16T00:00:00Z"), utc("2026-11-17T00:00:00Z"));
        let summary: Vec<_> = events.iter().map(|e| e.summary.as_str()).collect();
        assert_eq!(summary, ["Standup", "Standup (to next week)"]);
    }

    #[test]
    fn orphan_override_is_an_event() {
        let cal = calendar(
            "BEGIN:VEVENT\r\nUID:foreign\r\nSUMMARY:Invited once\r\n\
             RECURRENCE-ID:20261110T150000Z\r\nDTSTART:20261110T150000Z\r\n\
             DTEND:20261110T160000Z\r\nEND:VEVENT\r\n",
        );
        let events = cal.expand(utc("2026-11-10T00:00:00Z"), utc("2026-11-11T00:00:00Z"));
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn until_count_and_rdate() {
        let cal = calendar(
            "BEGIN:VEVENT\r\nUID:a\r\nSUMMARY:Until\r\nDTSTART;TZID=Europe/Berlin:20261101T090000\r\n\
             RRULE:FREQ=DAILY;UNTIL=20261110T235959\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:b\r\nSUMMARY:Count\r\nDTSTART:20261108T100000Z\r\n\
             RRULE:FREQ=DAILY;COUNT=3\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:c\r\nSUMMARY:Rdate\r\nDTSTART:20261101T110000Z\r\n\
             RDATE:20261110T110000Z,20261111T110000Z\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:d\r\nSUMMARY:Date until\r\nDTSTART;VALUE=DATE:20261101\r\n\
             RRULE:FREQ=DAILY;UNTIL=20261110\r\nEND:VEVENT\r\n",
        );
        let day = |d: &str| {
            let from = utc(&format!("{d}T00:00:00+01:00"));
            cal.expand(from, from + Duration::days(1))
                .into_iter()
                .map(|e| e.summary)
                .collect::<Vec<_>>()
        };
        assert_eq!(day("2026-11-10"), ["Date until", "Until", "Count", "Rdate"]);
        assert_eq!(day("2026-11-11"), ["Rdate"]);
        assert_eq!(day("2026-11-01"), ["Date until", "Until", "Rdate"]);
    }

    #[test]
    fn instances_that_started_before_the_window_are_included() {
        let cal = calendar(
            "BEGIN:VEVENT\r\nUID:a\r\nSUMMARY:Long\r\nDTSTART:20261101T080000Z\r\n\
             DURATION:PT4H\r\nRRULE:FREQ=DAILY\r\nEND:VEVENT\r\n",
        );
        let events = cal.expand(utc("2026-11-10T10:00:00Z"), utc("2026-11-10T11:00:00Z"));
        assert_eq!(starts(&events), ["2026-11-10T08:00:00+00:00"]);
    }

    #[test]
    fn malformed_events_are_skipped() {
        let cal = calendar(
            "BEGIN:VEVENT\r\nUID:bad\r\nSUMMARY:No start\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:bad2\r\nDTSTART:not-a-date\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:bad3\r\nDTSTART:20261110T100000Z\r\nDTEND:20261110T090000Z\r\nEND:VEVENT\r\n\
             garbage line without colon\r\n\
             BEGIN:VEVENT\r\nUID:ok\r\nSUMMARY:Fine\r\nDTSTART:20261110T100000Z\r\n\
             BEGIN:VALARM\r\nTRIGGER:-PT10M\r\nDTSTART:19700101T000000Z\r\nEND:VALARM\r\n\
             END:VEVENT\r\n",
        );
        assert_eq!(cal.skipped, 3);
        let events = cal.expand(utc("2026-11-10T00:00:00Z"), utc("2026-11-11T00:00:00Z"));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary, "Fine");
        assert_eq!(events[0].start, utc("2026-11-10T10:00:00Z"));
    }

    #[test]
    fn status_transparency_attendees_and_links() {
        let cal = calendar(
            "BEGIN:VEVENT\r\nUID:a\r\nSUMMARY:Sync\\, weekly\r\nDTSTART:20261110T100000Z\r\n\
             STATUS:TENTATIVE\r\nTRANSP:TRANSPARENT\r\n\
             ATTENDEE;CN=Me;PARTSTAT=DECLINED:mailto:Me@Example.com\r\n\
             ATTENDEE;CN=\"Other, Person\";PARTSTAT=ACCEPTED:mailto:other@example.com\r\n\
             X-GOOGLE-CONFERENCE:https://meet.google.com/abc-defg-hij\r\n\
             DESCRIPTION:Join: https://zoom.us/j/1\r\nEND:VEVENT\r\n",
        );
        let events = cal.expand(utc("2026-11-10T00:00:00Z"), utc("2026-11-11T00:00:00Z"));
        let e = &events[0];
        assert_eq!(e.summary, "Sync, weekly");
        assert_eq!(e.status, Status::Tentative);
        assert!(e.transparent);
        assert!(e.declined_by("me@example.com"));
        assert!(!e.declined_by("other@example.com"));
        assert_eq!(
            e.meeting_link.as_deref(),
            Some("https://meet.google.com/abc-defg-hij")
        );
    }

    #[test]
    fn google_fixture() {
        let ics = include_str!("../../tests/fixtures/google.ics");
        let cal = Calendar::parse(ics, BERLIN).unwrap();
        assert_eq!(cal.skipped, 0);
        let events = cal.expand(
            utc("2026-11-10T00:00:00+01:00"),
            utc("2026-11-11T00:00:00+01:00"),
        );
        let got: Vec<_> = events
            .iter()
            .map(|e| {
                (
                    e.start.with_timezone(&BERLIN).format("%H:%M").to_string(),
                    e.summary.as_str(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("00:00".to_owned(), "Public holiday"),
                ("09:00".to_owned(), "Focus time"),
                ("10:00".to_owned(), "Daily standup"),
                ("13:00".to_owned(), "Lunch with team (moved)"),
                ("15:00".to_owned(), "Customer call"),
                ("16:00".to_owned(), "Declined review"),
                ("17:00".to_owned(), "Cancelled sync"),
                ("23:30".to_owned(), "Late deploy"),
            ]
        );
    }
}
