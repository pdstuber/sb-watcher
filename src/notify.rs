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
        let text = format!("{}\n\n{}", alert.title, alert.body);
        self.bot.send_message(self.chat, text).await?;
        Ok(())
    }
}

// ---------- ntfy ----------

pub struct NtfyNotifier {
    client: reqwest::Client,
    topic: String,
}

impl NtfyNotifier {
    pub fn new(client: reqwest::Client, topic: String) -> Self {
        NtfyNotifier { client, topic }
    }
}

#[async_trait]
impl Notifier for NtfyNotifier {
    async fn send(&self, alert: &Alert) -> Result<()> {
        // ntfy headers must be ASCII, so the emoji-bearing title goes in the body.
        let resp = self
            .client
            .post(format!("https://ntfy.sh/{}", self.topic))
            .header("Title", "sb-watcher")
            .header("Priority", ntfy_priority(alert.severity))
            .header("Tags", "ticket")
            .body(format!("{}\n\n{}", alert.title, alert.body))
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
        let mut errors = Vec::new();
        for c in &self.channels {
            // Deliberately no early return: every channel gets its attempt. If
            // Telegram is down at 3am, ntfy is the entire point of the second channel.
            if let Err(e) = c.send(alert).await {
                log::error!("notifier failed: {e:#}");
                errors.push(e.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(
                "{} of {} channels failed: {}",
                errors.len(),
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
        assert!(r.is_err(), "the failure is still reported to the caller");
        assert_eq!(good.sent().len(), 1, "the healthy channel must still have fired");
    }

    #[tokio::test]
    async fn multi_with_no_channels_is_not_an_error() {
        assert!(MultiNotifier::new(vec![]).send(&alert()).await.is_ok());
    }

    #[test]
    fn ntfy_priority_reflects_severity() {
        assert_eq!(ntfy_priority(Severity::Max), "max");
        assert_eq!(ntfy_priority(Severity::Info), "default");
    }
}
