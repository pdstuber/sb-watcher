use crate::config::Config;
use crate::fetch::{FetchError, Fetcher};
use crate::notify::Notifier;
use crate::parse::{classify, PageObservation, ParseError, ResaleState};
use crate::state::{listing_summary, transitions, Alert, AlertKind, Severity};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

pub const REPEAT_EVERY_MINS: i64 = 5;
pub const REPEAT_CAP: u32 = 6;
pub const FAILURE_WARN_AFTER_MINS: i64 = 15;
pub const HEARTBEAT_EVERY_HOURS: i64 = 24;
pub const BACKOFF_MAX: Duration = Duration::from_secs(600);
pub const BLOCKED_BACKOFF: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct AlertRepeat {
    pub kind: AlertKind,
    pub sent_at: DateTime<Utc>,
    pub count: u32,
}

#[derive(Debug)]
pub struct AppState {
    pub last: Option<PageObservation>,
    pub checks: u64,
    pub failures_since: Option<DateTime<Utc>>,
    pub failure_warned: bool,
    pub repeat: Option<AlertRepeat>,
    pub last_change: Option<DateTime<Utc>>,
    pub started: DateTime<Utc>,
    pub backoff: Option<Duration>,
    pub last_structure_warn: Option<DateTime<Utc>>,
    pub last_heartbeat: DateTime<Utc>,
}

impl AppState {
    pub fn new(now: DateTime<Utc>) -> Self {
        AppState {
            last: None,
            checks: 0,
            failures_since: None,
            failure_warned: false,
            repeat: None,
            last_change: None,
            started: now,
            backoff: None,
            last_structure_warn: None,
            last_heartbeat: now,
        }
    }
}

/// Double the wait on each consecutive failure, starting at the poll interval,
/// capped at BACKOFF_MAX.
pub fn next_backoff(current: Option<Duration>, base: Duration) -> Duration {
    match current {
        None => base.min(BACKOFF_MAX),
        Some(c) => (c * 2).min(BACKOFF_MAX),
    }
}

pub fn should_repeat(repeat: &AlertRepeat, now: DateTime<Utc>) -> bool {
    repeat.count < REPEAT_CAP && now - repeat.sent_at >= ChronoDuration::minutes(REPEAT_EVERY_MINS)
}

pub fn failure_warning_due(failures_since: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    match failures_since {
        Some(since) => now - since >= ChronoDuration::minutes(FAILURE_WARN_AFTER_MINS),
        None => false,
    }
}

/// Fold one successful observation into the state, emitting any alerts.
///
/// Separated from the loop so it can be driven with injected timestamps
/// instead of real sleeps.
pub async fn apply_observation<N: Notifier + ?Sized>(
    st: &mut AppState,
    obs: PageObservation,
    now: DateTime<Utc>,
    url: &str,
    notifier: &N,
) {
    st.checks += 1;

    let alerts = transitions(st.last.as_ref(), &obs, url);
    let changed = st.last.as_ref() != Some(&obs);

    for alert in &alerts {
        if let Err(e) = notifier.send(alert).await {
            log::error!("failed to deliver alert: {e:#}");
        }
        if alert.severity == Severity::Max {
            // `count` is the number of REMINDERS sent so far, so the initial
            // alert starts it at zero and REPEAT_CAP reminders follow.
            st.repeat = Some(AlertRepeat {
                kind: alert.kind,
                sent_at: now,
                count: 0,
            });
        }
    }

    let still_alerting = obs.resale == ResaleState::Available || !obs.main_sold_out;

    // Clear unconditionally when the condition resolves. Doing this only in the
    // `alerts.is_empty()` branch would leave reminders armed on the very poll
    // where stock vanished, since that poll also emits a ResaleGone alert.
    if !still_alerting {
        st.repeat = None;
    } else if alerts.is_empty() {
        if let Some(r) = st.repeat.clone() {
            if should_repeat(&r, now) {
                let alert = Alert::new(
                    r.kind,
                    Severity::Max,
                    "🎟️ STILL AVAILABLE",
                    format!(
                        "Reminder {} of {}.\n\n{}\n\n{}",
                        r.count + 1,
                        REPEAT_CAP,
                        listing_summary(&obs.listings, &obs.resale_text),
                        url
                    ),
                );
                if let Err(e) = notifier.send(&alert).await {
                    log::error!("failed to deliver reminder: {e:#}");
                }
                st.repeat = Some(AlertRepeat {
                    kind: r.kind,
                    sent_at: now,
                    count: r.count + 1,
                });
            }
        }
    }

    if changed {
        st.last_change = Some(now);
    }
    st.last = Some(obs);
}

/// Fold a failed fetch into the state, warning once per failure episode.
pub async fn apply_failure<N: Notifier + ?Sized>(
    st: &mut AppState,
    err: &FetchError,
    now: DateTime<Utc>,
    url: &str,
    notifier: &N,
) {
    log::warn!("fetch failed: {err}");
    if st.failures_since.is_none() {
        st.failures_since = Some(now);
    }
    if !st.failure_warned && (failure_warning_due(st.failures_since, now) || err.is_blocked()) {
        st.failure_warned = true;
        let alert = Alert::new(
            AlertKind::FetchFailing,
            Severity::Info,
            "⚠️ sb-watcher cannot reach the site",
            format!("{err}\n\nThe watcher is currently blind. It keeps retrying.\n\n{url}"),
        );
        let _ = notifier.send(&alert).await;
    }
}

pub async fn run_watcher<N: Notifier + ?Sized>(
    cfg: Config,
    fetcher: Fetcher,
    notifier: Arc<N>,
    shared: Arc<Mutex<AppState>>,
) -> ! {
    loop {
        let now = Utc::now();
        let result = fetcher.fetch().await;
        let mut st = shared.lock().await;

        let wait = match result {
            Ok(html) => match classify(&html) {
                Ok(obs) => {
                    st.failures_since = None;
                    st.failure_warned = false;
                    st.backoff = None;
                    apply_observation(&mut st, obs, now, &cfg.target_url, notifier.as_ref()).await;
                    cfg.poll_interval
                }
                Err(ParseError::CardNotFound) => {
                    // Never silently treat this as "all quiet".
                    let due = st
                        .last_structure_warn
                        .map(|w| now - w >= ChronoDuration::hours(24))
                        .unwrap_or(true);
                    if due {
                        st.last_structure_warn = Some(now);
                        let alert = Alert::new(
                            AlertKind::StructureChanged,
                            Severity::Info,
                            "⚠️ Page structure changed",
                            format!(
                                "The Ticketbörse card could not be found. sb-watcher may be \
                                 blind and needs a code update.\n\n{}",
                                cfg.target_url
                            ),
                        );
                        let _ = notifier.send(&alert).await;
                    }
                    cfg.poll_interval
                }
            },
            Err(e) => {
                apply_failure(&mut st, &e, now, &cfg.target_url, notifier.as_ref()).await;
                let b = if e.is_blocked() && st.backoff.is_none() {
                    BLOCKED_BACKOFF
                } else {
                    next_backoff(st.backoff, cfg.poll_interval)
                };
                st.backoff = Some(b);
                b
            }
        };

        // Dead-man's switch: silence must never be ambiguous between
        // "no tickets" and "the process died".
        if now - st.last_heartbeat >= ChronoDuration::hours(HEARTBEAT_EVERY_HOURS) {
            st.last_heartbeat = now;
            let alert = Alert::new(
                AlertKind::Heartbeat,
                Severity::Info,
                "✅ sb-watcher still running",
                format!(
                    "{} checks since {}. Last change: {}.",
                    st.checks,
                    st.started.format("%Y-%m-%d %H:%M UTC"),
                    st.last_change
                        .map(|c| c.format("%Y-%m-%d %H:%M UTC").to_string())
                        .unwrap_or_else(|| "none".into())
                ),
            );
            let _ = notifier.send(&alert).await;
        }

        drop(st);
        tokio::time::sleep(wait).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::FakeNotifier;
    use chrono::TimeZone;

    fn t(min: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + min * 60, 0).unwrap()
    }

    #[test]
    fn backoff_doubles_from_the_poll_interval_and_caps() {
        let base = Duration::from_secs(60);
        assert_eq!(next_backoff(None, base), Duration::from_secs(60));
        assert_eq!(
            next_backoff(Some(Duration::from_secs(60)), base),
            Duration::from_secs(120)
        );
        assert_eq!(
            next_backoff(Some(Duration::from_secs(120)), base),
            Duration::from_secs(240)
        );
        assert_eq!(
            next_backoff(Some(Duration::from_secs(240)), base),
            Duration::from_secs(480)
        );
        assert_eq!(next_backoff(Some(Duration::from_secs(480)), base), BACKOFF_MAX);
        assert_eq!(next_backoff(Some(BACKOFF_MAX), base), BACKOFF_MAX);
    }

    #[test]
    fn repeat_waits_the_interval() {
        let r = AlertRepeat {
            kind: AlertKind::ResaleAvailable,
            sent_at: t(0),
            count: 1,
        };
        assert!(!should_repeat(&r, t(4)), "too soon");
        assert!(should_repeat(&r, t(5)), "five minutes have passed");
    }

    #[test]
    fn repeat_stops_at_the_cap() {
        let r = AlertRepeat {
            kind: AlertKind::ResaleAvailable,
            sent_at: t(0),
            count: REPEAT_CAP,
        };
        assert!(!should_repeat(&r, t(60)), "cap reached, stop pinging");
    }

    #[test]
    fn failure_warning_is_time_based_not_attempt_based() {
        // Under backoff, ten attempts take over an hour; a count-based rule
        // would leave the watcher silently blind through a whole drop.
        assert!(!failure_warning_due(Some(t(0)), t(14)));
        assert!(failure_warning_due(Some(t(0)), t(15)));
        assert!(!failure_warning_due(None, t(99)));
    }

    fn empty_obs() -> PageObservation {
        PageObservation {
            resale: ResaleState::Empty,
            resale_text: "Ticketbörse Es gibt aktuell keine Tickets zum Weiterverkauf.".into(),
            listings: vec![],
            main_sold_out: true,
        }
    }

    fn available_obs() -> PageObservation {
        PageObservation {
            resale: ResaleState::Available,
            resale_text: "Ticketbörse In den Warenkorb".into(),
            listings: vec![],
            main_sold_out: true,
        }
    }

    #[tokio::test]
    async fn a_drop_alerts_once_then_repeats_on_schedule_then_stops() {
        let n = FakeNotifier::new();
        let mut st = AppState::new(t(0));
        st.last = Some(empty_obs());

        // The drop happens.
        apply_observation(&mut st, available_obs(), t(0), "url", &n).await;
        assert_eq!(n.sent().len(), 1);
        assert_eq!(st.repeat.as_ref().unwrap().count, 0, "no reminders sent yet");

        // Still available a minute later: too soon to repeat.
        apply_observation(&mut st, available_obs(), t(1), "url", &n).await;
        assert_eq!(n.sent().len(), 1, "must not spam every poll");

        // Five minutes on: one reminder.
        apply_observation(&mut st, available_obs(), t(5), "url", &n).await;
        assert_eq!(n.sent().len(), 2);

        // Drive past the cap.
        for m in [10, 15, 20, 25, 30, 35, 40] {
            apply_observation(&mut st, available_obs(), t(m), "url", &n).await;
        }
        assert_eq!(n.sent().len(), 1 + REPEAT_CAP as usize, "stops after the cap");
    }

    #[tokio::test]
    async fn stock_going_away_clears_the_repeat() {
        let n = FakeNotifier::new();
        let mut st = AppState::new(t(0));
        st.last = Some(empty_obs());
        apply_observation(&mut st, available_obs(), t(0), "url", &n).await;
        assert!(st.repeat.is_some());
        apply_observation(&mut st, empty_obs(), t(1), "url", &n).await;
        assert!(st.repeat.is_none(), "no point reminding about stock that is gone");
    }
}
