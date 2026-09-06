use crate::state::{Alert, Severity};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use teloxide::prelude::*;
use teloxide::types::ChatId;

#[async_trait]
pub trait Notifier: Send + Sync {
    async fn send(&self, alert: &Alert) -> Result<()>;
}

pub fn ntfy_priority(sev: Severity) -> &'static str {
    match sev {
        Severity::Max => "max",
        Severity::Info => "default",
    }
}

/// Telegram rejects messages over 4096 chars and ntfy bodies over 4096 bytes.
/// Bytes ≥ chars, so one byte-based cap with headroom satisfies both.
pub const MAX_MESSAGE_BYTES: usize = 4000;

/// Cut `s` to at most `max_bytes` on a char boundary, marking the cut when the
/// marker itself fits within the limit.
pub fn truncate_message(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let marker = if max_bytes >= "\n[…]".len() { "\n[…]" } else { "" };
    let mut end = max_bytes.saturating_sub(marker.len()).min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{marker}", &s[..end])
}

// ---------- Telegram ----------

pub struct TelegramNotifier {
    bot: Bot,
    chat: ChatId,
}

impl TelegramNotifier {
    pub fn new(bot: Bot, chat: ChatId) -> Self {
        TelegramNotifier { bot, chat }
    }
}

#[async_trait]
impl Notifier for TelegramNotifier {
    async fn send(&self, alert: &Alert) -> Result<()> {
        let text = truncate_message(&format!("{}\n\n{}", alert.title, alert.body), MAX_MESSAGE_BYTES);
        self.bot.send_message(self.chat, text).await?;
        Ok(())
    }
}

// ---------- ntfy ----------

pub const NTFY_DEFAULT_BASE_URL: &str = "https://ntfy.sh";

/// HTTP client for ntfy with hard timeouts. A notifier must never be able to
/// hang the send loop: `Client::new()` has no timeout at all.
pub fn ntfy_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .build()
}

pub struct NtfyNotifier {
    client: reqwest::Client,
    base_url: String,
    topic: String,
}

impl NtfyNotifier {
    /// `base_url` is `NTFY_DEFAULT_BASE_URL` in production and a mock server in tests.
    pub fn new(client: reqwest::Client, base_url: impl Into<String>, topic: String) -> Self {
        NtfyNotifier {
            client,
            base_url: base_url.into(),
            topic,
        }
    }
}

#[async_trait]
impl Notifier for NtfyNotifier {
    async fn send(&self, alert: &Alert) -> Result<()> {
        // ntfy headers must be ASCII, so the emoji-bearing title goes in the body.
        let url = format!("{}/{}", self.base_url.trim_end_matches('/'), self.topic);
        let resp = self
            .client
            .post(url)
            .header("Title", "sb-watcher")
            .header("Priority", ntfy_priority(alert.severity))
            .header("Tags", "ticket")
            .body(truncate_message(
                &format!("{}\n\n{}", alert.title, alert.body),
                MAX_MESSAGE_BYTES,
            ))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(anyhow!("ntfy returned {}", resp.status()));
        }
        Ok(())
    }
}

// ---------- fan-out ----------

pub struct MultiNotifier {
    channels: Vec<Box<dyn Notifier>>,
}

impl MultiNotifier {
    pub fn new(channels: Vec<Box<dyn Notifier>>) -> Self {
        MultiNotifier { channels }
    }
}

#[async_trait]
impl Notifier for MultiNotifier {
    async fn send(&self, alert: &Alert) -> Result<()> {
        // Every alert funnels through here, so this is the one place that gives
        // an operator a record of what was actually sent and when.
        log::info!("ALERT [{:?}] {}", alert.severity, alert.title);
        if self.channels.is_empty() {
            return Err(anyhow!("no notifier channels configured"));
        }
        let mut errors = Vec::new();
        for c in &self.channels {
            // Deliberately no early return: every channel gets its attempt. If
            // Telegram is down at 3am, ntfy is the entire point of the second channel.
            if let Err(e) = c.send(alert).await {
                log::error!("notifier failed: {e:#}");
                errors.push(e.to_string());
            }
        }
        // Delivered means the user can see it somewhere. A partial failure is
        // logged above but is not a failure of the alert.
        if errors.len() < self.channels.len() {
            Ok(())
        } else {
            Err(anyhow!(
                "all {} channels failed: {}",
                self.channels.len(),
                errors.join("; ")
            ))
        }
    }
}

// ---------- test double ----------

#[derive(Default)]
pub struct FakeNotifier {
    sent: Mutex<Vec<Alert>>,
    fail_next: Mutex<bool>,
}

impl FakeNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sent(&self) -> Vec<Alert> {
        self.sent.lock().unwrap().clone()
    }

    pub fn fail_next(&self) {
        *self.fail_next.lock().unwrap() = true;
    }
}

#[async_trait]
impl Notifier for FakeNotifier {
    async fn send(&self, alert: &Alert) -> Result<()> {
        {
            let mut fail = self.fail_next.lock().unwrap();
            if *fail {
                *fail = false;
                return Err(anyhow!("simulated failure"));
            }
        }
        self.sent.lock().unwrap().push(alert.clone());
        Ok(())
    }
}

#[async_trait]
impl<T: Notifier + ?Sized> Notifier for Arc<T> {
    async fn send(&self, alert: &Alert) -> Result<()> {
        (**self).send(alert).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Alert, AlertKind, Severity};

    fn alert() -> Alert {
        Alert::new(AlertKind::ResaleAvailable, Severity::Max, "title", "body")
    }

    #[tokio::test]
    async fn fake_records_what_it_was_sent() {
        let f = FakeNotifier::new();
        f.send(&alert()).await.unwrap();
        assert_eq!(f.sent().len(), 1);
        assert_eq!(f.sent()[0].title, "title");
    }

    #[tokio::test]
    async fn multi_sends_to_every_channel() {
        let a = Arc::new(FakeNotifier::new());
        let b = Arc::new(FakeNotifier::new());
        let multi = MultiNotifier::new(vec![Box::new(a.clone()), Box::new(b.clone())]);
        multi.send(&alert()).await.unwrap();
        assert_eq!(a.sent().len(), 1);
        assert_eq!(b.sent().len(), 1);
    }

    #[tokio::test]
    async fn multi_still_delivers_when_one_channel_fails() {
        // The whole reason a second channel exists.
        let bad = Arc::new(FakeNotifier::new());
        bad.fail_next();
        let good = Arc::new(FakeNotifier::new());
        let multi = MultiNotifier::new(vec![Box::new(bad.clone()), Box::new(good.clone())]);
        let r = multi.send(&alert()).await;
        assert!(r.is_ok(), "one channel reaching the user counts as delivered");
        assert_eq!(good.sent().len(), 1, "the healthy channel must still have fired");
    }

    #[tokio::test]
    async fn multi_with_no_channels_fails_loudly() {
        let result = MultiNotifier::new(vec![]).send(&alert()).await;
        assert!(result.is_err(), "zero channels cannot count as delivered");
    }

    #[tokio::test]
    async fn multi_fails_only_when_every_channel_fails() {
        let a = Arc::new(FakeNotifier::new());
        let b = Arc::new(FakeNotifier::new());
        a.fail_next();
        b.fail_next();
        let multi = MultiNotifier::new(vec![Box::new(a.clone()), Box::new(b.clone())]);
        assert!(
            multi.send(&alert()).await.is_err(),
            "nobody got it, so it was not delivered"
        );
    }

    #[test]
    fn ntfy_priority_reflects_severity() {
        assert_eq!(ntfy_priority(Severity::Max), "max");
        assert_eq!(ntfy_priority(Severity::Info), "default");
    }

    fn ntfy_against(server: &wiremock::MockServer) -> NtfyNotifier {
        NtfyNotifier::new(ntfy_client().unwrap(), server.uri(), "my-topic".to_string())
    }

    #[tokio::test]
    async fn ntfy_posts_to_the_topic_with_max_priority_for_max_alerts() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/my-topic"))
            .and(header("Priority", "max"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        ntfy_against(&server).send(&alert()).await.unwrap();
        // `expect(1)` is verified when `server` drops at the end of the test.
    }

    #[tokio::test]
    async fn ntfy_reports_a_server_error_as_a_failure() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let r = ntfy_against(&server).send(&alert()).await;
        assert!(r.is_err(), "a 5xx must surface so MultiNotifier can report it");
    }

    #[test]
    fn ntfy_client_has_timeouts() {
        // reqwest does not expose its timeouts; building succeeding is the
        // contract we can test. The values live in ntfy_client().
        ntfy_client().expect("client must build");
    }

    #[test]
    fn short_messages_are_untouched() {
        assert_eq!(truncate_message("hello", 4000), "hello");
    }

    #[test]
    fn long_messages_are_cut_at_a_char_boundary_and_marked() {
        // Multi-byte chars: naive slicing would panic mid-codepoint.
        let long = "é".repeat(3000); // 6000 bytes
        let out = truncate_message(&long, MAX_MESSAGE_BYTES);
        assert!(out.len() <= MAX_MESSAGE_BYTES, "{} bytes", out.len());
        assert!(out.ends_with("[…]"), "must show that it was cut");
        assert!(out.starts_with("ééé"));
    }

    #[test]
    fn limits_smaller_than_the_marker_are_still_respected() {
        for max_bytes in 0.."\n[…]".len() {
            let out = truncate_message("ééé", max_bytes);
            assert!(
                out.len() <= max_bytes,
                "limit {max_bytes} produced {} bytes: {out:?}",
                out.len()
            );
        }
    }
}
