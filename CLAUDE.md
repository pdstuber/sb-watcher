# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What this is

A watcher for the resale exchange ("Ticketbörse") on one sold-out SUMMER BREEZE 2027 ticket page.
It polls every 60s and alerts over Telegram. It runs 24/7 on fly.io as app `sb-watcher`.

The user needs **two tickets**. Returned tickets sell within minutes and can appear at any hour, so
latency and never-failing-silently matter far more than elegance.

## Invariants — do not break these

These encode decisions that were expensive to reach. Changing them needs an explicit decision from
the user, not a refactor.

1. **Detection is inverted, and must stay inverted.** `resale` is `Empty` if and only if the card
   text contains `EMPTY_MARKER`; **anything else is `Available`**. Never add a positive pattern that
   gates the alert. A positive matcher **fails closed** — unfamiliar markup would read as "all
   quiet" and the drop would be missed silently. The inverted rule **fails open**.

2. **`listings` is enrichment, never a gate.** `voucher_swap` parsing supplies ticket count and
   prices for the message body. An `Available` observation with zero parsed listings must still
   fire a max-severity alert. There is a test named
   `alert_still_fires_when_no_listings_could_be_parsed` guarding exactly this.

3. **`ParseError::CardNotFound` must never be treated as quiet.** It raises a warning (rate-limited
   to once per 24h). Silently swallowing it would leave the watcher blind after a site redesign
   while continuing to look healthy.

4. **The card is located by `.card-header` *text*, not by `.card-header h2`.** fatoni.shop renders
   that heading as a plain `<div>`. `tests/fixtures/real_available_one.html` is the regression test.

5. **`main` must exit non-zero if either task returns.** fly's restart policy is `always`, but
   returning `Ok(())` on task death previously made a dead watcher look like a clean shutdown.

6. **The `live_site` test stays `#[ignore]d`.** CI must not depend on sbtix.de being up, and must
   not add traffic to a small shop on every push.

## Layout

| File | Responsibility |
|---|---|
| `src/parse.rs` | **Pure.** HTML → `PageObservation`. The core; most tests live here. |
| `src/state.rs` | **Pure.** Previous vs current observation → `Vec<Alert>`. |
| `src/watcher.rs` | Poll loop, backoff, capped repeats, heartbeat, structure warning. |
| `src/notify.rs` | `Notifier` trait, Telegram, ntfy, fan-out, and the recording fake. |
| `src/fetch.rs` | HTTP client, timeouts, and the fixture-injection escape hatch. |
| `src/bot.rs` | Telegram commands and chat-id discovery. |
| `src/config.rs` | Env parsing. `from_map` is pure so tests never touch process env. |

`parse.rs` and `state.rs` are deliberately I/O-free and clock-free. Keep decision logic there; keep
network and time in `watcher.rs`. That separation is what makes the alerting exhaustively testable.

## Fixtures

**Never edit these** — they are captured real pages, and their value is being unaltered:

- `empty_resale.html` — the live sbtix page (the actual target)
- `real_available_many.html` — fatoni.shop, 10 real offers at 45,20 €
- `real_available_one.html` — berq-shop.de, 1 offer, plain-`div` heading

The others (`available_blob`, `available_no_alert`, `no_card`, `main_on_sale`) are synthesized from
`empty_resale.html`; the generator is in the plan doc. `available_blob.html` is the fail-open test
and must not be deleted as redundant — sbtix's own populated markup has never been observed, so
unfamiliar markup is the likeliest real case.

## Commands

```fish
cargo test                                              # 57 tests, fully offline
cargo test --test live_site -- --ignored --nocapture    # hits the real page
cargo clippy --all-targets -- -D warnings
cargo fmt --all
fly logs --app sb-watcher
fly deploy --remote-only --app sb-watcher
```

To exercise the alert path without waiting for a real drop:

```fish
./scripts/with-env.sh env SB_WATCHER_FIXTURE_PATH=tests/fixtures/real_available_many.html \
  POLL_INTERVAL_SECS=10 ./target/debug/sb-watcher
```

## Secrets

`.env` is gitignored and holds `TELOXIDE_TOKEN` and `TELEGRAM_CHAT_ID` for local runs; source it via
`scripts/with-env.sh`, which keeps values out of shell history. **Never print these**, and never
inline them into a command — read them through the script or `bash -c '. ./.env; …'`.

In production they live in `fly secrets`. `FLY_API_TOKEN` is a GitHub Actions secret for the deploy
job. The bot token must **not** be added to GitHub — CI never talks to Telegram.

## Gotchas that have already cost time

- **`rust-toolchain.toml` must be copied into the Docker image before `rustup target add`.**
  Otherwise the musl target lands on the default toolchain, the pin switches away from it, and the
  build dies with `can't find crate for std`.
- **reqwest is pinned to 0.12, not 0.13**, to match teloxide 0.17's `^0.12.7` and avoid compiling
  reqwest twice. The rustls feature is `rustls-tls` in 0.12 and `rustls` in 0.13 — do not "upgrade"
  without checking.
- **`scratch` works only because** TLS roots come from `webpki-roots` (compiled in, no
  `ca-certificates` needed) and the rustls provider is `ring` (musl-friendly, unlike `aws-lc-rs`).
  Changing either can silently break the image.
- **The scratch image has no shell** — `fly ssh console` gives no prompt. Use `fly logs`.
- **`takecell` is pinned to 0.1.1**; 0.1.2 requires rustc 1.96 and the pin is 1.93.

## Design docs

`docs/superpowers/specs/2026-09-06-sb-watcher-design.md` explains *why* the detector is inverted and
records the reconnaissance (no API, no JS, robots.txt permits it, marker string is platform
boilerplate). Read it before changing detection behaviour.
