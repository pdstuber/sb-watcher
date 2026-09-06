# sb-watcher

Watches the **Ticketbörse** (resale exchange) on the sold-out
[SUMMER BREEZE 2027 ticket page](https://www.sbtix.de/catalog/tickets/98430-tickets-summer-breeze-2027-summer-breeze-open-air-dinkelsbuehl-am-18-08-2027)
and alerts within ~60 seconds when tickets appear.

Returned tickets for a sold-out festival get bought within minutes, and returns can be posted at any
hour. So this runs 24/7 on fly.io, alerts over Telegram plus an optional ntfy fallback, keeps
reminding for 30 minutes, and sends a daily heartbeat so silence is never ambiguous.
It also sends a message at every start, so a crash loop is visible even between heartbeats.

## How detection works, and why it looks backwards

The detector does **not** look for a "tickets available" pattern. It defines one known-quiet state —
the Ticketbörse card is present, contains
`Es gibt aktuell keine Tickets zum Weiterverkauf`, and is otherwise unchanged — and alerts on **any**
departure from it.

That inversion is deliberate. sbtix.de has exactly one ticket product, so its populated state has
never been observed. A positive-pattern detector **fails closed**: if the real markup differs from
what was guessed, it reports "all quiet" and you learn nothing. This **fails open**. Missing a real
drop loses the tickets; a false alarm costs ten seconds.

Real populated markup *was* recovered from two sibling Tickettoaster shops that had live resale stock
(fatoni.shop with 10 tickets, berq-shop.de with 1), and both are committed as test fixtures. That
markup is used for **enrichment only** — parsing `li[id^="voucher_swap_"]` to report ticket count and
prices — and never gates the alert. An alert saying *"3 tickets available, 45,20 € each"* tells you
whether it is worth racing to the checkout; one that never fires tells you nothing.

If the Ticketbörse card or the main product frame cannot be found, that raises a warning rather than
being treated as quiet, because a site redesign would otherwise blind the watcher permanently while
it looked healthy.

## Configuration

| Variable | Required | Default | Purpose |
|---|---|---|---|
| `TELOXIDE_TOKEN` | yes | — | Bot token from [@BotFather](https://t.me/BotFather) |
| `TELEGRAM_CHAT_ID` | yes* | — | *Not needed when `SB_WATCHER_DISCOVERY=1` (below) |
| `SB_WATCHER_DISCOVERY` | no | — | `1` ⇒ discovery mode: bot only, no watching; replies with chat ids |
| `NTFY_TOPIC` | no | — | Unset ⇒ ntfy channel disabled |
| `POLL_INTERVAL_SECS` | no | `60` | |
| `TARGET_URL` | no | the SB 2027 page | |
| `USER_AGENT` | no | a browser UA | |
| `SB_WATCHER_FIXTURE_PATH` | no | — | Read HTML from a local file instead of the site |

## Setup

1. Create a bot with [@BotFather](https://t.me/BotFather) and copy the token.
2. Find your chat id — run in discovery mode and message the bot; it replies with the id:
   ```fish
   set -x TELOXIDE_TOKEN "123456:ABC..."
   set -x SB_WATCHER_DISCOVERY 1
   cargo run
   ```
3. Make the alert loud enough to wake you. **On iPhone** this means Telegram, not ntfy:
   - iOS Settings → Focus → Do Not Disturb → **Allowed Apps** → add Telegram.
   - In Telegram, open the bot chat → Notifications → set a distinct, loud custom sound.

   The 5-minute repeat (six times, 30 minutes) is the real safety net here — a single push is easy
   to sleep through, six spread over half an hour is not.

4. Optional, and **only worth it on Android**: set `NTFY_TOPIC` to an unguessable string, install the
   [ntfy app](https://ntfy.sh/), subscribe, and set the topic to max priority. On Android that
   genuinely bypasses Do Not Disturb. **On iOS it does not** — the ntfy app has no Critical Alerts
   entitlement, so a max-priority ntfy push is no louder than a Telegram one, and it only adds
   redundancy against a Telegram outage.

   Note that **ntfy topics on the public server are unauthenticated**: anyone who knows the name can
   read your alerts *and* publish fake ones. Use a long random string, never `sb-watcher`.
   Leaving `NTFY_TOPIC` unset disables the channel entirely.

## Telegram commands

| Command | Effect |
|---|---|
| `/status` | What the watcher currently sees, check count, uptime, last change |
| `/ack` | Stop repeating the current alert |
| `/help` | Command list |

`/check` (force an immediate poll) was deliberately cut: it needs a wakeup channel into the loop and
saves at most 59 seconds. `/status` answers the question it was really for.

## Alerts

| Trigger | Severity |
|---|---|
| Resale went from empty to anything else | **max**, repeats every 5 min, capped at 6 |
| Main product no longer shows *Ausverkauft* | **max**, repeats |
| Resale stock gone / main sold out again | info, once |
| Ticketbörse wording changed while still empty | info, once |
| Ticketbörse card or main product frame not found — watcher may be blind | info, ≤1×/24h |
| Site unreachable for 15 min, or HTTP 403/429 | info, once per episode |
| Still running | info, every 24h |

The failure warning is time-based rather than attempt-based on purpose: under exponential backoff,
ten consecutive failures take over an hour, so counting attempts could leave the watcher silently
blind through an entire drop.

## Rehearsing an alert

The live site cannot be made to show resale stock on demand, so `SB_WATCHER_FIXTURE_PATH` reads local
HTML instead. This is the only way to prove the alert path works *before* the night it matters — run
it before trusting the deployment:

```fish
set -x TELOXIDE_TOKEN "..."; set -x TELEGRAM_CHAT_ID "..."
set -x SB_WATCHER_FIXTURE_PATH tests/fixtures/real_available_many.html
set -x POLL_INTERVAL_SECS 10
cargo run
```

Expect `10 tickets available` with `45,20 €` prices — real markup from a shop that genuinely had ten
on offer. Then try:

- `tests/fixtures/available_blob.html` — unrecognized markup still alerts (the fail-open path, and
  the most likely shape of a real sbtix drop, since its populated state is unobserved)
- `tests/fixtures/no_card.html` — raises the "page structure changed" warning

## Tests

```fish
cargo test                                        # 89 tests, no network
cargo test --test live_site -- --ignored --nocapture   # checks the real page
```

The live test is worth running occasionally on its own: if the committed fixture drifts from the live
page, it fails while the fixture tests still pass.

## Deploy

```fish
fly launch --no-deploy --copy-config --name sb-watcher --region fra
fly secrets set TELOXIDE_TOKEN=... TELEGRAM_CHAT_ID=... NTFY_TOPIC=...
fly deploy
fly logs
```

`fly.toml` has no `[http_service]` and no `PORT` — Telegram long polling needs no inbound port, and
`auto_stop_machines` applies only to services, so omitting the service is what keeps this running
24/7. `[[restart]] policy = 'always'` is set deliberately: fly's default is `on-failure`, which
would not restart a process that exited zero. `main` also exits non-zero if any long-running task
returns, so a dead watcher can never look like a clean shutdown.

## CI

`.github/workflows/ci.yml` runs `fmt`, `clippy`, the test suite and a Docker build on every push
and PR, then deploys to fly from `main` only. It needs exactly one secret, `FLY_API_TOKEN`
(`fly tokens create deploy`). The bot token is deliberately **not** a GitHub secret — it lives in
`fly secrets`, and CI never talks to Telegram.

The runtime image is `FROM scratch` (~4 MB). That works because TLS roots come from `webpki-roots`
compiled into the binary rather than a system `ca-certificates` package, and because the rustls
crypto provider is `ring`, which builds against musl cleanly. The build derives its musl target from
the builder's own architecture, so it works on fly's x86_64 builder and on an arm64 Mac alike.

**A scratch image has no shell**, so `fly ssh console` will not give you a prompt inside the
container. `fly logs` is the debugging channel.
