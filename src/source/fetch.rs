//! Fetching the ICS feed, the on-disk cache and retry backoff.
//!
//! The feed URL is a bearer secret: it must never appear in logs or error
//! messages. `reqwest` errors are stripped of it with
//! [`reqwest::Error::without_url`].

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail};

use super::Location;

/// Refuse feeds larger than this (Google feeds with years of history are a
/// few MiB at most).
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

pub struct Fetcher {
    location: Location,
    client: reqwest::Client,
    cache_path: Option<PathBuf>,
}

impl Fetcher {
    pub fn new(location: Location, cache_path: Option<PathBuf>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .timeout(TIMEOUT)
            .build()
            .map_err(reqwest::Error::without_url)?;
        Ok(Self {
            location,
            client,
            cache_path,
        })
    }

    /// Download (or read) the feed body.
    pub async fn fetch(&self) -> Result<String> {
        match &self.location {
            Location::File(path) => tokio::fs::read_to_string(path)
                .await
                .with_context(|| format!("failed to read {}", path.display())),
            Location::Url(url) => self.fetch_url(url).await,
        }
    }

    async fn fetch_url(&self, url: &str) -> Result<String> {
        let mut response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(reqwest::Error::without_url)
            .context("request failed")?;
        let status = response.status();
        if !status.is_success() {
            bail!("server answered with HTTP {status}");
        }
        if response
            .content_length()
            .is_some_and(|len| len > MAX_BODY_BYTES as u64)
        {
            bail!("feed is larger than {MAX_BODY_BYTES} bytes");
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(reqwest::Error::without_url)
            .context("failed to read the response")?
        {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_BODY_BYTES {
                bail!("feed is larger than {MAX_BODY_BYTES} bytes");
            }
        }
        String::from_utf8(body).context("feed is not valid UTF-8")
    }

    /// The cached feed body and when it was written.
    pub async fn load_cache(&self) -> Option<(String, SystemTime)> {
        load_cache(self.cache_path.as_ref()?).await
    }

    /// Store the feed body, readable only by the user. Failures are logged and
    /// otherwise ignored: the cache is an optimisation.
    pub async fn store_cache(&self, body: &str) {
        let Some(path) = &self.cache_path else {
            return;
        };
        if let Err(err) = write_private(path, body.as_bytes()).await {
            tracing::warn!("failed to write the cache: {err:#}");
        }
    }
}

/// The feed body cached at `path` and when it was written.
pub async fn load_cache(path: &Path) -> Option<(String, SystemTime)> {
    let modified = tokio::fs::metadata(path).await.ok()?.modified().ok()?;
    let body = tokio::fs::read_to_string(path).await.ok()?;
    Some((body, modified))
}

/// Atomically write `contents` to `path` with mode `0600` (directory `0700`).
async fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    if let Some(dir) = path.parent() {
        tokio::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .await
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .await
        .with_context(|| format!("failed to open {}", tmp.display()))?;
    file.write_all(contents).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

/// Exponential backoff for failed fetches: `min`, `2·min`, … capped at `max`.
#[derive(Debug, Clone)]
pub struct Backoff {
    min: Duration,
    max: Duration,
    current: Option<Duration>,
}

impl Backoff {
    pub fn new(min: Duration, max: Duration) -> Self {
        Self {
            min,
            max: max.max(min),
            current: None,
        }
    }

    /// Record a failure and return how long to wait before retrying.
    pub fn failure(&mut self) -> Duration {
        let next = match self.current {
            None => self.min,
            Some(current) => (current * 2).min(self.max),
        };
        self.current = Some(next);
        next
    }

    pub fn success(&mut self) {
        self.current = None;
    }
}

impl Default for Backoff {
    /// 30 s → 5 min.
    fn default() -> Self {
        Self::new(Duration::from_secs(30), Duration::from_secs(300))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn backoff_doubles_and_resets() {
        let mut b = Backoff::default();
        let secs: Vec<u64> = (0..6).map(|_| b.failure().as_secs()).collect();
        assert_eq!(secs, [30, 60, 120, 240, 300, 300]);
        b.success();
        assert_eq!(b.failure().as_secs(), 30);
    }

    #[tokio::test]
    async fn file_source_and_private_cache() {
        let dir = tempfile::tempdir().unwrap();
        let feed = dir.path().join("feed.ics");
        std::fs::write(&feed, "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n").unwrap();
        let cache = dir.path().join("cache/calendar.ics");

        let fetcher = Fetcher::new(Location::File(feed), Some(cache.clone())).unwrap();
        assert!(fetcher.load_cache().await.is_none());

        let body = fetcher.fetch().await.unwrap();
        fetcher.store_cache(&body).await;
        let (cached, _) = fetcher.load_cache().await.unwrap();
        assert_eq!(cached, body);

        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&cache), 0o600);
        assert_eq!(mode(cache.parent().unwrap()), 0o700);
    }

    #[test]
    fn location_debug_hides_url() {
        let loc = Location::Url("https://calendar.google.com/secret".into());
        assert!(!format!("{loc:?}").contains("secret"));
    }
}
