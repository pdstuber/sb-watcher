# sb-watcher — design

**Date:** 2026-09-06
**Status:** approved for implementation

## Problem

SUMMER BREEZE Open Air 2027 is sold out. The only remaining route to two tickets is the shop's
**Ticketbörse** (resale exchange) on the event's product page:

```
https://www.sbtix.de/catalog/tickets/98430-tickets-summer-breeze-2027-summer-breeze-open-air-dinkelsbuehl-am-18-08-2027
```

Today it reads *"Es gibt aktuell keine Tickets zum Weiterverkauf."*

Returned tickets for a sold-out festival are bought within minutes, and returns can be posted at
any hour of the day or night. So the watcher must be always-on, notice within about a minute, and
be loud enough to wake someone.

The subtler requirement: **it must not fail silently.** A watcher that died three weeks ago
produces exactly the same user experience as a watcher reporting "no tickets yet". Every design
decision below that looks like over-engineering is really an answer to that one failure mode.

## Goal

A Telegram bot, backed by a louder ntfy fallback, that alerts within ~60s of resale stock
appearing, keeps reminding for 30 minutes, and periodically proves it is still alive.

Non-goal: buying. No auto-add-to-cart, no auto-checkout, no account login. The alert carries the
Ticketbörse text and a direct link so a human can judge quantity and price and click through.

## Reconnaissance

Verified by direct HTTP probing of the live site on 2026-09-06:

- A plain `GET` returns the complete page (~64 KB). The shop is a server-rendered Rails/Turbo
  application; the platform is **Tickettoaster**. **No JavaScript, no login, no cookies are
  required** — therefore no headless browser, no Playwright, no session handling.
- `robots.txt` permits `/catalog/tickets/*`. It disallows only `/admin`, `/agb`, `/datenschutz`
  and three 2023 products.
- **There is no API.** `…/98430.json` returns `204 No Content`. The page's schema.org `Event`
  JSON-LD carries no `offers` field, so no structured availability flag exists. HTML parsing is
  the only route.
- The Ticketbörse block is rendered **inline** inside `<turbo-frame id="ticket_detail">`, not
  lazily loaded, so a single request retrieves everything.
- The empty state, verbatim, lives in a `div.card` whose `.card-header h2` reads `Ticketbörse`:

  ```html
  <div class="alert alert-info mb-0 p-2">
    <span class="float-start me-1"><i class="fas fa-info-circle fa-fw fa-lg"></i></span>Es gibt aktuell keine Tickets zum Weiterverkauf.</div>
  ```

- Separately, the main product renders `<div class="alert alert-danger">Ausverkauft</div>` in the
  `Tickets auswählen` section.

- **The card's extracted text is byte-stable across requests.** Three fetches two seconds apart
  produced an identical sha256 of the normalized card text. This was checked specifically because
  the `resale_hash` change-detector below would be unusable — firing on every single poll — if the
  card carried a CSRF token, session id or timestamp. It does not.

The captured page is committed as `tests/fixtures/empty_resale.html`.

## The constraint that shapes everything

sbtix.de has exactly **one** ticket product site-wide. Six sibling Tickettoaster shops were
scanned for a Ticketbörse holding stock; none had one.

**The "tickets available" markup cannot be observed.** We do not know, and cannot find out, what
the page looks like when resale tickets exist.

A detector written against a guessed "available" pattern would be a detector we cannot test and
have no reason to trust. So the design inverts the question. It defines exactly one
**known-quiet state**:

> the Ticketbörse card is present, contains the empty-state sentence, and is otherwise unchanged
> from the last observation

and treats **any** departure from it as alert-worthy. The asymmetry justifies this: missing a real
drop loses the tickets, while a false alarm costs ten seconds of attention. Alerts are labelled by
severity so the two remain distinguishable.

## Decisions

| Decision | Choice | Rationale |
|---|---|---|
| Host | fly.io, region `fra` | Survives home power, router and ISP failure — the failure modes most likely to coincide with a drop. ~$2/mo, always-on shared-cpu-1x 256 MB. |
| Language | Rust — teloxide, reqwest, scraper | Reuses the cargo-chef `Dockerfile` and `fly.toml` shape already proven in `../rust-telegram-bot-skeleton`. |
| Poll interval | 60s, day and night | ~1,440 requests/day: light for a single-tenant shop, worst-case one-minute latency. Returns can be posted at 3am, so no night throttle. |
| Telegram transport | Long polling (`getUpdates`) | Simplification over the skeleton's webhook setup: no public URL, no axum, no inbound port. Runs identically on a laptop, a Pi, or fly. |
| Recipients | One chat ID, direct message | Plus `NTFY_TOPIC` as the second channel. |
| Alert repeat | Every 5 min, capped at 6 (30 min); `/ack` stops early | Guards against a muted phone without pinging forever if unreachable. |
| Watch scope | Ticketbörse **and** the main "Ausverkauft" banner | Extra contingents are sometimes released through the normal shop rather than resale. |
| State persistence | In-memory only; no fly volume | A restart re-baselines. If it restarts while stock is up it simply re-alerts — the safe direction. A volume buys nothing here. |

## Architecture

One binary, one always-on machine, two concurrent tokio tasks over shared state.

```
  watcher task (60s loop)          bot task (long-poll dispatcher)
  fetch → classify → diff          /status  /ack  /check  /help
        → notify                          reads / updates
             \                            /
              \--- Arc<Mutex<AppState>> -/
                          |
                    MultiNotifier
                     /          \
              Telegram          ntfy.sh
```

The decision logic is kept **pure and I/O-free**, which is where the testability comes from: the
two modules that decide whether to wake you up at 3am (`parse`, `state`) touch neither network nor
clock-dependent global state, so they are exhaustively unit-testable against fixtures.

### Modules

| Module | Responsibility | Depends on |
|---|---|---|
| `main.rs` | Wiring: load config, build notifier, spawn both tasks, graceful shutdown | all |
| `config.rs` | Env parsing; fail fast at startup with actionable messages | — |
| `fetch.rs` | HTTP client, timeouts, fixture injection | `config` |
| `parse.rs` | **Pure.** `classify(html) -> Result<PageObservation, ParseError>` | — |
| `state.rs` | **Pure.** `PageObservation`, `AppState`, `transitions(prev, cur) -> Vec<Alert>` | `parse` |
| `notify.rs` | `Notifier` trait; Telegram, ntfy, multi, and a recording fake | — |
| `watcher.rs` | The poll loop, repeat bookkeeping, backoff | all above |
| `bot.rs` | teloxide command handlers reading shared state | `state` |

### Configuration

| Variable | Required | Default | Purpose |
|---|---|---|---|
| `TELOXIDE_TOKEN` | yes | — | Bot token from @BotFather |
| `TELEGRAM_CHAT_ID` | no | — | Unset ⇒ discovery mode (see below) |
| `NTFY_TOPIC` | no | — | Unset ⇒ ntfy channel disabled |
| `POLL_INTERVAL_SECS` | no | `60` | |
| `TARGET_URL` | no | the 98430 URL | |
| `USER_AGENT` | no | browser-like string | The verified-working UA |
| `SB_WATCHER_FIXTURE_PATH` | no | — | Read HTML from a file instead of HTTP (rehearsal) |

### `parse.rs` — the core

`scraper` has no `:contains()`, so the card is located structurally rather than by text position:
iterate `div.card`, and for each, check whether its `.card-header h2` text trims to `Ticketbörse`.
This survives the card moving on the page or gaining sibling cards.

```rust
enum ResaleState { Empty, Available }

struct PageObservation {
    resale: ResaleState,
    resale_text: String,   // normalized (whitespace-collapsed) inner text of the card
    resale_hash: String,   // sha256 of resale_text
    main_sold_out: bool,   // "Ausverkauft" present in the ticket_detail section
}
```

- `Empty` iff the normalized card text contains `Es gibt aktuell keine Tickets zum Weiterverkauf`.
- `Available` otherwise — that is, on *anything else at all*.
- `ParseError::CardNotFound` if the card cannot be located.

`CardNotFound` must **never** be swallowed. A site redesign would otherwise blind the watcher
permanently while it continued to report all quiet. It raises a warning alert, rate-limited to
once per 24h until the structure recovers.

### `state.rs` — transitions

| Condition | Message | Severity |
|---|---|---|
| resale `Empty` → `Available` | 🎟️ TICKETS AVAILABLE + link + card text | **max**, repeats |
| main sold out → not sold out | 🎟️ MAIN SHOP NO LONGER SOLD OUT + link | **max**, repeats |
| resale `Available` → `Empty` | resale stock gone | info, once |
| still `Empty`, `resale_hash` changed | ⚠️ Ticketbörse wording changed (old → new) | info, once |
| `CardNotFound` | ⚠️ page structure changed — watcher may be blind | info, ≤1×/24h |
| fetch failing continuously for 15 min, or 403/429 | ⚠️ cannot reach site / possibly blocked | info, once per episode |
| every 24h | ✅ still watching; N checks; last change at T | info |

**First-run rule.** With no previous observation the run establishes a baseline silently — *except*
that if the very first observation is already `Available`, it alerts immediately. Swallowing that
case would mean a restart during a live drop loses the drop.

**Repeat with cap.** A max-severity alert sets `AlertRepeat { fired_at, count }`. Each later poll
where the condition still holds, at least 5 minutes have passed, and `count < 6`, re-sends.
Cleared by `/ack` or by the condition resolving.

**Backoff.** On fetch error the interval doubles from the poll interval — 60, 120, 240, 480 —
capped at 600s, and resets to the poll interval on the first success. On 403/429 it starts at 300s;
being blocked means being blind, so it also raises the warning above.

The failure warning is deliberately **time-based, not attempt-based**. Under backoff, ten
consecutive failures would take well over an hour, so a count-based threshold would leave the
watcher silently blind through an entire ticket drop. Fifteen minutes of continuous failure
triggers it regardless of how few attempts fit into that window.

The 24h heartbeat is the dead-man's switch. Without it, silence is ambiguous between "no tickets"
and "the process died".

### `bot.rs` — commands

`/status` (current state, last change, uptime, check count, next poll), `/ack` (silence the
repeat), `/check` (force an immediate poll), `/help`.

When `TELEGRAM_CHAT_ID` is unset the bot runs in **discovery mode** and replies to any message
with that chat's ID. This removes the third-party-bot step from setup: create the bot, run it,
message it, read your ID off the reply.

## Testing

Implementation follows TDD. The pure layers come first and run without network or secrets.

**Fixtures.** The real captured page is the golden negative, `tests/fixtures/empty_resale.html`.

Because the positive markup is unobservable, positive fixtures are **synthesized** by editing that
real page — replacing the `alert-info` div with (a) a plausible listing table, (b) an empty div,
(c) nothing at all. These deliberately do not attempt to guess the real markup. They assert the
property that makes the design sound:

> anything other than the known empty sentence classifies as `Available`

Three structurally different "somethings" all landing on `Available` is what demonstrates the
detector is not pattern-matching a guess.

Further fixtures: the card removed entirely → `CardNotFound`; `alert-danger Ausverkauft` present
and absent → `main_sold_out` both ways.

Transition, repeat and cap logic is tested against the recording fake notifier — no network, and
injected timestamps rather than wall-clock sleeps.

## Verification

1. `cargo test` — parse and transition suites green.
2. `cargo run` against the live URL with `POLL_INTERVAL_SECS=10`; confirm the startup message
   arrives and `/status` responds.
3. **The rehearsal that matters.** Set `SB_WATCHER_FIXTURE_PATH` to a synthesized *available*
   fixture and confirm the real 🎟️ alert reaches the phone over **both** Telegram and ntfy, that
   the 5-minute repeat fires, that it stops after 6, and that `/ack` silences it early.
   Since the live site cannot produce this state on demand, this is the only way to prove the
   alert path works before the moment it has to.
4. `fly deploy`; `fly logs` shows the poll loop; `/status` answers from the deployed machine.
5. Confirm the 24h heartbeat the following day.

## Deployment

- `fly.toml`: app `sb-watcher`, `primary_region = 'fra'`, `[[vm]] memory = '256mb'`, 1 shared cpu.
  **No `[http_service]` and no `PORT`** — long polling needs no inbound port — and no
  `auto_stop_machines`, so the machine stays up.
- `Dockerfile`: the skeleton's cargo-chef multi-stage, binary renamed `sb-watcher`. Using
  `reqwest` with `rustls-tls` and `default-features = false` drops the OpenSSL dependency, so the
  runtime layer needs only `ca-certificates`.
- Secrets: `fly secrets set TELOXIDE_TOKEN=… TELEGRAM_CHAT_ID=… NTFY_TOPIC=…`

## Manual setup

1. @BotFather → create bot → token.
2. Run once without `TELEGRAM_CHAT_ID`, message the bot, read the chat ID off its reply.
3. Choose an unguessable ntfy topic — **topics are public to anyone who knows the name** — install
   the ntfy app, subscribe, and set that topic to max priority so it bypasses Do Not Disturb.
4. Give the Telegram chat a custom loud sound and exempt it from Do Not Disturb.
