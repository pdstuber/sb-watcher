use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_TARGET_URL: &str = "https://www.sbtix.de/catalog/tickets/\
98430-tickets-summer-breeze-2027-summer-breeze-open-air-dinkelsbuehl-am-18-08-2027";

pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36";

#[derive(Debug, Clone)]
pub struct Config {
    pub telegram_token: String,
    pub chat_id: Option<i64>,
    pub ntfy_topic: Option<String>,
    pub poll_interval: Duration,
    pub target_url: String,
    pub user_agent: String,
    pub fixture_path: Option<PathBuf>,
}

/// Treat whitespace-only values as absent. Deploy tooling frequently sets empty
/// strings rather than unsetting a variable.
fn opt(map: &HashMap<String, String>, key: &str) -> Option<String> {
    map.get(key).map(|s| s.trim()).filter(|s| !s.is_empty()).map(|s| s.to_string())
}

impl Config {
    pub fn from_map(map: &HashMap<String, String>) -> Result<Self> {
        let telegram_token = opt(map, "TELOXIDE_TOKEN")
            .ok_or_else(|| anyhow!("TELOXIDE_TOKEN is required — create a bot with @BotFather"))?;

        let chat_id = match opt(map, "TELEGRAM_CHAT_ID") {
            Some(v) => Some(
                v.parse::<i64>()
                    .with_context(|| format!("TELEGRAM_CHAT_ID must be an integer, got {v:?}"))?,
            ),
            None => None,
        };

        let poll_interval = match opt(map, "POLL_INTERVAL_SECS") {
            Some(v) => {
                let secs: u64 = v
                    .parse()
                    .with_context(|| format!("POLL_INTERVAL_SECS must be an integer, got {v:?}"))?;
                if secs == 0 {
                    return Err(anyhow!("POLL_INTERVAL_SECS must be greater than zero"));
                }
                Duration::from_secs(secs)
            }
            None => Duration::from_secs(60),
        };

        Ok(Config {
            telegram_token,
            chat_id,
            ntfy_topic: opt(map, "NTFY_TOPIC"),
            poll_interval,
            target_url: opt(map, "TARGET_URL").unwrap_or_else(|| DEFAULT_TARGET_URL.to_string()),
            user_agent: opt(map, "USER_AGENT").unwrap_or_else(|| DEFAULT_USER_AGENT.to_string()),
            fixture_path: opt(map, "SB_WATCHER_FIXTURE_PATH").map(PathBuf::from),
        })
    }

    pub fn from_env() -> Result<Self> {
        Self::from_map(&std::env::vars().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("TELOXIDE_TOKEN".into(), "123:ABC".into());
        m
    }

    #[test]
    fn requires_token() {
        let cfg = Config::from_map(&HashMap::new());
        assert!(cfg.is_err(), "missing token must be a hard startup error");
    }

    #[test]
    fn applies_defaults() {
        let cfg = Config::from_map(&base()).unwrap();
        assert_eq!(cfg.poll_interval, Duration::from_secs(60));
        assert_eq!(cfg.target_url, DEFAULT_TARGET_URL);
        assert_eq!(cfg.user_agent, DEFAULT_USER_AGENT);
        assert_eq!(cfg.chat_id, None);
        assert_eq!(cfg.ntfy_topic, None);
        assert_eq!(cfg.fixture_path, None);
    }

    #[test]
    fn parses_chat_id_and_topic() {
        let mut m = base();
        m.insert("TELEGRAM_CHAT_ID".into(), "-100123".into());
        m.insert("NTFY_TOPIC".into(), "secret-topic".into());
        let cfg = Config::from_map(&m).unwrap();
        assert_eq!(cfg.chat_id, Some(-100123));
        assert_eq!(cfg.ntfy_topic.as_deref(), Some("secret-topic"));
    }

    #[test]
    fn rejects_non_numeric_chat_id() {
        let mut m = base();
        m.insert("TELEGRAM_CHAT_ID".into(), "not-a-number".into());
        assert!(Config::from_map(&m).is_err());
    }

    #[test]
    fn rejects_zero_poll_interval() {
        let mut m = base();
        m.insert("POLL_INTERVAL_SECS".into(), "0".into());
        assert!(Config::from_map(&m).is_err(), "a zero interval would hammer the site");
    }

    #[test]
    fn blank_optional_values_are_treated_as_unset() {
        let mut m = base();
        m.insert("NTFY_TOPIC".into(), "   ".into());
        assert_eq!(Config::from_map(&m).unwrap().ntfy_topic, None);
    }
}
