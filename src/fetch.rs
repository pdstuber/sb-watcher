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
        Ok(Fetcher {
            client,
            url: cfg.target_url.clone(),
            fixture: cfg.fixture_path.clone(),
        })
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

    async fn fetcher_for(server: &wiremock::MockServer) -> Fetcher {
        let mut m = HashMap::new();
        m.insert("TELOXIDE_TOKEN".to_string(), "123:ABC".to_string());
        m.insert("TARGET_URL".to_string(), format!("{}/ticket", server.uri()));
        Fetcher::from_config(&Config::from_map(&m).unwrap()).unwrap()
    }

    async fn respond_with(status: u16, body: &str) -> (wiremock::MockServer, Fetcher) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ticket"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&server)
            .await;
        let f = fetcher_for(&server).await;
        (server, f)
    }

    #[tokio::test]
    async fn a_200_returns_the_body() {
        let (_s, f) = respond_with(200, "<html>hello</html>").await;
        assert_eq!(f.fetch().await.unwrap(), "<html>hello</html>");
    }

    #[tokio::test]
    async fn a_429_maps_to_blocked_not_a_generic_http_error() {
        // Rate limiting must be distinguishable: it triggers a longer backoff
        // and an immediate warning, because being throttled means being blind.
        let (_s, f) = respond_with(429, "slow down").await;
        let e = f.fetch().await.unwrap_err();
        assert_eq!(e, FetchError::Blocked(429));
        assert!(e.is_blocked());
    }

    #[tokio::test]
    async fn a_403_maps_to_blocked() {
        let (_s, f) = respond_with(403, "forbidden").await;
        assert_eq!(f.fetch().await.unwrap_err(), FetchError::Blocked(403));
    }

    #[tokio::test]
    async fn a_500_is_an_ordinary_http_error_not_a_block() {
        // A server error is transient and must NOT trigger the blocked backoff.
        let (_s, f) = respond_with(500, "boom").await;
        let e = f.fetch().await.unwrap_err();
        assert_eq!(e, FetchError::Http(500));
        assert!(!e.is_blocked());
    }

    #[tokio::test]
    async fn a_404_is_an_http_error() {
        let (_s, f) = respond_with(404, "gone").await;
        assert_eq!(f.fetch().await.unwrap_err(), FetchError::Http(404));
    }

    #[tokio::test]
    async fn an_unreachable_host_is_a_network_error() {
        let mut m = HashMap::new();
        m.insert("TELOXIDE_TOKEN".to_string(), "123:ABC".to_string());
        // Reserved TEST-NET-1 address, guaranteed not to route anywhere.
        m.insert("TARGET_URL".to_string(), "http://192.0.2.1:9/ticket".to_string());
        let f = Fetcher::from_config(&Config::from_map(&m).unwrap()).unwrap();
        assert!(matches!(f.fetch().await, Err(FetchError::Network(_))));
    }

    #[test]
    fn blocking_statuses_are_recognised() {
        assert!(FetchError::Blocked(429).is_blocked());
        assert!(FetchError::Blocked(403).is_blocked());
        assert!(!FetchError::Http(500).is_blocked());
        assert!(!FetchError::Network("timeout".into()).is_blocked());
    }
}
