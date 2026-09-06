# AGENTS.md

Guidance for coding agents working in this repository. `CLAUDE.md` is the Claude Code equivalent
and carries the same content; **if you change one, mirror the change in the other.**

## What this is

A watcher for the resale exchange ("Ticketbörse") on one sold-out SUMMER BREEZE 2027 ticket page.
It polls every 60s and alerts over Telegram, plus an optional ntfy channel. It runs 24/7 on
fly.io as app `sb-watcher`.

The owner needs **two tickets**. Returned tickets sell within minutes and can appear at any hour.

**The prime directive: this program must never fail silently.** A watcher that is broken but looks
healthy is worse than one that crashes, because the owner will not know to check. Latency and
never-failing-silently matter far more than elegance. When you face a design choice here, pick the
option that fails loudly.

## Invariants — do not break these

These encode decisions that were expensive to reach. Changing them needs an explicit decision from
the owner, not a refactor.

1. **Detection is inverted, and must stay inverted.** `resale` is `Empty` if and only if the card
   text contains `EMPTY_MARKER`; **anything else is `Available`**. Never add a positive pattern
   that gates the alert. A positive matcher **fails closed** — unfamiliar markup would read as
   "all quiet" and the drop would be missed silently. The inverted rule **fails open**.

2. **`listings` is enrichment, never a gate.** `voucher_swap` parsing supplies ticket count and
   prices for the message body. An `Available` observation with zero parsed listings must still
   fire a max-severity alert. There is a test named
   `alert_still_fires_when_no_listings_could_be_parsed` guarding exactly this.

3. **`ParseError::CardNotFound` must never be treated as quiet.** It raises a warning, rate-limited
   to once per 24h. Silently swallowing it would leave the watcher blind after a site redesign
   while continuing to look healthy.

4. **The card is located by `.card-header` *text*, not by `.card-header h2`.** fatoni.shop renders
   that heading as a plain `<div>`. `tests/fixtures/real_available_one.html` is the regression test.

5. **`main` must exit non-zero if any long-running task returns, including the bot dispatcher in
   discovery mode.** fly's restart policy is `always`, but returning `Ok(())` on task death
   previously made a dead watcher look like a clean shutdown.

6. **The `live_site` test stays `#[ignore]d`.** CI must not depend on sbtix.de being up, and must
   not add traffic to a small shop on every push.

7. **Bot commands are answered only from the configured chat.** `AllowedChat` in `bot.rs` gates
   `/status` and `/ack`; discovery mode (`SB_WATCHER_DISCOVERY=1`) is the only time every chat is
   answered. A stranger must never be able to silence reminders.

8. **State updates are pure; sending happens outside the lock.** `apply_*` and `poll_once` in
   `watcher.rs` return the alerts to send. `run_watcher` sends them after releasing `shared`, then
   records delivery with `record_delivery`. Do not put network calls back inside the lock.

9. **"Delivered" means at least one channel accepted the message.** `MultiNotifier` returns `Err`
   when every channel failed or no channels exist. An undelivered Max alert is retried on the next
   poll, even if that poll cannot produce a new observation.

## Layout

| File | Responsibility |
|---|---|
| `src/parse.rs` | **Pure.** HTML → `PageObservation`. The core; most tests live here. |
| `src/state.rs` | **Pure.** Previous vs current observation → `Vec<Alert>`. |
| `src/watcher.rs` | Pure poll fold (`poll_once`), backoff, per-condition reminders with retry, heartbeat, startup message. `run_watcher` is the only I/O. |
| `src/notify.rs` | `Notifier` trait, Telegram, ntfy, fan-out, and the recording fake. |
| `src/fetch.rs` | HTTP client, timeouts, and the fixture-injection escape hatch. |
| `src/bot.rs` | Telegram commands and chat-id discovery. |
| `src/config.rs` | Env parsing. `from_map` is pure so tests never touch process env. |

`parse.rs` and `state.rs` are deliberately I/O-free and clock-free. Keep decision logic there;
keep network and time in `watcher.rs`. That separation is what makes the alerting exhaustively
testable. Preserve it.

## How to work here

This is the quality bar. It is not optional, and it is the reason the test suite is worth trusting.

- **Test first.** Write the failing test, run it and watch it fail for the reason you expect, then
  write the minimal code to pass it, then run it again. A test you never saw fail is not evidence
  of anything.
- **Never claim something passes without running it.** Paste or summarise the actual output. If
  tests fail, say so plainly with the output. If you skipped a step, say that.
- **Run the full gate before every commit:**
  ```
  cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
  ```
  All three must pass. Clippy warnings are errors here.
- **One logical change per commit,** with a conventional-prefix message (`fix:`, `feat:`,
  `refactor:`, `test:`, `chore:`, `ci:`, `docs:`). Commit and push frequently rather than
  accumulating a large diff.
- **Assertions carry a message when they could fail ambiguously.** Look at the existing tests: most
  `assert!` calls include a formatted message showing the actual value. Match that style.
- **Test names are sentences describing the behaviour,** not the function under test. Existing
  examples: `a_missing_card_is_an_error_not_a_quiet_empty`,
  `transient_failures_are_not_reported_but_a_sustained_outage_is`,
  `being_blocked_warns_at_once_without_waiting`. Keep writing them that way.
- **Comments explain *why*, not *what*.** The existing comments record decisions that cost time to
  reach. Do not delete them while refactoring; they are the institutional memory of this repo.
- **No placeholders.** No `TODO`, no "handle edge cases later", no stub that returns a fixed value
  to make a test pass artificially.
- **Prefer exhaustive `match` over a catch-all** when the arms are a closed set of alert kinds or
  states. A wildcard lets a future variant slip through silently, which violates the prime
  directive. Two functions in `watcher.rs` are exhaustive on purpose for exactly this reason; do
  not "simplify" them back.
- **Stay in scope.** Fix what was asked. If you spot something else, report it rather than
  quietly expanding the diff.

## Commands

```fish
cargo test                                              # 89 tests, fully offline
cargo test --test live_site -- --ignored --nocapture    # hits the real page
cargo clippy --all-targets -- -D warnings
cargo fmt --all
fly logs --app sb-watcher
fly deploy --remote-only --app sb-watcher
```

The shell is **fish**. `export FOO=bar` is `set -gx FOO bar`, and a `FOO=bar cmd` prefix does not
work. Prefer `bash -c '...'` when a command genuinely needs bash syntax.

To exercise the alert path without waiting for a real drop:

```fish
./scripts/with-env.sh env SB_WATCHER_FIXTURE_PATH=tests/fixtures/real_available_many.html \
  POLL_INTERVAL_SECS=10 ./target/debug/sb-watcher
```

This is the only way to prove the alert path works *before* the night it matters. Expect ten
tickets at 45,20 €. Then try `available_blob.html` (unrecognised markup must still alert) and
`no_card.html` (must raise the structure warning).

## Fixtures

**Never edit these** — they are captured real pages, and their value is being unaltered:

- `empty_resale.html` — the live sbtix page (the actual target)
- `real_available_many.html` — fatoni.shop, 10 real offers at 45,20 €
- `real_available_one.html` — berq-shop.de, 1 offer, plain-`div` heading

The others (`available_blob`, `available_no_alert`, `no_card`, `main_on_sale`) are synthesized from
`empty_resale.html`. `available_blob.html` is the fail-open test and must not be deleted as
redundant — sbtix's own populated markup has never been observed, so unfamiliar markup is the
likeliest real case.

## Secrets

`.env` is gitignored and holds `TELOXIDE_TOKEN` and `TELEGRAM_CHAT_ID` for local runs; source it
via `scripts/with-env.sh`, which keeps values out of shell history. **Never print these**, and
never inline them into a command — read them through the script or `bash -c '. ./.env; …'`.

In production they live in `fly secrets`. `FLY_API_TOKEN` is a GitHub Actions secret for the
deploy job. The bot token must **not** be added to GitHub — CI never talks to Telegram.

Every test in this repo is offline and uses dummy values: token `"123:ABC"`, chat id `"1"`. No
test should ever contact Telegram, ntfy, or sbtix.de.

## Gotchas that have already cost time

- **`rust-toolchain.toml` must be copied into the Docker image before `rustup target add`.**
  Otherwise the musl target lands on the default toolchain, the pin switches away from it, and the
  build dies with `can't find crate for std`.
- **The toolchain is pinned to 1.93.** Do not change it, and do not run a bare `cargo update`. A
  transitive crate raising its MSRV has broken this build before: `takecell` is pinned to 0.1.1
  because 0.1.2 requires rustc 1.96.
- **reqwest is pinned to 0.12, not 0.13**, to match teloxide 0.17's `^0.12.7` and avoid compiling
  reqwest twice. The rustls feature is `rustls-tls` in 0.12 and `rustls` in 0.13 — do not
  "upgrade" without checking.
- **`scratch` works only because** TLS roots come from `webpki-roots` (compiled in, no
  `ca-certificates` needed) and the rustls provider is `ring` (musl-friendly, unlike `aws-lc-rs`).
  Changing either can silently break the image.
- **The scratch image has no shell** — `fly ssh console` gives no prompt. Use `fly logs`.
- **Line limit is 120 columns**, set in `rustfmt.toml`.
- **Only ONE process may long-poll a bot token.** Telegram terminates the older `getUpdates`
  consumer, so running the binary locally while the fly machine is up makes the two instances
  repeatedly kill each other's listener, logging `Api(TerminatedByOtherGetUpdates)`. Sending is
  unaffected (alerts still arrive), but `/status` and `/ack` become unreliable. Before any local
  run that needs commands to work — the fixture rehearsal especially — stop the machine first:
  `fly machine stop <id> --app sb-watcher`, then start it again afterwards. Restarting is safe: the
  watcher re-baselines and re-alerts if stock is up, which is the safe direction.
- **To discover a group's chat id you do not need a local run at all.** The deployed bot logs every
  chat id it sees, so send a command in the group and read it out of `fly logs`. In a group,
  Telegram's privacy mode means only *commands* reach the bot, so send `/help`, not a plain
  message. The bot does not need to be a group admin, and should not be.

## Design docs

`docs/superpowers/specs/2026-09-06-sb-watcher-design.md` explains *why* the detector is inverted
and records the reconnaissance: no API, no JavaScript, `robots.txt` permits it, and the marker
string is platform boilerplate rather than shop-specific copy. Read it before changing detection
behaviour. It embeds a screenshot of the card in its empty state, which is the visual source of
the two detector constants.

## In-flight work

Branch `fix/review-2026-09-06` is executing a 16-task plan that fixes the findings of a full code
review. **If you are picking that up, read `docs/HANDOVER-2026-09-06.md` first** — it records
exactly which tasks are done, which is half-finished, the expected test count after each task, and
the judgement calls already made that should not be silently undone.

The plan itself, with the exact code for every remaining task, is
`docs/superpowers/plans/2026-09-06-review-fixes.md`. You should not need to design anything; if you
find yourself inventing an approach, re-read the task.
