//! Find the video meeting link of an event.

/// Hosts that count as video meeting links, matched as the host itself or any
/// subdomain of it (`example.zoom.us`).
const MEETING_HOSTS: &[&str] = &[
    "meet.google.com",
    "zoom.us",
    "teams.microsoft.com",
    "teams.live.com",
];

/// The `X-GOOGLE-CONFERENCE` URL if present, else the first Meet/Zoom/Teams
/// URL found in `LOCATION`, then `DESCRIPTION`.
pub fn meeting_link(
    conference: Option<&str>,
    location: Option<&str>,
    description: Option<&str>,
) -> Option<String> {
    if let Some(url) = conference
        .map(str::trim)
        .filter(|u| u.starts_with("https://"))
    {
        return Some(url.to_owned());
    }
    [location, description]
        .into_iter()
        .flatten()
        .find_map(first_meeting_url)
}

fn first_meeting_url(text: &str) -> Option<String> {
    let mut rest = text;
    while let Some(pos) = rest.find("https://") {
        let candidate = &rest[pos..];
        let end = candidate
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '\\'))
            .unwrap_or(candidate.len());
        let url = candidate[..end].trim_end_matches(['.', ',', ';', ')', ']']);
        if is_meeting_url(url) {
            return Some(url.to_owned());
        }
        rest = &candidate["https://".len()..];
    }
    None
}

fn is_meeting_url(url: &str) -> bool {
    let Some(after_scheme) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = host
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    MEETING_HOSTS
        .iter()
        .any(|m| host == *m || host.ends_with(&format!(".{m}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conference_wins() {
        assert_eq!(
            meeting_link(
                Some("https://meet.google.com/abc"),
                Some("https://zoom.us/j/1"),
                None
            )
            .as_deref(),
            Some("https://meet.google.com/abc")
        );
    }

    #[test]
    fn location_before_description() {
        assert_eq!(
            meeting_link(
                None,
                Some("Room 1, https://acme.zoom.us/j/1?pwd=x."),
                Some("https://meet.google.com/x")
            )
            .as_deref(),
            Some("https://acme.zoom.us/j/1?pwd=x")
        );
    }

    #[test]
    fn finds_link_in_html_description() {
        let desc = r#"<p>Agenda: https://docs.example.com/x</p><a href="https://teams.microsoft.com/l/meetup-join/19%3a">Join</a>"#;
        assert_eq!(
            meeting_link(None, None, Some(desc)).as_deref(),
            Some("https://teams.microsoft.com/l/meetup-join/19%3a")
        );
    }

    #[test]
    fn ignores_lookalikes_and_other_links() {
        assert_eq!(
            meeting_link(None, Some("https://notzoom.us/j/1"), None),
            None
        );
        assert_eq!(
            meeting_link(None, Some("https://zoom.us.evil.com/j/1"), None),
            None
        );
        assert_eq!(
            meeting_link(None, Some("https://evil.com/?u=https://x"), None),
            None
        );
        assert_eq!(
            meeting_link(None, Some("http://meet.google.com/abc"), None),
            None
        );
        assert_eq!(meeting_link(Some(""), None, Some("no links")), None);
    }
}
