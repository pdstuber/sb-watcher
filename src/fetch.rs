use crate::config::Config;
use anyhow::Result;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// 403 / 429 — we are probably being rate-limited or blocked, which means blind.
    Blocked(u16),
    Http(u16),
    Network(String),
    Io(String),
}

impl FetchError {
    pub fn is_blocked(&self) -> bool {
        matches!(self, FetchError::Blocked(_))
    }
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Blocked(c) => write!(f, "blocked by the site (HTTP {c})"),
            FetchError::Http(c) => write!(f, "HTTP {c}"),
            FetchError::Network(e) => write!(f, "network error: {e}"),
            FetchError::Io(e) => write!(f, "fixture read error: {e}"),
        }
    }
}

impl std::error::Error for FetchError {}

pub struct Fetcher {
    client: reqwest::Client,
    url: String,
    fixture: Option<PathBuf>,
}

impl Fetcher {
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(&cfg.user_agent)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Fetcher { client, url: cfg.target_url.clone(), fixture: cfg.fixture_path.clone() })
    }

    pub async fn fetch(&self) -> Result<String, FetchError> {
        // Rehearsal mode: read local HTML instead of hitting the site, so the
        // "tickets available" path can be exercised on demand.
        if let Some(path) = &self.fixture {
            return std::fs::read_to_string(path).map_err(|e| FetchError::Io(e.to_string()));
        }

        let resp = self
            .client
            .get(&self.url)
            .send()
            .await
            .map_err(|e| FetchError::Network(e.to_string()))?;

        let status = resp.status().as_u16();
        if status == 403 || status == 429 {
            return Err(FetchError::Blocked(status));
        }
        if !resp.status().is_success() {
            return Err(FetchError::Http(status));
        }

        resp.text().await.map_err(|e| FetchError::Network(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::collections::HashMap;

    fn cfg_with(k: &str, v: &str) -> Config {
        let mut m = HashMap::new();
        m.insert("TELOXIDE_TOKEN".to_string(), "123:ABC".to_string());
        m.insert(k.to_string(), v.to_string());
        Config::from_map(&m).unwrap()
    }

    #[tokio::test]
    async fn fixture_path_bypasses_the_network() {
        let cfg = cfg_with("SB_WATCHER_FIXTURE_PATH", "tests/fixtures/empty_resale.html");
        let html = Fetcher::from_config(&cfg).unwrap().fetch().await.unwrap();
        assert!(html.contains("Ticketbörse"));
    }

    #[tokio::test]
    async fn a_missing_fixture_is_an_io_error() {
        let cfg = cfg_with("SB_WATCHER_FIXTURE_PATH", "tests/fixtures/does-not-exist.html");
        assert!(matches!(
            Fetcher::from_config(&cfg).unwrap().fetch().await,
            Err(FetchError::Io(_))
        ));
    }

    #[test]
    fn blocking_statuses_are_recognised() {
        assert!(FetchError::Blocked(429).is_blocked());
        assert!(FetchError::Blocked(403).is_blocked());
        assert!(!FetchError::Http(500).is_blocked());
        assert!(!FetchError::Network("timeout".into()).is_blocked());
    }
}
