# Review Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the operational gaps found in the 2026-09-06 code review so that a *degraded* watcher can never look healthy, without touching the inverted detector.

**Architecture:** The watcher loop is refactored first so that every state change is a pure, synchronous function that *returns* the alerts to send; the loop sends them after releasing the lock. Every later fix is then a small change to a pure function with a unit test. Detection (`parse.rs`) and the fixtures are untouched except for one additive tri-state on the main-shop frame.

**Tech Stack:** Rust 1.93 (pinned), tokio, teloxide 0.17, reqwest 0.12, scraper, wiremock (dev).

**Spec:** `docs/superpowers/specs/2026-09-06-sb-watcher-design.md` (the original design), plus the review summary at the top of this file. Read `CLAUDE.md` before starting: its six invariants are non-negotiable.

## Global Constraints

- Toolchain is pinned to **1.93** in `rust-toolchain.toml`. Do not change it.
- **reqwest stays at 0.12** with the `rustls-tls` feature. Do not upgrade to 0.13.
- **`takecell` stays at 0.1.1.** Do not run `cargo update` without `--precise` on a specific crate.
- Line limit is **120 columns** (`rustfmt.toml` sets `max_width = 120`).
- **Never edit any file in `tests/fixtures/`.** They are captured real pages.
- The six invariants in `CLAUDE.md` hold throughout. In particular: detection stays inverted, `listings` is never a gate, `CardNotFound` is never quiet, `main` must exit non-zero if a task dies, and `live_site` stays `#[ignore]`d.
- **Never print `TELOXIDE_TOKEN`, `TELEGRAM_CHAT_ID` or `.env` contents.** No task in this plan needs them; everything runs offline.
- The user's shell is **fish**. Every command below is a plain single command and works in fish as written. Do not write `FOO=bar cmd` prefixes.
- Every commit must pass all three gates. Run them together before each commit:
  ```
  cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
  ```
  Commit messages use the repository's conventional prefixes (`fix:`, `feat:`, `refactor:`, `test:`, `ci:`, `docs:`).
- Work on a branch: `git checkout -b fix/review-2026-09-06` before Task 1.

## Review summary (what the tasks fix, in order)

| # | Severity | Finding | Task |
|---|---|---|---|
| 1 | High | Shared mutex is held across every notifier send; loop is untestable | 1 |
| 2 | High | Fetch-failure state resets only on a successful *parse*, so a 200 without a card keeps a stale episode alive and suppresses the next outage warning | 2 |
| 3 | Medium | Blocked (403/429) backoff floor only applies when it is the first failure | 3 |
| 4 | Medium | `checks` counts only successful parses, so a blind watcher looks idle | 4 |
| 5 | High | No startup message; a crash loop under 24h never heartbeats and is invisible | 5 |
| 6 | High | ntfy client has no timeout; ntfy base URL hardcoded so it is untestable | 6 |
| 7 | High | Bot commands (`/ack`, `/status`) accepted from any Telegram chat | 7 |
| 8 | Medium | "Wording changed" alert fires on Available→Available churn and suppresses reminders | 8 |
| 9 | High+Medium | A Max alert whose delivery failed is not retried for 5 min; reminders ignore which condition is open | 9 |
| 10 | Medium | Discovery mode is implicit: a blank `TELEGRAM_CHAT_ID` in prod silently runs bot-only | 10 |
| 11 | Medium | `main_sold_out` is a positive matcher; a renamed frame reads as ON SALE and fires a Max alert | 11 |
| 12 | Low | `setup-flyctl@master` unpinned; no dependency audit; no Dependabot | 12 |
| 13 | Low | Unbounded response body could OOM the 256 MB VM | 13 |
| 14 | Low | Alert text sent untruncated; Telegram rejects > 4096 chars | 14 |
| 15 | Low | Dead `fingerprint`, inline `crate::` paths, stale comment, sleep ignores work time | 15 |
| 16 | Docs | README / CLAUDE.md updates for the above | 16 |

---

### Task 1: Make the watcher loop pure and send alerts outside the lock

**Files:**
- Modify: `src/watcher.rs` (whole file is replaced below)
- No other file changes: `run_watcher`'s signature is unchanged, so `main.rs` compiles as is.

**Interfaces:**
- Consumes: `transitions`, `listing_summary`, `Alert`, `AlertKind`, `Severity` from `src/state.rs`; `classify`, `ParseError` from `src/parse.rs`; `FetchError`, `Fetcher` from `src/fetch.rs`; `Notifier` from `src/notify.rs`.
- Produces (used by every later task):
  - `pub fn apply_observation(st: &mut AppState, obs: PageObservation, now: DateTime<Utc>, url: &str) -> Vec<Alert>`
  - `pub fn apply_failure(st: &mut AppState, err: &FetchError, now: DateTime<Utc>, url: &str) -> Option<Alert>`
  - `pub fn apply_structure_error(st: &mut AppState, now: DateTime<Utc>, url: &str) -> Option<Alert>`
  - `pub fn maybe_heartbeat(st: &mut AppState, now: DateTime<Utc>) -> Option<Alert>`
  - `pub fn poll_once(st: &mut AppState, result: Result<String, FetchError>, now: DateTime<Utc>, cfg: &Config) -> (Vec<Alert>, Duration)`
  - `pub async fn run_watcher<N: Notifier + ?Sized>(cfg: Config, fetcher: Fetcher, notifier: Arc<N>, shared: Arc<Mutex<AppState>>) -> !` (unchanged signature)

This task is **behaviour-preserving**. The old `apply_*` functions were `async`, took a notifier, and sent inside. The new ones are synchronous and return what to send. `poll_once` is the body of the old loop minus locking and sleeping, so the loop's wiring (backoff, resets) becomes unit-testable. `FakeNotifier` in `notify.rs` stays; `notify.rs` tests still use it.

- [ ] **Step 1: Replace `src/watcher.rs` with the following**

```rust
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

/// Fold one successful observation into the state.
///
/// Pure: returns the alerts that must be sent, in order, and never touches the
/// network. The caller sends them after releasing the state lock, so a slow
/// Telegram round-trip can neither stall polling nor block `/ack`.
pub fn apply_observation(st: &mut AppState, obs: PageObservation, now: DateTime<Utc>, url: &str) -> Vec<Alert> {
    st.checks += 1;

    let mut alerts = transitions(st.last.as_ref(), &obs, url);
    let changed = st.last.as_ref() != Some(&obs);

    for alert in &alerts {
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
                alerts.push(Alert::new(
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
                ));
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
    alerts
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

/// Handle a page that parsed but had no Ticketbörse card.
///
/// Rate-limited to one warning per 24h so a permanent redesign does not spam,
/// but it must never be silent: treating this as "all quiet" would leave the
/// watcher blind while looking healthy. Returns the warning to send, if due.
pub fn apply_structure_error(st: &mut AppState, now: DateTime<Utc>, url: &str) -> Option<Alert> {
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
        format!(
            "The Ticketbörse card could not be found. sb-watcher may be blind and needs a code \
             update.\n\n{url}"
        ),
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

    let wait = match result {
        Ok(html) => match classify(&html) {
            Ok(obs) => {
                st.failures_since = None;
                st.failure_warned = false;
                st.backoff = None;
                alerts.extend(apply_observation(st, obs, now, &cfg.target_url));
                cfg.poll_interval
            }
            Err(ParseError::CardNotFound) => {
                alerts.extend(apply_structure_error(st, now, &cfg.target_url));
                cfg.poll_interval
            }
        },
        Err(e) => {
            alerts.extend(apply_failure(st, &e, now, &cfg.target_url));
            let b = if e.is_blocked() && st.backoff.is_none() {
                BLOCKED_BACKOFF
            } else {
                next_backoff(st.backoff, cfg.poll_interval)
            };
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
    loop {
        let result = fetcher.fetch().await;
        // Sampled AFTER the fetch so a slow response does not skew sent_at,
        // last_change and the heartbeat clock by the fetch duration.
        let now = Utc::now();

        // The lock is held only for the pure state update, never across a send:
        // a hung notifier must not stall polling or block /ack and /status.
        let (alerts, wait) = {
            let mut st = shared.lock().await;
            poll_once(&mut st, result, now, &cfg)
        };

        for alert in &alerts {
            if let Err(e) = notifier.send(alert).await {
                log::error!("failed to deliver {:?}: {e:#}", alert.kind);
            }
        }

        tokio::time::sleep(wait).await;
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

    #[test]
    fn a_drop_alerts_once_then_repeats_on_schedule_then_stops() {
        let mut st = AppState::new(t(0));
        st.last = Some(empty_obs());
        let mut sent = Vec::new();

        // The drop happens.
        sent.extend(apply_observation(&mut st, available_obs(), t(0), "url"));
        assert_eq!(sent.len(), 1);
        assert_eq!(st.repeat.as_ref().unwrap().count, 0, "no reminders sent yet");

        // Still available a minute later: too soon to repeat.
        sent.extend(apply_observation(&mut st, available_obs(), t(1), "url"));
        assert_eq!(sent.len(), 1, "must not spam every poll");

        // Five minutes on: one reminder.
        sent.extend(apply_observation(&mut st, available_obs(), t(5), "url"));
        assert_eq!(sent.len(), 2);

        // Drive past the cap.
        for m in [10, 15, 20, 25, 30, 35, 40] {
            sent.extend(apply_observation(&mut st, available_obs(), t(m), "url"));
        }
        assert_eq!(sent.len(), 1 + REPEAT_CAP as usize, "stops after the cap");
    }

    // ---- the safety nets: these are what guarantee it never fails silently ----

    #[test]
    fn missing_card_warns_immediately_but_only_once_per_day() {
        // A site redesign must never read as "all quiet", and must not spam either.
        let mut st = AppState::new(t(0));

        let first = apply_structure_error(&mut st, t(0), "url").expect("first one must warn");
        assert_eq!(first.kind, AlertKind::StructureChanged);

        // Every poll for the next day is suppressed.
        assert!(apply_structure_error(&mut st, t(60), "url").is_none());
        assert!(apply_structure_error(&mut st, t(60 * 23), "url").is_none());

        // But it re-warns after 24h, so a lasting breakage keeps nagging.
        assert!(apply_structure_error(&mut st, t(60 * 24), "url").is_some());
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
        apply_observation(&mut st, available_obs(), t(0), "url");
        assert!(st.repeat.is_some());
        apply_observation(&mut st, empty_obs(), t(1), "url");
        assert!(st.repeat.is_none(), "no point reminding about stock that is gone");
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
}
```

- [ ] **Step 2: Run the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: all green. The test count rises from 57 to 61 (four new `poll_once` tests). If clippy complains about an unused import of `Notifier` or `Fetcher`, it is because a copy step was missed: both are used by `run_watcher`.

- [ ] **Step 3: Commit**

```
git add src/watcher.rs
git commit -m "refactor(watcher): pure state updates, send alerts outside the lock"
```

---

### Task 2: Reset the failure episode on any HTTP success, not only on a successful parse

**Files:**
- Modify: `src/watcher.rs` (`poll_once`, plus one new test)

**Interfaces:** none new.

**Why:** A network outage sets `failure_warned = true` and `backoff = 600s`. If the site then comes back with a 200 page that has no card, the `CardNotFound` arm leaves both untouched. The next real outage then never gets a "cannot reach the site" warning and starts at the 10-minute cap.

- [ ] **Step 1: Write the failing test** (append inside `mod tests` in `src/watcher.rs`)

```rust
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
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test a_200_without_a_card_still_ends_the_failure_episode`
Expected: FAIL on `assert_eq!(st.failures_since, None)`.

- [ ] **Step 3: Move the three resets up into the `Ok(html)` arm**

In `poll_once`, replace:

```rust
        Ok(html) => match classify(&html) {
            Ok(obs) => {
                st.failures_since = None;
                st.failure_warned = false;
                st.backoff = None;
                alerts.extend(apply_observation(st, obs, now, &cfg.target_url));
                cfg.poll_interval
            }
```

with:

```rust
        Ok(html) => {
            // The site answered: the fetch-failure episode is over regardless of
            // whether the page parses. Leaving these set on CardNotFound would
            // suppress the warning for the next real outage.
            st.failures_since = None;
            st.failure_warned = false;
            st.backoff = None;
            match classify(&html) {
                Ok(obs) => {
                    alerts.extend(apply_observation(st, obs, now, &cfg.target_url));
                    cfg.poll_interval
                }
                Err(ParseError::CardNotFound) => {
                    alerts.extend(apply_structure_error(st, now, &cfg.target_url));
                    cfg.poll_interval
                }
            }
        }
```

and delete the old `Err(ParseError::CardNotFound) => { ... }` arm that used to follow the `Ok(obs)` arm (it is now inside the block above).

- [ ] **Step 4: Run the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: green, 62 tests.

- [ ] **Step 5: Commit**

```
git add src/watcher.rs
git commit -m "fix(watcher): end the fetch-failure episode on any HTTP success"
```

---

### Task 3: Fold the blocked-backoff floor into `next_backoff`

**Files:**
- Modify: `src/watcher.rs` (`next_backoff`, `poll_once`, tests)

**Interfaces:**
- Changes: `pub fn next_backoff(current: Option<Duration>, base: Duration, blocked: bool) -> Duration`

**Why:** `BLOCKED_BACKOFF` is applied only when `st.backoff.is_none()`. A timeout (60s) followed by a 429 waits 120s, not the intended 300s floor, and keeps hitting a site that just throttled us.

- [ ] **Step 1: Write the failing test** (append inside `mod tests`)

```rust
    #[test]
    fn a_block_after_a_transient_failure_still_waits_the_blocked_floor() {
        let base = Duration::from_secs(60);
        assert_eq!(next_backoff(None, base, true), BLOCKED_BACKOFF);
        assert_eq!(next_backoff(Some(Duration::from_secs(60)), base, true), BLOCKED_BACKOFF);
        assert_eq!(next_backoff(Some(BLOCKED_BACKOFF), base, true), BACKOFF_MAX);
        // Not blocked: the floor does not apply.
        assert_eq!(next_backoff(Some(Duration::from_secs(60)), base, false), Duration::from_secs(120));
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test a_block_after_a_transient_failure`
Expected: compile error, `next_backoff` takes 2 arguments.

- [ ] **Step 3: Change `next_backoff` and its call site**

Replace the function:

```rust
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
```

In `poll_once`, replace:

```rust
            let b = if e.is_blocked() && st.backoff.is_none() {
                BLOCKED_BACKOFF
            } else {
                next_backoff(st.backoff, cfg.poll_interval)
            };
            st.backoff = Some(b);
            b
```

with:

```rust
            let b = next_backoff(st.backoff, cfg.poll_interval, e.is_blocked());
            st.backoff = Some(b);
            b
```

In the existing test `backoff_doubles_from_the_poll_interval_and_caps`, add `, false` as a third argument to every `next_backoff(...)` call (six calls).

- [ ] **Step 4: Run the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: green, 63 tests. `a_blocked_poll_waits_the_blocked_backoff_and_warns` still passes because `next_backoff(None, 60s, true)` is 300s.

- [ ] **Step 5: Commit**

```
git add src/watcher.rs
git commit -m "fix(watcher): apply the blocked-backoff floor mid-episode too"
```

---

### Task 4: Count every poll in `checks`, not only successful parses

**Files:**
- Modify: `src/watcher.rs`

**Why:** Under a redesign or outage the heartbeat reports yesterday's count, so a busy-but-blind watcher looks idle.

- [ ] **Step 1: Write the failing test** (append inside `mod tests`)

```rust
    #[test]
    fn every_poll_counts_as_a_check_even_when_blind() {
        let mut st = AppState::new(t(0));
        poll_once(&mut st, net_err(), t(0), &cfg());
        poll_once(&mut st, Ok(fixture("no_card.html")), t(1), &cfg());
        poll_once(&mut st, Ok(fixture("empty_resale.html")), t(2), &cfg());
        assert_eq!(st.checks, 3, "failed and unparseable polls are still polls");
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test every_poll_counts_as_a_check`
Expected: FAIL, `left: 1, right: 3`.

- [ ] **Step 3: Move the increment**

Delete the line `st.checks += 1;` at the top of `apply_observation`. Add `st.checks += 1;` as the first statement of `poll_once`, directly after `let mut alerts = Vec::new();`.

- [ ] **Step 4: Run the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: green, 64 tests.

- [ ] **Step 5: Commit**

```
git add src/watcher.rs
git commit -m "fix(watcher): count every poll so a blind watcher does not look idle"
```

---

### Task 5: Send a startup message

**Files:**
- Modify: `src/state.rs` (add `AlertKind::Started`)
- Modify: `src/watcher.rs` (add `startup_alert`, call it in `run_watcher`)

**Interfaces:**
- Produces: `pub fn startup_alert(cfg: &Config) -> Alert` in `src/watcher.rs`; `AlertKind::Started` in `src/state.rs`.

**Why:** `last_heartbeat` is reset to boot time, so a process that restarts more often than every 24h never heartbeats and nothing announces the restart. The design spec's verification step 2 already assumes a startup message exists. Sending one also proves the notifier pipeline works at boot.

- [ ] **Step 1: Add the variant**

In `src/state.rs`, in `pub enum AlertKind`, add `Started,` as the last variant after `Heartbeat,`.

- [ ] **Step 2: Write the failing test** (append inside `mod tests` in `src/watcher.rs`)

```rust
    #[test]
    fn startup_alert_announces_the_target_and_warns_about_fixture_mode() {
        let a = startup_alert(&cfg());
        assert_eq!(a.kind, AlertKind::Started);
        assert_eq!(a.severity, Severity::Info);
        assert!(a.body.contains("url"), "must say what it watches: {}", a.body);
        assert!(!a.body.contains("FIXTURE"), "no fixture warning in normal mode: {}", a.body);

        let mut m = HashMap::new();
        m.insert("TELOXIDE_TOKEN".to_string(), "123:ABC".to_string());
        m.insert(
            "SB_WATCHER_FIXTURE_PATH".to_string(),
            "tests/fixtures/empty_resale.html".to_string(),
        );
        let rehearsal = startup_alert(&Config::from_map(&m).unwrap());
        assert!(rehearsal.body.contains("FIXTURE"), "rehearsal mode must be unmistakable: {}", rehearsal.body);
    }
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cargo test startup_alert_announces`
Expected: compile error, `startup_alert` not found.

- [ ] **Step 4: Implement**

Add to `src/watcher.rs`, directly above `pub fn poll_once`:

```rust
/// Sent once at boot. Makes restarts visible (a crash loop shorter than the
/// heartbeat interval would otherwise be silent) and proves at start-up that
/// alerts can actually be delivered.
pub fn startup_alert(cfg: &Config) -> Alert {
    let mut body = format!(
        "Watching {} every {}s.",
        cfg.target_url,
        cfg.poll_interval.as_secs()
    );
    if let Some(p) = &cfg.fixture_path {
        body.push_str(&format!("\n\n⚠️ FIXTURE MODE: reading {} instead of the live site.", p.display()));
    }
    Alert::new(AlertKind::Started, Severity::Info, "▶️ sb-watcher started", body)
}
```

In `run_watcher`, insert before `loop {`:

```rust
    if let Err(e) = notifier.send(&startup_alert(&cfg)).await {
        log::error!("failed to deliver the startup message: {e:#}");
    }
```

- [ ] **Step 5: Run the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: green, 65 tests.

- [ ] **Step 6: Commit**

```
git add src/state.rs src/watcher.rs
git commit -m "feat(watcher): send a startup message so restarts are visible"
```

---

### Task 6: ntfy client timeouts and an injectable base URL

**Files:**
- Modify: `src/notify.rs` (`NtfyNotifier`, new `ntfy_client`, new tests)
- Modify: `src/main.rs` (construction site)

**Interfaces:**
- Changes: `NtfyNotifier::new(client: reqwest::Client, base_url: impl Into<String>, topic: String)`
- Produces: `pub const NTFY_DEFAULT_BASE_URL: &str = "https://ntfy.sh"`, `pub fn ntfy_client() -> reqwest::Result<reqwest::Client>`

**Why:** `main.rs` builds the ntfy client with `reqwest::Client::new()`, which has no connect or request timeout. One hung ntfy connection blocks the whole send. The hardcoded `https://ntfy.sh/` also made the notifier untestable, while `Fetcher` already has wiremock tests.

- [ ] **Step 1: Write the failing tests** (append inside `mod tests` in `src/notify.rs`)

```rust
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
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test ntfy_`
Expected: compile errors: `ntfy_client` not found, `NtfyNotifier::new` takes 2 arguments.

- [ ] **Step 3: Implement**

In `src/notify.rs`, replace the whole `// ---------- ntfy ----------` section (struct, `impl NtfyNotifier`, and the `impl Notifier for NtfyNotifier`) with:

```rust
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
            .body(format!("{}\n\n{}", alert.title, alert.body))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(anyhow!("ntfy returned {}", resp.status()));
        }
        Ok(())
    }
}
```

In `src/main.rs`, change the import line

```rust
use sb_watcher::notify::{MultiNotifier, Notifier, NtfyNotifier, TelegramNotifier};
```

to

```rust
use sb_watcher::notify::{ntfy_client, MultiNotifier, Notifier, NtfyNotifier, TelegramNotifier, NTFY_DEFAULT_BASE_URL};
```

and replace

```rust
            channels.push(Box::new(NtfyNotifier::new(reqwest::Client::new(), topic.clone())));
```

with

```rust
            let client = ntfy_client().context("failed to build ntfy HTTP client")?;
            channels.push(Box::new(NtfyNotifier::new(client, NTFY_DEFAULT_BASE_URL, topic.clone())));
```

- [ ] **Step 4: Run the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: green, 68 tests.

- [ ] **Step 5: Commit**

```
git add src/notify.rs src/main.rs
git commit -m "fix(notify): ntfy client timeouts; injectable base URL with wiremock tests"
```

---

### Task 7: Only the configured chat may use bot commands

**Files:**
- Modify: `src/bot.rs`
- Modify: `src/main.rs` (two `run_bot` call sites)

**Interfaces:**
- Produces: `pub struct AllowedChat(pub Option<ChatId>)` with `pub fn permits(self, chat: ChatId) -> bool`
- Changes: `pub async fn run_bot(bot: Bot, shared: Arc<Mutex<AppState>>, url: String, allowed: AllowedChat)`

**Why:** `on_command` and `on_message` answer any Telegram chat. Anyone who finds the bot's username can `/ack` your reminders mid-drop or read `/status`. In discovery mode (no chat id configured) everything stays open, since replying with the chat id is the whole point of that mode.

- [ ] **Step 1: Write the failing test** (append inside `mod tests` in `src/bot.rs`)

```rust
    #[test]
    fn only_the_configured_chat_is_permitted() {
        let mine = ChatId(42);
        let stranger = ChatId(7);
        assert!(AllowedChat(Some(mine)).permits(mine));
        assert!(!AllowedChat(Some(mine)).permits(stranger), "a stranger must not be able to /ack");
        // Discovery mode: nothing configured yet, so every chat is answered.
        assert!(AllowedChat(None).permits(stranger));
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test only_the_configured_chat`
Expected: compile error, `AllowedChat` not found.

- [ ] **Step 3: Implement in `src/bot.rs`**

Add after the `Command` enum:

```rust
/// The one chat allowed to drive the bot. `None` means discovery mode, where
/// every chat is answered so the user can learn their chat id.
#[derive(Debug, Clone, Copy)]
pub struct AllowedChat(pub Option<ChatId>);

impl AllowedChat {
    pub fn permits(self, chat: ChatId) -> bool {
        self.0.is_none_or(|c| c == chat)
    }
}
```

Add `use teloxide::types::ChatId;` to the imports at the top of the file.

Change `on_command`'s signature and add the check as its first statement:

```rust
async fn on_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    shared: Arc<Mutex<AppState>>,
    url: String,
    allowed: AllowedChat,
) -> ResponseResult<()> {
    if !allowed.permits(msg.chat.id) {
        log::warn!("ignoring command from unauthorised chat {}", msg.chat.id.0);
        return Ok(());
    }
    // ... existing body unchanged from here ...
```

Change `on_message` likewise:

```rust
async fn on_message(bot: Bot, msg: Message, allowed: AllowedChat) -> ResponseResult<()> {
    if !allowed.permits(msg.chat.id) {
        log::warn!("ignoring message from unauthorised chat {}", msg.chat.id.0);
        return Ok(());
    }
    // ... existing body unchanged from here ...
```

Change `run_bot`:

```rust
pub async fn run_bot(bot: Bot, shared: Arc<Mutex<AppState>>, url: String, allowed: AllowedChat) {
    let handler = Update::filter_message()
        .branch(dptree::entry().filter_command::<Command>().endpoint(on_command))
        .branch(dptree::endpoint(on_message));

    // No listener passed: teloxide defaults to long polling (getUpdates), so no
    // public URL or inbound port is required.
    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![shared, url, allowed])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}
```

- [ ] **Step 4: Update `src/main.rs`**

Change the import `use sb_watcher::bot::run_bot;` to `use sb_watcher::bot::{run_bot, AllowedChat};`.

In the discovery branch, change `run_bot(bot, shared, cfg.target_url.clone()).await;` to
`run_bot(bot, shared, cfg.target_url.clone(), AllowedChat(None)).await;`.

Change the spawn line
`let commands = tokio::spawn(run_bot(bot, shared, cfg.target_url.clone()));` to
`let commands = tokio::spawn(run_bot(bot, shared, cfg.target_url.clone(), AllowedChat(Some(ChatId(chat_id)))));`.

- [ ] **Step 5: Run the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: green, 69 tests. If clippy suggests `is_none_or`, you already use it; if it errors that `is_none_or` does not exist, the toolchain is not 1.93 — run `rustup show` and stop.

- [ ] **Step 6: Commit**

```
git add src/bot.rs src/main.rs
git commit -m "fix(bot): only answer the configured chat"
```

---

### Task 8: Only report a wording change while the exchange is still empty

**Files:**
- Modify: `src/state.rs` (one match arm)
- Test: `tests/transition_tests.rs`

**Why:** The `_ if p.resale_text != cur.resale_text` arm exists to catch a rewrite of the empty sentence, which would blind the inverted detector. It also matches Available→Available, so during a live drop every offer that sells produces an Info alert with two full card-text dumps.

- [ ] **Step 1: Write the failing test** (append to `tests/transition_tests.rs`)

```rust
#[test]
fn listing_churn_while_available_is_not_a_wording_change() {
    // During a drop the card text changes every time an offer sells. That is
    // not the detector going blind and must not spam Info alerts.
    let mut before = available();
    before.resale_text = "Ticketbörse Festivalticket 222,00 Festivalticket 222,00 In den Warenkorb".into();
    let after = available();
    assert!(transitions(Some(&before), &after, URL).is_empty());
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --test transition_tests listing_churn`
Expected: FAIL, the vec contains `ResaleTextChanged`.

- [ ] **Step 3: Narrow the arm**

In `src/state.rs`, replace:

```rust
                // Same state, but the wording moved: worth knowing, since a
                // rewrite of the empty sentence would otherwise blind the detector.
                _ if p.resale_text != cur.resale_text => out.push(Alert::new(
```

with:

```rust
                // Still empty, but the wording moved: worth knowing, since a
                // rewrite of the empty sentence would otherwise blind the detector.
                // Deliberately NOT for Available→Available: listings churn while
                // stock is up, and that is not the detector going blind.
                (ResaleState::Empty, ResaleState::Empty) if p.resale_text != cur.resale_text => out.push(Alert::new(
```

- [ ] **Step 4: Run the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: green, 70 tests. `wording_change_while_still_empty_is_informational` still passes.

- [ ] **Step 5: Commit**

```
git add src/state.rs tests/transition_tests.rs
git commit -m "fix(state): report wording changes only while the exchange is still empty"
```

---

### Task 9: Per-kind reminders that retry an undelivered alert

**Files:**
- Modify: `src/watcher.rs` (`AlertRepeat`, `AppState.repeat` → `repeats`, `apply_observation`, new `record_delivery`, `run_watcher`, tests)
- Modify: `src/notify.rs` (`MultiNotifier` returns `Ok` when at least one channel delivered; one test changes)
- Modify: `src/bot.rs` (`status_text` and `/ack` read `repeats`)

**Interfaces:**
- Changes: `pub struct AlertRepeat { pub kind: AlertKind, pub sent_at: DateTime<Utc>, pub count: u32, pub delivered: bool }`
- Changes: `AppState.repeat: Option<AlertRepeat>` becomes `AppState.repeats: Vec<AlertRepeat>`
- Produces: `pub fn record_delivery(st: &mut AppState, kind: AlertKind, delivered: bool)`
- Changes: `MultiNotifier::send` returns `Err` only when **every** channel failed.

**Why (three findings):**
1. A Max alert whose delivery failed is armed with `sent_at = now`, so the first retry is the 5-minute reminder. If Telegram is down at the moment of a drop the user learns five minutes late, or never.
2. One shared `repeat` slot plus a merged `still_alerting` flag means a `MainOnSale` reminder says "STILL AVAILABLE" with the empty-Ticketbörse fallback text, and resale reminders keep firing after resale is empty as long as the main shop is on sale.
3. Reminders were gated on `alerts.is_empty()`, so any Info alert on the same poll delayed a due reminder.

**Semantics after this task:**
- One `AlertRepeat` per open Max condition (`ResaleAvailable`, `MainOnSale`), each cleared when *its own* condition resolves.
- `delivered` starts `false`; the loop calls `record_delivery` after sending. If it is still `false` at the next poll, the alert is re-sent immediately and does **not** count toward `REPEAT_CAP`. If every channel is permanently broken this retries every poll and logs an error each time, which is the intended behaviour for a watcher whose only job is to alert.
- "Delivered" means at least one channel accepted the message, hence the `MultiNotifier` change.

- [ ] **Step 1: Change `MultiNotifier` semantics** (`src/notify.rs`)

Replace the tail of `MultiNotifier::send`:

```rust
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
```

with:

```rust
        // Delivered means the user can see it somewhere. A partial failure is
        // logged above but is not a failure of the alert.
        if errors.is_empty() || errors.len() < self.channels.len() {
            Ok(())
        } else {
            Err(anyhow!(
                "all {} channels failed: {}",
                self.channels.len(),
                errors.join("; ")
            ))
        }
```

In the test `multi_still_delivers_when_one_channel_fails`, replace

```rust
        assert!(r.is_err(), "the failure is still reported to the caller");
```

with

```rust
        assert!(r.is_ok(), "one channel reaching the user counts as delivered");
```

and append this test inside `mod tests`:

```rust
    #[tokio::test]
    async fn multi_fails_only_when_every_channel_fails() {
        let a = Arc::new(FakeNotifier::new());
        let b = Arc::new(FakeNotifier::new());
        a.fail_next();
        b.fail_next();
        let multi = MultiNotifier::new(vec![Box::new(a.clone()), Box::new(b.clone())]);
        assert!(multi.send(&alert()).await.is_err(), "nobody got it, so it was not delivered");
    }
```

- [ ] **Step 2: Write the failing watcher tests**

In `src/watcher.rs` `mod tests`, add this helper after `fn net_err()`:

```rust
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
            main_sold_out: false,
        }
    }
```

Replace the test `a_drop_alerts_once_then_repeats_on_schedule_then_stops` with:

```rust
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
```

Replace the test `stock_going_away_clears_the_repeat` with:

```rust
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
```

In `repeat_waits_the_interval` and `repeat_stops_at_the_cap`, add `delivered: true,` to the `AlertRepeat { ... }` literal.

Append these new tests:

```rust
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
            main_sold_out: false,
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
        let mut with_main_sold_out_again = available_obs();
        with_main_sold_out_again.main_sold_out = true;
        st.last.as_mut().unwrap().main_sold_out = false;
        let a = apply_observation(&mut st, with_main_sold_out_again, t(5), "url");
        let kinds: Vec<AlertKind> = a.iter().map(|x| x.kind).collect();
        assert!(kinds.contains(&AlertKind::MainSoldOut), "{kinds:?}");
        assert!(kinds.contains(&AlertKind::ResaleAvailable), "reminder must still go out: {kinds:?}");
    }
```

- [ ] **Step 3: Run them to verify they fail**

Run: `cargo test --lib watcher`
Expected: compile errors (`repeats`, `delivered`, `record_delivery` unknown).

- [ ] **Step 4: Implement in `src/watcher.rs`**

Replace the `AlertRepeat` struct:

```rust
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
```

In `AppState`, replace the field `pub repeat: Option<AlertRepeat>,` with `pub repeats: Vec<AlertRepeat>,` and in `AppState::new` replace `repeat: None,` with `repeats: Vec::new(),`.

Add these two helpers directly above `pub fn apply_observation`:

```rust
/// Whether the condition behind a Max alert is still true in `obs`.
fn condition_open(kind: AlertKind, obs: &PageObservation) -> bool {
    match kind {
        AlertKind::ResaleAvailable => obs.resale == ResaleState::Available,
        AlertKind::MainOnSale => !obs.main_sold_out,
        _ => false,
    }
}

/// What a reminder for `kind` should say about the current page.
fn reminder_body(kind: AlertKind, obs: &PageObservation, url: &str) -> String {
    match kind {
        AlertKind::MainOnSale => format!("The main shop is still not showing 'Ausverkauft'.\n\n{url}"),
        _ => format!("{}\n\n{}", listing_summary(&obs.listings, &obs.resale_text), url),
    }
}
```

Replace the whole `apply_observation` function:

```rust
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

    // Each repeat lives exactly as long as its own condition.
    st.repeats.retain(|r| condition_open(r.kind, &obs));

    for r in st.repeats.iter_mut() {
        if fired.contains(&r.kind) {
            continue;
        }
        if !r.delivered {
            // Nothing reached the user last time: resend now, and do not let it
            // eat into the reminder cap.
            alerts.push(Alert::new(
                r.kind,
                Severity::Max,
                "🎟️ STILL AVAILABLE",
                format!(
                    "The previous alert could not be delivered.\n\n{}",
                    reminder_body(r.kind, &obs, url)
                ),
            ));
            r.sent_at = now;
        } else if should_repeat(r, now) {
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
```

In `run_watcher`, replace the send loop:

```rust
        for alert in &alerts {
            if let Err(e) = notifier.send(alert).await {
                log::error!("failed to deliver {:?}: {e:#}", alert.kind);
            }
        }
```

with:

```rust
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
```

- [ ] **Step 5: Update `src/bot.rs`**

In `status_text`, replace:

```rust
    let repeat = match &st.repeat {
        Some(r) => format!(
            "\nalerting: {} of {} reminders sent",
            r.count,
            crate::watcher::REPEAT_CAP
        ),
        None => String::new(),
    };
```

with:

```rust
    let repeat: String = st
        .repeats
        .iter()
        .map(|r| format!("\nalerting {:?}: {} of {} reminders sent", r.kind, r.count, crate::watcher::REPEAT_CAP))
        .collect();
```

In `on_command`, replace the `Command::Ack` arm body:

```rust
        Command::Ack => {
            let mut st = shared.lock().await;
            if st.repeat.take().is_some() {
                "Acknowledged — reminders stopped.".to_string()
            } else {
                "Nothing to acknowledge.".to_string()
            }
        }
```

with:

```rust
        Command::Ack => {
            let mut st = shared.lock().await;
            if st.repeats.is_empty() {
                "Nothing to acknowledge.".to_string()
            } else {
                st.repeats.clear();
                "Acknowledged — reminders stopped.".to_string()
            }
        }
```

- [ ] **Step 6: Run the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: green, 75 tests. Run `grep -rnw repeat src` and expect hits only in comments; a hit on a field access such as `st.repeat` or `.repeat.take()` is a missed rename.

- [ ] **Step 7: Commit**

```
git add src/watcher.rs src/notify.rs src/bot.rs
git commit -m "fix(watcher): per-condition reminders; retry an undelivered Max alert next poll"
```

---

### Task 10: Make discovery mode explicit

**Files:**
- Modify: `src/config.rs`
- Modify: `src/main.rs`
- Modify test helpers that build a `Config` with only a token: `src/fetch.rs`, `src/watcher.rs`, `tests/live_site.rs`
- Modify: `README.md` (configuration table and setup step 2)

**Interfaces:**
- Produces: `Config.discovery: bool` from env `SB_WATCHER_DISCOVERY` (`1`, `true`, `yes`, case-insensitive).
- Changes: `Config::from_map` errors when `TELEGRAM_CHAT_ID` is missing and `discovery` is false.

**Why:** A blank `TELEGRAM_CHAT_ID` secret (deploy tooling often sets empty strings) silently runs bot-only: no fetch loop, no heartbeat, machine green on fly for weeks. Fail-loud belongs in `Config::from_map`, where it is already tested.

- [ ] **Step 1: Write the failing tests** (in `src/config.rs` `mod tests`)

Change `base()` so the default test config is a valid production config:

```rust
    fn base() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("TELOXIDE_TOKEN".into(), "123:ABC".into());
        m.insert("TELEGRAM_CHAT_ID".into(), "1".into());
        m
    }
```

In `applies_defaults`, replace `assert_eq!(cfg.chat_id, None);` with `assert_eq!(cfg.chat_id, Some(1));` and add `assert!(!cfg.discovery);`.

Append:

```rust
    #[test]
    fn requires_chat_id_unless_discovery_is_explicit() {
        let mut m = HashMap::new();
        m.insert("TELOXIDE_TOKEN".into(), "123:ABC".into());
        let err = Config::from_map(&m).unwrap_err().to_string();
        assert!(err.contains("SB_WATCHER_DISCOVERY"), "the error must say how to fix it: {err}");

        m.insert("SB_WATCHER_DISCOVERY".into(), "1".into());
        let cfg = Config::from_map(&m).unwrap();
        assert!(cfg.discovery);
        assert_eq!(cfg.chat_id, None);
    }

    #[test]
    fn blank_chat_id_is_a_startup_error_not_silent_discovery() {
        // `fly secrets set TELEGRAM_CHAT_ID=''` must not produce a bot-only deploy.
        let mut m = base();
        m.insert("TELEGRAM_CHAT_ID".into(), "  ".into());
        assert!(Config::from_map(&m).is_err());
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib config`
Expected: compile error, no field `discovery`.

- [ ] **Step 3: Implement in `src/config.rs`**

Add `pub discovery: bool,` to `struct Config` after `chat_id`.

In `from_map`, directly after the `let chat_id = match ... ;` block, add:

```rust
        let discovery = opt(map, "SB_WATCHER_DISCOVERY")
            .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"));

        if chat_id.is_none() && !discovery {
            return Err(anyhow!(
                "TELEGRAM_CHAT_ID is required. To find it, run once with SB_WATCHER_DISCOVERY=1 and \
                 message the bot; it replies with the id."
            ));
        }
```

Add `discovery,` to the `Ok(Config { ... })` literal after `chat_id,`.

- [ ] **Step 4: Update `src/main.rs`**

Replace:

```rust
    let Some(chat_id) = cfg.chat_id else {
        // Discovery mode: no chat configured, so just help the user find theirs.
        log::warn!("TELEGRAM_CHAT_ID is not set — running in discovery mode.");
        log::warn!("Message the bot on Telegram and it will reply with the chat id to use.");
        run_bot(bot, shared, cfg.target_url.clone(), AllowedChat(None)).await;
        return Ok(());
    };
```

with:

```rust
    if cfg.discovery {
        // Explicit opt-in only: an accidentally blank TELEGRAM_CHAT_ID must be a
        // startup error, never a silent bot-only deployment with no watcher.
        log::warn!("SB_WATCHER_DISCOVERY is set — running in discovery mode, NOT watching.");
        log::warn!("Message the bot on Telegram and it will reply with the chat id to use.");
        run_bot(bot, shared, cfg.target_url.clone(), AllowedChat(None)).await;
        return Ok(());
    }
    let Some(chat_id) = cfg.chat_id else {
        anyhow::bail!("TELEGRAM_CHAT_ID missing outside discovery mode; Config::from_map should have rejected this");
    };
```

- [ ] **Step 5: Fix every test helper that built a token-only config**

Each of these now needs a chat id. Add the line `m.insert("TELEGRAM_CHAT_ID".to_string(), "1".to_string());` immediately after the existing `TELOXIDE_TOKEN` insert in:

- `src/fetch.rs`: `cfg_with`, `fetcher_for`, and `an_unreachable_host_is_a_network_error` (three places).
- `src/watcher.rs`: `cfg()` and the second map inside `startup_alert_announces_the_target_and_warns_about_fixture_mode` (two places).
- `tests/live_site.rs`: after `env.insert("TELOXIDE_TOKEN"...)` (one place; use `env` as the map name there).

Run `grep -rn '"TELOXIDE_TOKEN"' src tests` and check every hit has a `TELEGRAM_CHAT_ID` insert next to it, except `config.rs` tests that deliberately omit it.

- [ ] **Step 6: Update `README.md`**

In the configuration table, change the `TELEGRAM_CHAT_ID` row to:

```
| `TELEGRAM_CHAT_ID` | yes* | — | *Not needed when `SB_WATCHER_DISCOVERY=1` (below) |
```

and add a row after it:

```
| `SB_WATCHER_DISCOVERY` | no | — | `1` ⇒ discovery mode: bot only, no watching; replies with chat ids |
```

In Setup step 2, change the sentence and the fish block to:

```
2. Find your chat id — run in discovery mode and message the bot; it replies with the id:
   ```fish
   set -x TELOXIDE_TOKEN "123456:ABC..."
   set -x SB_WATCHER_DISCOVERY 1
   cargo run
   ```
```

- [ ] **Step 7: Run the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: green, 77 tests.

- [ ] **Step 8: Commit**

```
git add src/config.rs src/main.rs src/fetch.rs src/watcher.rs tests/live_site.rs README.md
git commit -m "fix(config): require TELEGRAM_CHAT_ID unless SB_WATCHER_DISCOVERY is set"
```

---

### Task 11: Tri-state main-shop detection

**Files:**
- Modify: `src/parse.rs` (`MainStock`, `PageObservation.main`)
- Modify: `src/state.rs` (`transitions`)
- Modify: `src/watcher.rs` (`condition_open`, `apply_structure_error` gains a `what` argument, `poll_once`, tests)
- Modify: `src/bot.rs` (`status_text` and tests)
- Modify: `tests/parse_tests.rs`, `tests/transition_tests.rs`, `tests/live_site.rs`

**Interfaces:**
- Produces: `pub enum MainStock { SoldOut, OnSale, Unknown }` in `src/parse.rs`
- Changes: `PageObservation.main_sold_out: bool` becomes `PageObservation.main: MainStock`
- Changes: `pub fn apply_structure_error(st: &mut AppState, now: DateTime<Utc>, url: &str, what: &str) -> Option<Alert>`

**Why:** `main_sold_out` is a positive matcher: frame found *and* banner found. If sbtix renames `turbo-frame#ticket_detail`, the page reads as ON SALE and a Max alert plus six reminders fire at 3am for a page that has not changed. The resale side already has the right model (`CardNotFound` → rate-limited structure warning). This does **not** touch the resale detector, which stays inverted.

Rules after this task:
- Frame missing → `Unknown`. Frame present with an `Ausverkauft` danger banner → `SoldOut`. Frame present without → `OnSale`.
- `MainOnSale` (Max) fires on `SoldOut→OnSale`, `Unknown→OnSale` (fail open), and first observation `OnSale`.
- `MainSoldOut` (Info) fires on `OnSale→SoldOut`.
- `Unknown` never fires a Max alert. The watcher reports it through the existing rate-limited structure warning.

- [ ] **Step 1: Write the failing tests**

In `tests/parse_tests.rs`, change the import to `use sb_watcher::parse::{classify, normalize_ws, MainStock, ParseError, ResaleState};`. In `real_live_sbtix_page_is_empty` replace `assert!(obs.main_sold_out, "the live page shows Ausverkauft");` with `assert_eq!(obs.main, MainStock::SoldOut, "the live page shows Ausverkauft");`. In `detects_main_product_back_on_sale` replace `assert!(!classify(...).unwrap().main_sold_out);` with `assert_eq!(classify(&fixture("main_on_sale.html")).unwrap().main, MainStock::OnSale);`. Append:

```rust
#[test]
fn a_missing_product_frame_is_unknown_not_on_sale() {
    // A renamed frame must produce a structure warning, not a 3am Max alert.
    let html = r#"<html><body>
        <div class="card">
          <div class="card-header"><h2>Ticketbörse</h2></div>
          <div class="card-body">Es gibt aktuell keine Tickets zum Weiterverkauf.</div>
        </div>
    </body></html>"#;
    let obs = classify(html).unwrap();
    assert_eq!(obs.resale, ResaleState::Empty);
    assert_eq!(obs.main, MainStock::Unknown);
}
```

In `tests/transition_tests.rs`, change the import to `use sb_watcher::parse::{Listing, MainStock, PageObservation, ResaleState};` and change the helper:

```rust
fn obs(resale: ResaleState, text: &str, main: MainStock) -> PageObservation {
    PageObservation {
        resale,
        resale_text: text.to_string(),
        listings: vec![],
        main,
    }
}
```

Then update every call: `true` → `MainStock::SoldOut`, `false` → `MainStock::OnSale`. That is `empty()`, `available()`, `first_run_main_on_sale_alerts_immediately`, `main_going_on_sale_is_a_max_alert`, `main_selling_out_again_is_informational`, `resale_and_main_can_fire_together`. Append:

```rust
#[test]
fn unknown_main_frame_is_not_an_on_sale_alert() {
    let cur = obs(ResaleState::Empty, empty().resale_text.as_str(), MainStock::Unknown);
    assert!(transitions(Some(&empty()), &cur, URL).is_empty(), "Unknown is a structure problem, not a drop");
    assert!(transitions(None, &cur, URL).is_empty());
}

#[test]
fn recovering_from_unknown_straight_to_on_sale_fails_open() {
    let prev = obs(ResaleState::Empty, empty().resale_text.as_str(), MainStock::Unknown);
    let cur = obs(ResaleState::Empty, empty().resale_text.as_str(), MainStock::OnSale);
    assert_eq!(kinds(&transitions(Some(&prev), &cur, URL)), vec![AlertKind::MainOnSale]);
}
```

In `src/watcher.rs` `mod tests`, append:

```rust
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
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test`
Expected: compile errors about `MainStock` / `main`.

- [ ] **Step 3: Implement `src/parse.rs`**

Add after `pub enum ResaleState { ... }`:

```rust
/// State of the main (non-resale) product. Unlike `ResaleState` this is a
/// positive match on a specific frame, so it carries an explicit `Unknown`:
/// a renamed frame must become a structure warning, never an on-sale alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainStock {
    SoldOut,
    OnSale,
    /// `turbo-frame#ticket_detail` was not found.
    Unknown,
}
```

In `PageObservation` replace `pub main_sold_out: bool,` with `pub main: MainStock,`.

In `classify`, replace:

```rust
    let main_sold_out = doc
        .select(&TICKET_FRAME)
        .next()
        .is_some_and(|frame| frame.select(&DANGER).any(|a| text_of(&a).contains(SOLD_OUT_MARKER)));

    Ok(PageObservation {
        resale,
        resale_text,
        listings,
        main_sold_out,
    })
```

with:

```rust
    let main = match doc.select(&TICKET_FRAME).next() {
        None => MainStock::Unknown,
        Some(frame) if frame.select(&DANGER).any(|a| text_of(&a).contains(SOLD_OUT_MARKER)) => MainStock::SoldOut,
        Some(_) => MainStock::OnSale,
    };

    Ok(PageObservation {
        resale,
        resale_text,
        listings,
        main,
    })
```

- [ ] **Step 4: Implement `src/state.rs`**

Change the import to `use crate::parse::{Listing, MainStock, PageObservation, ResaleState};`.

In `transitions`, in the `None =>` arm replace `if !cur.main_sold_out {` with `if cur.main == MainStock::OnSale {`.

In the `Some(p) =>` arm replace the second match:

```rust
            match (p.main_sold_out, cur.main_sold_out) {
                (true, false) => main_on_sale(&mut out),
                (false, true) => out.push(Alert::new(
```

with:

```rust
            match (p.main, cur.main) {
                // Fail open: coming back from Unknown straight to OnSale is
                // treated as a drop rather than swallowed.
                (MainStock::SoldOut | MainStock::Unknown, MainStock::OnSale) => main_on_sale(&mut out),
                (MainStock::OnSale, MainStock::SoldOut) => out.push(Alert::new(
```

The `_ => {}` arm that follows already covers every transition into `Unknown`; the watcher reports those.

- [ ] **Step 5: Implement `src/watcher.rs`**

Change the parse import to `use crate::parse::{classify, MainStock, PageObservation, ParseError, ResaleState};`.

In `condition_open`, replace `AlertKind::MainOnSale => !obs.main_sold_out,` with `AlertKind::MainOnSale => obs.main == MainStock::OnSale,`.

Replace `apply_structure_error` with:

```rust
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
```

In `poll_once`, replace the `Ok(obs)` and `CardNotFound` arms:

```rust
                Ok(obs) => {
                    alerts.extend(apply_observation(st, obs, now, &cfg.target_url));
                    cfg.poll_interval
                }
                Err(ParseError::CardNotFound) => {
                    alerts.extend(apply_structure_error(st, now, &cfg.target_url));
                    cfg.poll_interval
                }
```

with:

```rust
                Ok(obs) => {
                    let main_unknown = obs.main == MainStock::Unknown;
                    alerts.extend(apply_observation(st, obs, now, &cfg.target_url));
                    if main_unknown {
                        alerts.extend(apply_structure_error(st, now, &cfg.target_url, "The main product frame"));
                    }
                    cfg.poll_interval
                }
                Err(ParseError::CardNotFound) => {
                    alerts.extend(apply_structure_error(st, now, &cfg.target_url, "The Ticketbörse card"));
                    cfg.poll_interval
                }
```

In the tests module: in `empty_obs`, `available_obs` replace `main_sold_out: true,` with `main: MainStock::SoldOut,`; in `main_on_sale_obs` and the `both` literal in `resale_going_empty_stops_resale_reminders_even_while_main_is_on_sale` replace `main_sold_out: false,` with `main: MainStock::OnSale,`. In `an_info_alert_on_the_same_poll_does_not_delay_a_due_reminder` replace `with_main_sold_out_again.main_sold_out = true;` with `with_main_sold_out_again.main = MainStock::SoldOut;` and `st.last.as_mut().unwrap().main_sold_out = false;` with `st.last.as_mut().unwrap().main = MainStock::OnSale;`. In `missing_card_warns_immediately_but_only_once_per_day`, add a fourth argument `"The Ticketbörse card"` to all four `apply_structure_error(...)` calls.

- [ ] **Step 6: Implement `src/bot.rs`**

Change the parse import to `use crate::parse::{MainStock, ResaleState};`. In `status_text` replace:

```rust
            let main = if o.main_sold_out {
                "main shop: sold out"
            } else {
                "main shop: ON SALE"
            };
```

with:

```rust
            let main = match o.main {
                MainStock::SoldOut => "main shop: sold out",
                MainStock::OnSale => "main shop: ON SALE",
                MainStock::Unknown => "main shop: UNKNOWN (product frame not found)",
            };
```

In the tests module, `use crate::parse::PageObservation;` becomes `use crate::parse::{MainStock, PageObservation};` and both `main_sold_out: true,` literals become `main: MainStock::SoldOut,`.

- [ ] **Step 7: Fix `tests/live_site.rs`**

Replace `println!("main_sold_out : {}", obs.main_sold_out);` with `println!("main          : {:?}", obs.main);`.

- [ ] **Step 8: Run the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: green, 81 tests. Then run `grep -rn "main_sold_out" src tests docs/superpowers/plans/2026-09-06-sb-watcher.md README.md CLAUDE.md` and expect hits only in the old plan document (historical, leave it).

- [ ] **Step 9: Commit**

```
git add src tests
git commit -m "feat(parse): tri-state main-shop detection; unknown frame is a structure warning"
```

---

### Task 12: CI supply-chain hardening

**Files:**
- Modify: `.github/workflows/ci.yml`
- Create: `.github/dependabot.yml`

**Why:** `superfly/flyctl-actions/setup-flyctl@master` is an unpinned mutable ref on the job that holds the fly deploy token. There is no advisory check on the dependency tree and nothing proposes updates.

Note: the `audit` job is **not** added to the deploy job's `needs`. A transitive advisory should not block shipping a fix to a watcher whose downtime is the real risk; it fails the check visibly instead.

- [ ] **Step 1: Pin the flyctl action**

In `.github/workflows/ci.yml` replace

```yaml
      - uses: superfly/flyctl-actions/setup-flyctl@master
```

with

```yaml
      # Pinned by SHA: this job holds FLY_API_TOKEN, so a mutable ref is a
      # supply-chain hole. SHA is tag 1.5 of superfly/flyctl-actions.
      - uses: superfly/flyctl-actions/setup-flyctl@fc53c09e1bc3be6f54706524e3b82c4f462f77be # 1.5
```

- [ ] **Step 2: Add an audit job**

Append to the `jobs:` map in `.github/workflows/ci.yml`, after the `image` job and before `deploy`:

```yaml
  audit:
    name: cargo audit
    runs-on: ubuntu-latest
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@v4
      # Informational: fails the check on a RUSTSEC advisory but does not gate
      # deploy, since a down watcher is worse than a theoretical advisory.
      - uses: rustsec/audit-check@69366f33c96575abad1ee0dba8212993eecbe998 # v2.0.0
        with:
          token: ${{ secrets.GITHUB_TOKEN }}
```

- [ ] **Step 3: Create `.github/dependabot.yml`**

```yaml
version: 2
updates:
  - package-ecosystem: cargo
    directory: /
    schedule:
      interval: weekly
    # rust-toolchain.toml pins 1.93; a bump that raises MSRV past it fails CI
    # loudly (see takecell 0.1.2 in CLAUDE.md), which is the point.
    open-pull-requests-limit: 5
  - package-ecosystem: github-actions
    directory: /
    schedule:
      interval: weekly
```

- [ ] **Step 4: Validate the YAML**

Run: `ruby -ryaml -e 'YAML.load_file(".github/workflows/ci.yml"); YAML.load_file(".github/dependabot.yml"); puts "ok"'`
Expected: `ok`.

- [ ] **Step 5: Commit**

```
git add .github/workflows/ci.yml .github/dependabot.yml
git commit -m "ci: pin flyctl action by SHA, add cargo audit and dependabot"
```

---

### Task 13: Cap the response body size

**Files:**
- Modify: `Cargo.toml` (reqwest `stream` feature, `futures-util`)
- Modify: `src/fetch.rs`

**Interfaces:**
- Produces: `pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;`, `FetchError::TooLarge(usize)`

**Why:** `resp.text()` buffers whatever the origin sends. The VM has 256 MB. The real pages are around 100 KB, so 2 MiB is generous.

- [ ] **Step 1: Write the failing test** (append inside `mod tests` in `src/fetch.rs`)

```rust
    #[tokio::test]
    async fn an_oversized_body_is_rejected_before_it_is_buffered() {
        let big = "a".repeat(MAX_BODY_BYTES + 1);
        let (_s, f) = respond_with(200, &big).await;
        assert_eq!(f.fetch().await.unwrap_err(), FetchError::TooLarge(MAX_BODY_BYTES));
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test an_oversized_body`
Expected: compile error, `MAX_BODY_BYTES` / `TooLarge` unknown.

- [ ] **Step 3: Add the dependency features**

In `Cargo.toml` change the reqwest line to:

```toml
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "stream"] }
```

and add under `[dependencies]`:

```toml
futures-util = { version = "0.3", default-features = false }
```

`futures-util` is already in `Cargo.lock` via reqwest, so this adds no new crate. Do **not** run `cargo update`.

- [ ] **Step 4: Implement in `src/fetch.rs`**

Add after the `use` lines:

```rust
use futures_util::StreamExt;

/// The real pages are ~100 KB. Anything far beyond that is not the page we
/// want, and buffering it unbounded on a 256 MB machine is how a watcher dies.
pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
```

Add the variant `TooLarge(usize),` to `enum FetchError` after `Io(String),` and a `Display` arm:

```rust
            FetchError::TooLarge(max) => write!(f, "response larger than {max} bytes"),
```

Replace the last line of `fetch`, `resp.text().await.map_err(|e| FetchError::Network(e.to_string()))`, with:

```rust
        if resp.content_length().is_some_and(|n| n > MAX_BODY_BYTES as u64) {
            return Err(FetchError::TooLarge(MAX_BODY_BYTES));
        }

        // Stream so the cap applies before the bytes land in memory, not after.
        let mut stream = resp.bytes_stream();
        let mut body: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| FetchError::Network(e.to_string()))?;
            if body.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(FetchError::TooLarge(MAX_BODY_BYTES));
            }
            body.extend_from_slice(&chunk);
        }
        // The site serves UTF-8; lossy decoding is acceptable for a page we only
        // scan for a marker string.
        Ok(String::from_utf8_lossy(&body).into_owned())
```

- [ ] **Step 5: Run the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: green, 82 tests. `a_200_returns_the_body` still passes.

- [ ] **Step 6: Commit**

```
git add Cargo.toml Cargo.lock src/fetch.rs
git commit -m "fix(fetch): cap response body at 2 MiB"
```

---

### Task 14: Truncate outgoing messages to the transport limits

**Files:**
- Modify: `src/notify.rs`

**Interfaces:**
- Produces: `pub const MAX_MESSAGE_BYTES: usize = 4000;`, `pub fn truncate_message(s: &str, max_bytes: usize) -> String`

**Why:** Telegram rejects messages over 4096 characters; ntfy's default body limit is 4096 bytes. On the fail-open path the body is raw card text, which is exactly the case where the size is unknown. Truncating to 4000 **bytes** at a char boundary satisfies both limits.

- [ ] **Step 1: Write the failing tests** (append inside `mod tests` in `src/notify.rs`)

```rust
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
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test messages_are`
Expected: compile error, `truncate_message` unknown. (Both new test names contain `messages_are`; cargo's filter is a plain substring match.)

- [ ] **Step 3: Implement**

Add near the top of `src/notify.rs`, after `ntfy_priority`:

```rust
/// Telegram rejects messages over 4096 chars and ntfy bodies over 4096 bytes.
/// Bytes ≥ chars, so one byte-based cap with headroom satisfies both.
pub const MAX_MESSAGE_BYTES: usize = 4000;

/// Cut `s` to at most `max_bytes` on a char boundary, marking the cut.
pub fn truncate_message(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let marker = "\n[…]";
    let mut end = max_bytes.saturating_sub(marker.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{marker}", &s[..end])
}
```

In `TelegramNotifier::send` replace `let text = format!("{}\n\n{}", alert.title, alert.body);` with
`let text = truncate_message(&format!("{}\n\n{}", alert.title, alert.body), MAX_MESSAGE_BYTES);`.

In `NtfyNotifier::send` replace `.body(format!("{}\n\n{}", alert.title, alert.body))` with
`.body(truncate_message(&format!("{}\n\n{}", alert.title, alert.body), MAX_MESSAGE_BYTES))`.

- [ ] **Step 4: Run the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: green, 84 tests.

- [ ] **Step 5: Commit**

```
git add src/notify.rs
git commit -m "fix(notify): truncate messages to the Telegram/ntfy limits"
```

---

### Task 15: Small cleanups

**Files:**
- Modify: `src/parse.rs` (remove dead `fingerprint`)
- Modify: `src/bot.rs` (imports instead of inline `crate::` paths)
- Modify: `src/main.rs` (stale comment)
- Modify: `src/watcher.rs` (sleep accounts for work time)

- [ ] **Step 1: Remove dead code**

In `src/parse.rs` delete the function `pub fn fingerprint(...)` together with its doc comment, and delete the two now-unused imports:

```rust
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
```

Run `grep -rn fingerprint src tests` and expect no output.

- [ ] **Step 2: Tidy `src/bot.rs` imports**

Add `use crate::state::listing_summary;` and `use crate::watcher::{AppState, REPEAT_CAP};` (replacing the existing `use crate::watcher::AppState;`). Then replace `crate::state::listing_summary(` with `listing_summary(` and `crate::watcher::REPEAT_CAP` with `REPEAT_CAP` in `status_text`.

- [ ] **Step 3: Fix the stale comment in `src/main.rs`**

Replace the comment above `tokio::select!`:

```rust
    // Neither task should ever finish: run_watcher loops forever and the bot
    // dispatcher runs until shutdown. If one does return, the process MUST exit
    // non-zero — fly's default restart policy is "on-failure", so returning
    // Ok(()) here would look like a clean shutdown and the machine would never
    // be restarted, leaving the watcher silently dead.
```

with:

```rust
    // Neither task should ever finish: run_watcher loops forever and the bot
    // dispatcher runs until shutdown. If one does return, the process MUST exit
    // non-zero. fly.toml sets the restart policy to "always" as a belt, but the
    // default is "on-failure", and a clean Ok(()) exit previously left a dead
    // watcher looking like a deliberate shutdown (CLAUDE.md invariant 5).
```

- [ ] **Step 4: Make the sleep account for work time**

In `run_watcher`, add `let started = std::time::Instant::now();` as the first statement inside `loop {`, and replace `tokio::time::sleep(wait).await;` with:

```rust
        // Sleep for the remainder of the interval, so a slow fetch or send does
        // not stretch the effective poll period.
        tokio::time::sleep(wait.saturating_sub(started.elapsed())).await;
```

- [ ] **Step 5: Run the gates**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: green, 84 tests.

- [ ] **Step 6: Commit**

```
git add src/parse.rs src/bot.rs src/main.rs src/watcher.rs
git commit -m "chore: remove dead fingerprint, tidy imports, fix stale comment, sleep for the remainder"
```

---

### Task 16: Documentation

**Files:**
- Modify: `CLAUDE.md`
- Modify: `README.md`

- [ ] **Step 1: Get the real test count**

Run: `cargo test 2>&1 | grep "test result" | awk '{s+=$4} END {print s}'`
Use the printed number below wherever `<N>` appears.

- [ ] **Step 2: Update `CLAUDE.md`**

In the **Invariants** list, append:

```
7. **Bot commands are answered only from the configured chat.** `AllowedChat` in `bot.rs` gates
   `/status` and `/ack`; discovery mode (`SB_WATCHER_DISCOVERY=1`) is the only time every chat is
   answered. A stranger must never be able to silence reminders.

8. **State updates are pure; sending happens outside the lock.** `apply_*` and `poll_once` in
   `watcher.rs` return the alerts to send. `run_watcher` sends them after releasing `shared`, then
   records delivery with `record_delivery`. Do not put network calls back inside the lock.

9. **"Delivered" means at least one channel accepted the message.** `MultiNotifier` returns `Err`
   only when every channel failed. An undelivered Max alert is retried on the next poll.
```

In the **Commands** block, change `# 57 tests, fully offline` to `# <N> tests, fully offline`.

In the **Layout** table, change the `src/watcher.rs` row to:

```
| `src/watcher.rs` | Pure poll fold (`poll_once`), backoff, per-condition reminders with retry, heartbeat, startup message. `run_watcher` is the only I/O. |
```

- [ ] **Step 3: Update `README.md`**

Change `cargo test                                        # 57 tests, no network` to use `<N>`.

In the **What it does** area near the top (the paragraph mentioning the heartbeat), add one sentence: "It also sends a message at every start, so a crash loop is visible even between heartbeats."

- [ ] **Step 4: Commit**

```
git add CLAUDE.md README.md
git commit -m "docs: record the new invariants and test count"
```

---

## Self-review notes

- **Coverage:** every row of the review summary table maps to a task. The mutex finding (row 1) and the untestable loop are both closed by Task 1; the "Info alert delays reminder" sub-finding is closed by Task 9's `fired` gate rather than by Task 8.
- **Type consistency:** `apply_structure_error` gains its fourth `what: &str` argument in Task 11 only; Tasks 1 to 10 use the three-argument form. `next_backoff` gains `blocked: bool` in Task 3; Task 1's tests use the two-argument form and Task 3 updates them. `AppState.repeat` exists through Task 8 and becomes `repeats` in Task 9; `bot.rs` is updated in the same task.
- **Test counts** are the expected running totals (57 → 61 → 62 → 63 → 64 → 65 → 68 → 69 → 70 → 75 → 77 → 81 → 82 → 84 → 84). If a count differs by one, check whether a test was accidentally left out rather than adjusting the number.
- **Not in this plan, deliberately:** pinning the `cargo-chef` base image (the toolchain file already controls the compiler), running channels concurrently in `MultiNotifier` (sequential is fine now that both clients have timeouts), and switching the fixture read to `tokio::fs` (rehearsal-only path).
