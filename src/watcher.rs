use crate::config::Config;
use crate::fetch::{FetchError, Fetcher};
use crate::notify::Notifier;
use crate::parse::{classify, MainStock, PageObservation, ParseError, ResaleState};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertRepeat {
    pub kind: AlertKind,
    pub sent_at: DateTime<Utc>,
    /// Reminders sent so far. The initial alert and delivery retries are not counted.
    pub count: u32,
    /// Whether the most recent send for this kind reached at least one channel.
    /// Set by `record_delivery` after the loop has actually sent it.
    pub delivered: bool,
}

#[derive(Debug)]
pub struct AppState {
    pub last: Option<PageObservation>,
    pub checks: u64,
    pub failures_since: Option<DateTime<Utc>>,
    pub failure_warned: bool,
    pub repeats: Vec<AlertRepeat>,
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
            repeats: Vec::new(),
            last_change: None,
            started: now,
            backoff: None,
            last_structure_warn: None,
            last_heartbeat: now,
        }
    }
}

/// Double the wait on each consecutive failure, starting at the poll interval,
/// capped at BACKOFF_MAX. A 403/429 means we are being throttled, so it never
/// waits less than BLOCKED_BACKOFF regardless of where the doubling had got to.
pub fn next_backoff(current: Option<Duration>, base: Duration, blocked: bool) -> Duration {
    let doubled = match current {
        None => base,
        Some(c) => c * 2,
    };
    let floored = if blocked { doubled.max(BLOCKED_BACKOFF) } else { doubled };
    floored.min(BACKOFF_MAX)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConditionState {
    Open,
    Closed,
    Unknown,
}

/// What the current observation says about the condition behind a Max alert.
///
/// Exhaustive on purpose, with no wildcard arm: a future `Severity::Max` kind
/// that fell through to a default `Closed` here would be armed and then
/// immediately dropped by the `retain` in `apply_observation` on the very same
/// poll — one alert, no reminders, no delivery retry, with no compile error to
/// catch it. Enumerating every variant turns "add a Max kind" into a build
/// break until this function is taught about it.
fn condition_state(kind: AlertKind, obs: &PageObservation) -> ConditionState {
    match kind {
        AlertKind::ResaleAvailable => match obs.resale {
            ResaleState::Available => ConditionState::Open,
            ResaleState::Empty => ConditionState::Closed,
        },
        AlertKind::MainOnSale => match obs.main {
            MainStock::OnSale => ConditionState::Open,
            MainStock::SoldOut => ConditionState::Closed,
            MainStock::Unknown => ConditionState::Unknown,
        },
        AlertKind::ResaleGone
        | AlertKind::MainSoldOut
        | AlertKind::ResaleTextChanged
        | AlertKind::StructureChanged
        | AlertKind::FetchFailing
        | AlertKind::Heartbeat
        | AlertKind::Started => ConditionState::Closed,
    }
}

/// What a reminder for `kind` should say about the current page.
///
/// Exhaustive on purpose, with no wildcard arm: a future `Severity::Max` kind
/// that fell through to a default here would silently reuse the resale
/// listing wording (bug 2 all over again — "STILL AVAILABLE" resale text on a
/// reminder about something else). Enumerating every variant turns "add a Max
/// kind" into a build break until this function is taught about it. Every
/// variant besides `MainOnSale` currently shares the resale-listing body: the
/// two that can actually reach here today (`ResaleAvailable`, and `MainOnSale`
/// handled above) get the same wording as before this change; the rest are
/// unreachable in practice since they are never armed as repeats.
fn reminder_body(kind: AlertKind, obs: &PageObservation, url: &str) -> String {
    match kind {
        AlertKind::MainOnSale => format!("The main shop is still not showing 'Ausverkauft'.\n\n{url}"),
        AlertKind::ResaleAvailable
        | AlertKind::ResaleGone
        | AlertKind::MainSoldOut
        | AlertKind::ResaleTextChanged
        | AlertKind::StructureChanged
        | AlertKind::FetchFailing
        | AlertKind::Heartbeat
        | AlertKind::Started => format!("{}\n\n{}", listing_summary(&obs.listings, &obs.resale_text), url),
    }
}

fn delivery_retry(kind: AlertKind, obs: &PageObservation, url: &str) -> Alert {
    let previous_observation = match kind {
        AlertKind::MainOnSale => {
            format!("A previous check found the main shop without 'Ausverkauft'.\n\n{url}")
        }
        AlertKind::ResaleAvailable
        | AlertKind::ResaleGone
        | AlertKind::MainSoldOut
        | AlertKind::ResaleTextChanged
        | AlertKind::StructureChanged
        | AlertKind::FetchFailing
        | AlertKind::Heartbeat
        | AlertKind::Started => reminder_body(kind, obs, url),
    };
    Alert::new(
        kind,
        Severity::Max,
        "🎟️ STILL AVAILABLE",
        format!("The previous alert could not be delivered.\n\n{}", previous_observation),
    )
}

/// Retry Max alerts that nobody received even when this poll cannot produce a
/// new observation. The last confirmed observation is enough to preserve the
/// delivery guarantee; unconfirmed stock only pauses scheduled reminders.
fn retry_undelivered(st: &mut AppState, now: DateTime<Utc>, url: &str) -> Vec<Alert> {
    let Some(obs) = st.last.as_ref() else {
        return Vec::new();
    };
    let mut alerts = Vec::new();
    for repeat in st.repeats.iter_mut().filter(|repeat| !repeat.delivered) {
        alerts.push(delivery_retry(repeat.kind, obs, url));
        repeat.sent_at = now;
    }
    alerts
}

/// Fold one successful observation into the state.
///
/// Pure: returns the alerts that must be sent, in order, and never touches the
/// network. The caller sends them after releasing the state lock, then reports
/// each Max alert's outcome through `record_delivery`.
pub fn apply_observation(st: &mut AppState, obs: PageObservation, now: DateTime<Utc>, url: &str) -> Vec<Alert> {
    let mut alerts = transitions(st.last.as_ref(), &obs, url);
    let changed = st.last.as_ref() != Some(&obs);

    // Arm (or re-arm) a repeat for every Max alert that fired this poll.
    let fired: Vec<AlertKind> = alerts
        .iter()
        .filter(|a| a.severity == Severity::Max)
        .map(|a| a.kind)
        .collect();
    for kind in &fired {
        st.repeats.retain(|r| r.kind != *kind);
        st.repeats.push(AlertRepeat {
            kind: *kind,
            sent_at: now,
            count: 0,
            delivered: false,
        });
    }

    // A confirmed closure ends a repeat. Unknown preserves it: an unrecognised
    // frame cannot prove that an alert condition resolved.
    st.repeats
        .retain(|r| condition_state(r.kind, &obs) != ConditionState::Closed);

    for r in st.repeats.iter_mut() {
        if fired.contains(&r.kind) {
            continue;
        }
        if !r.delivered {
            // Nothing reached the user last time: resend now, and do not let it
            // eat into the reminder cap.
            alerts.push(delivery_retry(r.kind, &obs, url));
            r.sent_at = now;
        } else if condition_state(r.kind, &obs) == ConditionState::Open && should_repeat(r, now) {
            alerts.push(Alert::new(
                r.kind,
                Severity::Max,
                "🎟️ STILL AVAILABLE",
                format!(
                    "Reminder {} of {}.\n\n{}",
                    r.count + 1,
                    REPEAT_CAP,
                    reminder_body(r.kind, &obs, url)
                ),
            ));
            r.sent_at = now;
            r.count += 1;
            r.delivered = false;
        }
    }

    if changed {
        st.last_change = Some(now);
    }
    st.last = Some(obs);
    alerts
}

/// Record whether the most recent Max alert of `kind` reached at least one
/// channel. Called by the loop after sending, under a fresh lock.
pub fn record_delivery(st: &mut AppState, kind: AlertKind, delivered: bool) {
    if let Some(r) = st.repeats.iter_mut().find(|r| r.kind == kind) {
        r.delivered = delivered;
    }
}

/// Fold a failed fetch into the state. Returns the outage warning to send, at
/// most once per failure episode.
pub fn apply_failure(st: &mut AppState, err: &FetchError, now: DateTime<Utc>, url: &str) -> Option<Alert> {
    log::warn!("fetch failed: {err}");
    if st.failures_since.is_none() {
        st.failures_since = Some(now);
    }
    if st.failure_warned || !(failure_warning_due(st.failures_since, now) || err.is_blocked()) {
        return None;
    }
    st.failure_warned = true;
    Some(Alert::new(
        AlertKind::FetchFailing,
        Severity::Info,
        "⚠️ sb-watcher cannot reach the site",
        format!("{err}\n\nThe watcher is currently blind. It keeps retrying.\n\n{url}"),
    ))
}

/// Handle a page that fetched but is structurally unrecognisable: no
/// Ticketbörse card, or no main product frame. `what` names the missing part.
///
/// Rate-limited to one warning per 24h so a permanent redesign does not spam,
/// but it must never be silent: treating this as "all quiet" would leave the
/// watcher blind while looking healthy. Returns the warning to send, if due.
pub fn apply_structure_error(st: &mut AppState, now: DateTime<Utc>, url: &str, what: &str) -> Option<Alert> {
    let due = st
        .last_structure_warn
        .map(|w| now - w >= ChronoDuration::hours(24))
        .unwrap_or(true);
    if !due {
        return None;
    }
    st.last_structure_warn = Some(now);
    Some(Alert::new(
        AlertKind::StructureChanged,
        Severity::Info,
        "⚠️ Page structure changed",
        format!("{what} could not be found. sb-watcher may be blind and needs a code update.\n\n{url}"),
    ))
}

/// The dead-man's switch. Without it, silence is ambiguous between "no tickets"
/// and "the process died three weeks ago". Returns the heartbeat to send, if due.
pub fn maybe_heartbeat(st: &mut AppState, now: DateTime<Utc>) -> Option<Alert> {
    if now - st.last_heartbeat < ChronoDuration::hours(HEARTBEAT_EVERY_HOURS) {
        return None;
    }
    st.last_heartbeat = now;
    Some(Alert::new(
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
    ))
}

/// Sent once at boot. Makes restarts visible (a crash loop shorter than the
/// heartbeat interval would otherwise be silent) and proves at start-up that
/// alerts can actually be delivered.
pub fn startup_alert(cfg: &Config) -> Alert {
    let mut body = format!("Watching {} every {}s.", cfg.target_url, cfg.poll_interval.as_secs());
    if let Some(p) = &cfg.fixture_path {
        body.push_str(&format!(
            "\n\n⚠️ FIXTURE MODE: reading {} instead of the live site.",
            p.display()
        ));
    }
    Alert::new(AlertKind::Started, Severity::Info, "▶️ sb-watcher started", body)
}

/// One poll's worth of state changes: fold the fetch result into `st` and
/// return the alerts to send plus how long to wait before the next poll.
///
/// Pure (no clock, no I/O), so the loop's wiring — backoff, episode resets,
/// the heartbeat — is unit-testable with injected timestamps.
pub fn poll_once(
    st: &mut AppState,
    result: Result<String, FetchError>,
    now: DateTime<Utc>,
    cfg: &Config,
) -> (Vec<Alert>, Duration) {
    let mut alerts = Vec::new();
    st.checks += 1;

    let wait = match result {
        Ok(html) => {
            // The site answered: the fetch-failure episode is over regardless of
            // whether the page parses. Leaving these set on CardNotFound would
            // suppress the warning for the next real outage.
            st.failures_since = None;
            st.failure_warned = false;
            st.backoff = None;
            match classify(&html) {
                Ok(obs) => {
                    let main_unknown = obs.main == MainStock::Unknown;
                    alerts.extend(apply_observation(st, obs, now, &cfg.target_url));
                    if main_unknown {
                        alerts.extend(apply_structure_error(
                            st,
                            now,
                            &cfg.target_url,
                            "The main product frame",
                        ));
                    }
                    cfg.poll_interval
                }
                Err(ParseError::CardNotFound) => {
                    alerts.extend(retry_undelivered(st, now, &cfg.target_url));
                    alerts.extend(apply_structure_error(st, now, &cfg.target_url, "The Ticketbörse card"));
                    cfg.poll_interval
                }
            }
        }
        Err(e) => {
            alerts.extend(retry_undelivered(st, now, &cfg.target_url));
            alerts.extend(apply_failure(st, &e, now, &cfg.target_url));
            let b = next_backoff(st.backoff, cfg.poll_interval, e.is_blocked());
            st.backoff = Some(b);
            b
        }
    };

    alerts.extend(maybe_heartbeat(st, now));
    (alerts, wait)
}

pub async fn run_watcher<N: Notifier + ?Sized>(
    cfg: Config,
    fetcher: Fetcher,
    notifier: Arc<N>,
    shared: Arc<Mutex<AppState>>,
) -> ! {
    if let Err(e) = notifier.send(&startup_alert(&cfg)).await {
        log::error!("failed to deliver the startup message: {e:#}");
    }

    loop {
        let started = std::time::Instant::now();
        let result = fetcher.fetch().await;
        // Sampled AFTER the fetch so a slow response does not skew sent_at,
        // last_change and the heartbeat clock by the fetch duration.
        let now = Utc::now();

        // The lock is held only for the pure state update, never across a send:
        // notifier latency cannot block /ack or /status. Transport timeouts
        // bound how long sending can delay the next poll.
        let (alerts, wait) = {
            let mut st = shared.lock().await;
            poll_once(&mut st, result, now, &cfg)
        };

        for alert in &alerts {
            let delivered = match notifier.send(alert).await {
                Ok(()) => true,
                Err(e) => {
                    log::error!("failed to deliver {:?}: {e:#}", alert.kind);
                    false
                }
            };
            if alert.severity == Severity::Max {
                record_delivery(&mut *shared.lock().await, alert.kind, delivered);
            }
        }

        // Sleep for the remainder of the interval, so a slow fetch or send does
        // not stretch the effective poll period.
        tokio::time::sleep(wait.saturating_sub(started.elapsed())).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::collections::HashMap;

    fn t(min: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + min * 60, 0).unwrap()
    }

    fn cfg() -> Config {
        let mut m = HashMap::new();
        m.insert("TELOXIDE_TOKEN".to_string(), "123:ABC".to_string());
        m.insert("TELEGRAM_CHAT_ID".to_string(), "1".to_string());
        m.insert("TARGET_URL".to_string(), "url".to_string());
        Config::from_map(&m).unwrap()
    }

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("tests/fixtures/{name}"))
            .unwrap_or_else(|e| panic!("cannot read fixture {name}: {e}"))
    }

    fn net_err() -> Result<String, FetchError> {
        Err(FetchError::Network("timeout".into()))
    }

    /// Simulate the loop delivering every Max alert successfully.
    fn deliver_all(st: &mut AppState, alerts: &[Alert]) {
        for a in alerts {
            if a.severity == Severity::Max {
                record_delivery(st, a.kind, true);
            }
        }
    }

    fn main_on_sale_obs() -> PageObservation {
        PageObservation {
            resale: ResaleState::Empty,
            resale_text: "Ticketbörse Es gibt aktuell keine Tickets zum Weiterverkauf.".into(),
            listings: vec![],
            main: MainStock::OnSale,
        }
    }

    #[test]
    fn backoff_doubles_from_the_poll_interval_and_caps() {
        let base = Duration::from_secs(60);
        assert_eq!(next_backoff(None, base, false), Duration::from_secs(60));
        assert_eq!(
            next_backoff(Some(Duration::from_secs(60)), base, false),
            Duration::from_secs(120)
        );
        assert_eq!(
            next_backoff(Some(Duration::from_secs(120)), base, false),
            Duration::from_secs(240)
        );
        assert_eq!(
            next_backoff(Some(Duration::from_secs(240)), base, false),
            Duration::from_secs(480)
        );
        assert_eq!(next_backoff(Some(Duration::from_secs(480)), base, false), BACKOFF_MAX);
        assert_eq!(next_backoff(Some(BACKOFF_MAX), base, false), BACKOFF_MAX);
    }

    #[test]
    fn a_block_after_a_transient_failure_still_waits_the_blocked_floor() {
        let base = Duration::from_secs(60);
        assert_eq!(next_backoff(None, base, true), BLOCKED_BACKOFF);
        assert_eq!(next_backoff(Some(Duration::from_secs(60)), base, true), BLOCKED_BACKOFF);
        assert_eq!(next_backoff(Some(BLOCKED_BACKOFF), base, true), BACKOFF_MAX);
        // Not blocked: the floor does not apply.
        assert_eq!(
            next_backoff(Some(Duration::from_secs(60)), base, false),
            Duration::from_secs(120)
        );
    }

    #[test]
    fn repeat_waits_the_interval() {
        let r = AlertRepeat {
            kind: AlertKind::ResaleAvailable,
            sent_at: t(0),
            count: 1,
            delivered: true,
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
            delivered: true,
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
            main: MainStock::SoldOut,
        }
    }

    fn available_obs() -> PageObservation {
        PageObservation {
            resale: ResaleState::Available,
            resale_text: "Ticketbörse In den Warenkorb".into(),
            listings: vec![],
            main: MainStock::SoldOut,
        }
    }

    #[test]
    fn a_drop_alerts_once_then_repeats_on_schedule_then_stops() {
        let mut st = AppState::new(t(0));
        st.last = Some(empty_obs());
        let mut sent = Vec::new();

        // The drop happens.
        let a = apply_observation(&mut st, available_obs(), t(0), "url");
        deliver_all(&mut st, &a);
        sent.extend(a);
        assert_eq!(sent.len(), 1);
        assert_eq!(st.repeats[0].count, 0, "no reminders sent yet");

        // Still available a minute later: too soon to repeat.
        let a = apply_observation(&mut st, available_obs(), t(1), "url");
        deliver_all(&mut st, &a);
        sent.extend(a);
        assert_eq!(sent.len(), 1, "must not spam every poll");

        // Five minutes on: one reminder.
        let a = apply_observation(&mut st, available_obs(), t(5), "url");
        deliver_all(&mut st, &a);
        sent.extend(a);
        assert_eq!(sent.len(), 2);

        // Drive past the cap.
        for m in [10, 15, 20, 25, 30, 35, 40] {
            let a = apply_observation(&mut st, available_obs(), t(m), "url");
            deliver_all(&mut st, &a);
            sent.extend(a);
        }
        assert_eq!(sent.len(), 1 + REPEAT_CAP as usize, "stops after the cap");
    }

    // ---- the safety nets: these are what guarantee it never fails silently ----

    #[test]
    fn missing_card_warns_immediately_but_only_once_per_day() {
        // A site redesign must never read as "all quiet", and must not spam either.
        let mut st = AppState::new(t(0));

        let first = apply_structure_error(&mut st, t(0), "url", "The Ticketbörse card").expect("first one must warn");
        assert_eq!(first.kind, AlertKind::StructureChanged);

        // Every poll for the next day is suppressed.
        assert!(apply_structure_error(&mut st, t(60), "url", "The Ticketbörse card").is_none());
        assert!(apply_structure_error(&mut st, t(60 * 23), "url", "The Ticketbörse card").is_none());

        // But it re-warns after 24h, so a lasting breakage keeps nagging.
        assert!(apply_structure_error(&mut st, t(60 * 24), "url", "The Ticketbörse card").is_some());
    }

    #[test]
    fn heartbeat_is_silent_until_due_then_fires_daily() {
        let mut st = AppState::new(t(0));
        st.checks = 1440;

        assert!(maybe_heartbeat(&mut st, t(60 * 23)).is_none(), "not due yet");

        let hb = maybe_heartbeat(&mut st, t(60 * 24)).expect("due at 24h");
        assert_eq!(hb.kind, AlertKind::Heartbeat);
        assert!(hb.body.contains("1440"), "must report the check count");

        // The clock resets, so it is quiet again for another day.
        assert!(maybe_heartbeat(&mut st, t(60 * 25)).is_none());
        assert!(maybe_heartbeat(&mut st, t(60 * 48)).is_some());
    }

    #[test]
    fn transient_failures_are_not_reported_but_a_sustained_outage_is() {
        let mut st = AppState::new(t(0));
        let err = FetchError::Network("timeout".into());

        assert!(apply_failure(&mut st, &err, t(0), "url").is_none());
        assert!(
            apply_failure(&mut st, &err, t(5), "url").is_none(),
            "a brief blip is not worth a message"
        );

        // 15 minutes of continuous failure means the watcher is genuinely blind.
        let warn = apply_failure(&mut st, &err, t(15), "url").expect("must warn after 15 minutes");
        assert_eq!(warn.kind, AlertKind::FetchFailing);

        // Only once per episode, not once per poll.
        assert!(apply_failure(&mut st, &err, t(20), "url").is_none());
        assert!(
            apply_failure(&mut st, &err, t(120), "url").is_none(),
            "must not repeat the outage warning every poll"
        );
    }

    #[test]
    fn being_blocked_warns_at_once_without_waiting() {
        // 403/429 means we are already blind, so the 15-minute grace does not apply.
        let mut st = AppState::new(t(0));
        let warn = apply_failure(&mut st, &FetchError::Blocked(429), t(0), "url").expect("warn at once");
        assert_eq!(warn.kind, AlertKind::FetchFailing);
    }

    #[test]
    fn stock_going_away_clears_the_repeat() {
        let mut st = AppState::new(t(0));
        st.last = Some(empty_obs());
        let a = apply_observation(&mut st, available_obs(), t(0), "url");
        deliver_all(&mut st, &a);
        assert_eq!(st.repeats.len(), 1);
        apply_observation(&mut st, empty_obs(), t(1), "url");
        assert!(st.repeats.is_empty(), "no point reminding about stock that is gone");
    }

    #[test]
    fn an_undelivered_alert_is_retried_next_poll_without_counting_toward_the_cap() {
        let mut st = AppState::new(t(0));
        st.last = Some(empty_obs());

        // The drop fires, but the loop reports that no channel accepted it.
        let a = apply_observation(&mut st, available_obs(), t(0), "url");
        assert_eq!(a.len(), 1);
        record_delivery(&mut st, AlertKind::ResaleAvailable, false);

        // Next poll, one minute later: resend at once, not in five minutes.
        let retry = apply_observation(&mut st, available_obs(), t(1), "url");
        assert_eq!(retry.len(), 1, "an undelivered Max alert must be retried immediately");
        assert_eq!(retry[0].severity, Severity::Max);
        assert!(retry[0].body.contains("could not be delivered"), "{}", retry[0].body);
        assert_eq!(st.repeats[0].count, 0, "retries do not eat into the reminder cap");
        deliver_all(&mut st, &retry);

        // Delivered now, so the regular schedule resumes from the retry.
        assert!(apply_observation(&mut st, available_obs(), t(2), "url").is_empty());
        let reminder = apply_observation(&mut st, available_obs(), t(6), "url");
        assert_eq!(reminder.len(), 1);
        assert!(reminder[0].body.contains("Reminder 1 of"), "{}", reminder[0].body);
    }

    #[test]
    fn an_undelivered_alert_is_retried_when_the_next_fetch_fails() {
        let mut st = AppState::new(t(0));
        st.last = Some(empty_obs());
        let initial = apply_observation(&mut st, available_obs(), t(0), "url");
        record_delivery(&mut st, AlertKind::ResaleAvailable, false);
        assert_eq!(initial.len(), 1, "the initial drop must arm one retry");

        let (alerts, _) = poll_once(&mut st, net_err(), t(1), &cfg());
        assert_eq!(alerts.len(), 1, "the failed fetch must not suppress delivery retry");
        assert_eq!(alerts[0].kind, AlertKind::ResaleAvailable);
        assert!(alerts[0].body.contains("could not be delivered"), "{}", alerts[0].body);
    }

    #[test]
    fn an_undelivered_alert_is_retried_when_the_next_page_has_no_card() {
        let mut st = AppState::new(t(0));
        st.last = Some(empty_obs());
        let initial = apply_observation(&mut st, available_obs(), t(0), "url");
        record_delivery(&mut st, AlertKind::ResaleAvailable, false);
        assert_eq!(initial.len(), 1, "the initial drop must arm one retry");

        let (alerts, _) = poll_once(&mut st, Ok(fixture("no_card.html")), t(1), &cfg());
        let kinds: Vec<AlertKind> = alerts.iter().map(|alert| alert.kind).collect();
        assert_eq!(
            kinds,
            vec![AlertKind::ResaleAvailable, AlertKind::StructureChanged],
            "the retry must precede the structure warning: {alerts:?}"
        );
    }

    #[test]
    fn an_undelivered_main_alert_is_retried_while_the_frame_is_unknown() {
        let mut st = AppState::new(t(0));
        st.last = Some(empty_obs());
        let initial = apply_observation(&mut st, main_on_sale_obs(), t(0), "url");
        record_delivery(&mut st, AlertKind::MainOnSale, false);
        assert_eq!(initial.len(), 1, "the initial drop must arm one retry");

        let mut unknown = main_on_sale_obs();
        unknown.main = MainStock::Unknown;
        let retry = apply_observation(&mut st, unknown, t(1), "url");
        assert_eq!(retry.len(), 1, "Unknown must not discard an undelivered alert");
        assert_eq!(retry[0].kind, AlertKind::MainOnSale);
        assert!(retry[0].body.contains("could not be delivered"), "{}", retry[0].body);
        assert!(retry[0].body.contains("previous check"), "{}", retry[0].body);
        assert!(
            !retry[0].body.contains("is still not showing"),
            "Unknown must not be described as confirmed stock: {}",
            retry[0].body
        );
    }

    #[test]
    fn a_delivered_main_reminder_pauses_while_the_frame_is_unknown() {
        let mut st = AppState::new(t(0));
        st.last = Some(empty_obs());
        let initial = apply_observation(&mut st, main_on_sale_obs(), t(0), "url");
        deliver_all(&mut st, &initial);

        let mut unknown = main_on_sale_obs();
        unknown.main = MainStock::Unknown;
        let alerts = apply_observation(&mut st, unknown, t(5), "url");
        assert!(
            alerts.is_empty(),
            "unconfirmed stock must not generate a reminder: {alerts:?}"
        );
        assert_eq!(
            st.repeats.len(),
            1,
            "Unknown must preserve the repeat until stock is confirmed"
        );
    }

    #[test]
    fn main_on_sale_reminders_describe_the_main_shop_not_the_resale() {
        let mut st = AppState::new(t(0));
        st.last = Some(empty_obs());
        let a = apply_observation(&mut st, main_on_sale_obs(), t(0), "url");
        assert_eq!(a[0].kind, AlertKind::MainOnSale);
        deliver_all(&mut st, &a);

        let reminder = apply_observation(&mut st, main_on_sale_obs(), t(5), "url");
        assert_eq!(reminder.len(), 1);
        assert_eq!(reminder[0].kind, AlertKind::MainOnSale);
        assert!(reminder[0].body.contains("main shop"), "{}", reminder[0].body);
        assert!(
            !reminder[0].body.contains("no offers could be parsed"),
            "must not claim resale stock that does not exist: {}",
            reminder[0].body
        );
    }

    #[test]
    fn resale_going_empty_stops_resale_reminders_even_while_main_is_on_sale() {
        let mut st = AppState::new(t(0));
        st.last = Some(empty_obs());
        let both = PageObservation {
            resale: ResaleState::Available,
            resale_text: "Ticketbörse In den Warenkorb".into(),
            listings: vec![],
            main: MainStock::OnSale,
        };
        let a = apply_observation(&mut st, both, t(0), "url");
        assert_eq!(a.len(), 2, "resale and main both fire");
        deliver_all(&mut st, &a);
        assert_eq!(st.repeats.len(), 2);

        // Resale sells out; main stays on sale.
        let a = apply_observation(&mut st, main_on_sale_obs(), t(1), "url");
        deliver_all(&mut st, &a);
        assert_eq!(st.repeats.len(), 1);
        assert_eq!(st.repeats[0].kind, AlertKind::MainOnSale);

        let reminders = apply_observation(&mut st, main_on_sale_obs(), t(6), "url");
        assert_eq!(reminders.len(), 1, "only the main-shop reminder remains");
        assert_eq!(reminders[0].kind, AlertKind::MainOnSale);
    }

    #[test]
    fn an_info_alert_on_the_same_poll_does_not_delay_a_due_reminder() {
        let mut st = AppState::new(t(0));
        st.last = Some(empty_obs());
        let a = apply_observation(&mut st, available_obs(), t(0), "url");
        deliver_all(&mut st, &a);

        // At t+5 the main shop also sells out again (Info) while resale stays up.
        let mut sold_out_again = available_obs();
        sold_out_again.main = MainStock::SoldOut;
        st.last.as_mut().unwrap().main = MainStock::OnSale;
        let a = apply_observation(&mut st, sold_out_again, t(5), "url");
        let kinds: Vec<AlertKind> = a.iter().map(|x| x.kind).collect();
        assert!(kinds.contains(&AlertKind::MainSoldOut), "{kinds:?}");
        assert!(
            kinds.contains(&AlertKind::ResaleAvailable),
            "reminder must still go out: {kinds:?}"
        );
    }

    // ---- the loop wiring, now testable ----

    #[test]
    fn a_successful_poll_ends_the_failure_episode_and_waits_the_poll_interval() {
        let mut st = AppState::new(t(0));
        st.failures_since = Some(t(0));
        st.failure_warned = true;
        st.backoff = Some(BACKOFF_MAX);

        let (alerts, wait) = poll_once(&mut st, Ok(fixture("empty_resale.html")), t(30), &cfg());

        assert!(alerts.is_empty(), "a quiet first observation sends nothing: {alerts:?}");
        assert_eq!(wait, cfg().poll_interval);
        assert_eq!(st.failures_since, None);
        assert!(!st.failure_warned);
        assert_eq!(st.backoff, None);
    }

    #[test]
    fn a_failed_poll_backs_off_exponentially() {
        let mut st = AppState::new(t(0));
        let (_, w1) = poll_once(&mut st, net_err(), t(0), &cfg());
        let (_, w2) = poll_once(&mut st, net_err(), t(1), &cfg());
        assert_eq!(w1, Duration::from_secs(60));
        assert_eq!(w2, Duration::from_secs(120));
    }

    #[test]
    fn a_blocked_poll_waits_the_blocked_backoff_and_warns() {
        let mut st = AppState::new(t(0));
        let (alerts, wait) = poll_once(&mut st, Err(FetchError::Blocked(429)), t(0), &cfg());
        assert_eq!(wait, BLOCKED_BACKOFF);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].kind, AlertKind::FetchFailing);
    }

    #[test]
    fn a_drop_seen_by_the_loop_is_returned_for_the_caller_to_send() {
        let mut st = AppState::new(t(0));
        poll_once(&mut st, Ok(fixture("empty_resale.html")), t(0), &cfg());
        let (alerts, _) = poll_once(&mut st, Ok(fixture("real_available_many.html")), t(1), &cfg());
        assert_eq!(alerts[0].kind, AlertKind::ResaleAvailable);
        assert_eq!(alerts[0].severity, Severity::Max);
    }

    #[test]
    fn a_200_without_a_card_still_ends_the_failure_episode() {
        // The site answered, so "cannot reach the site" is over even though the
        // page is unrecognisable. Otherwise the NEXT outage is never reported.
        let mut st = AppState::new(t(0));
        st.failures_since = Some(t(0));
        st.failure_warned = true;
        st.backoff = Some(BACKOFF_MAX);

        let (alerts, _) = poll_once(&mut st, Ok(fixture("no_card.html")), t(30), &cfg());

        assert_eq!(alerts.len(), 1, "the structure warning must still fire");
        assert_eq!(alerts[0].kind, AlertKind::StructureChanged);
        assert_eq!(st.failures_since, None);
        assert!(!st.failure_warned);
        assert_eq!(st.backoff, None);
    }

    #[test]
    fn every_poll_counts_as_a_check_even_when_blind() {
        let mut st = AppState::new(t(0));
        poll_once(&mut st, net_err(), t(0), &cfg());
        poll_once(&mut st, Ok(fixture("no_card.html")), t(1), &cfg());
        poll_once(&mut st, Ok(fixture("empty_resale.html")), t(2), &cfg());
        assert_eq!(st.checks, 3, "failed and unparseable polls are still polls");
    }

    #[test]
    fn startup_alert_announces_the_target_and_warns_about_fixture_mode() {
        let a = startup_alert(&cfg());
        assert_eq!(a.kind, AlertKind::Started);
        assert_eq!(a.severity, Severity::Info);
        assert!(a.body.contains("url"), "must say what it watches: {}", a.body);
        assert!(
            !a.body.contains("FIXTURE"),
            "no fixture warning in normal mode: {}",
            a.body
        );

        let mut m = HashMap::new();
        m.insert("TELOXIDE_TOKEN".to_string(), "123:ABC".to_string());
        m.insert("TELEGRAM_CHAT_ID".to_string(), "1".to_string());
        m.insert(
            "SB_WATCHER_FIXTURE_PATH".to_string(),
            "tests/fixtures/empty_resale.html".to_string(),
        );
        let rehearsal = startup_alert(&Config::from_map(&m).unwrap());
        assert!(
            rehearsal.body.contains("FIXTURE"),
            "rehearsal mode must be unmistakable: {}",
            rehearsal.body
        );
    }

    #[test]
    fn a_missing_product_frame_raises_the_structure_warning_not_a_max_alert() {
        let html = r#"<div class="card"><div class="card-header"><h2>Ticketbörse</h2></div>
            <div>Es gibt aktuell keine Tickets zum Weiterverkauf.</div></div>"#;
        let mut st = AppState::new(t(0));
        let (alerts, _) = poll_once(&mut st, Ok(html.to_string()), t(0), &cfg());
        let kinds: Vec<AlertKind> = alerts.iter().map(|a| a.kind).collect();
        assert_eq!(kinds, vec![AlertKind::StructureChanged], "{alerts:?}");
        assert!(alerts[0].body.contains("product frame"), "{}", alerts[0].body);
    }
}
