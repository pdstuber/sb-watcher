# sb-watcher Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Rust daemon that polls the SUMMER BREEZE 2027 Ticketbörse every 60s and alerts via Telegram and ntfy within about a minute of resale stock appearing.

**Architecture:** One binary, two tokio tasks (a poll loop and a Telegram long-polling dispatcher) sharing `Arc<Mutex<AppState>>`. All decision logic lives in two pure, I/O-free modules (`parse`, `state`) so the code that decides whether to wake someone at 3am is exhaustively unit-testable against HTML fixtures. Network, Telegram and clock live behind thin adapters.

**Tech Stack:** Rust 1.93, teloxide 0.17 (long polling), reqwest 0.12 (rustls), scraper 0.27, tokio 1, chrono, anyhow, async-trait. Deployed as a single always-on fly.io machine in `fra`.

**Spec:** `docs/superpowers/specs/2026-09-06-sb-watcher-design.md`

## Global Constraints

- **Rust edition 2021**, toolchain 1.93 (installed and verified).
- **teloxide 0.17** with `default-features = false, features = ["macros", "ctrlc_handler", "rustls"]`. Its default features pull in `native-tls`; they must be off.
- **reqwest 0.12** (NOT 0.13) with `default-features = false, features = ["rustls-tls"]`. teloxide 0.17 pins `reqwest ^0.12.7`; using 0.13 would compile reqwest twice. In 0.12 the feature is `rustls-tls`; in 0.13 it was renamed `rustls`.
- **No OpenSSL.** The whole point of the rustls features above is that the runtime Docker layer needs only `ca-certificates`.
- **120-column line limit** (per the user's global CLAUDE.md), not 80.
- **The shell is fish.** `export FOO=bar` is `set -gx FOO bar`. For heredocs or bash-only syntax, run `bash -c '...'`.
- **Detector rule, never weaken it:** `Empty` iff the card text contains the empty marker; `Available` on *anything else*. Never add a positive pattern match — the available markup is unobservable, so any positive pattern would be an untestable guess.
- **Marker string, verbatim:** `Es gibt aktuell keine Tickets zum Weiterverkauf`
- **Card heading, verbatim:** `Ticketbörse` — matched against `.card-header` **text**, never via a
  `.card-header h2` selector. fatoni.shop renders that heading as a plain `div`, so requiring an
  `h2` would silently stop finding the card after any theme change.
- **Listing enrichment must never gate the alert.** `li[id^="voucher_swap_"]` parsing supplies the
  ticket count and prices. An `Available` observation that parses zero listings still alerts.
- **Target URL:** `https://www.sbtix.de/catalog/tickets/98430-tickets-summer-breeze-2027-summer-breeze-open-air-dinkelsbuehl-am-18-08-2027`

**Deviation from spec, deliberate:** the spec named a `resale_hash` (sha256) field. Implementation compares `resale_text` directly instead and drops the `sha2` dependency — string equality is exactly the property the change-detector needs, and the hash bought nothing since state is in-memory only. A short non-cryptographic fingerprint is derived only for log/message display.

---

### Task 1: Project skeleton and config

**Files:**
- Create: `Cargo.toml`, `src/main.rs`, `src/config.rs`
- Test: inline `#[cfg(test)]` module in `src/config.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `Config { telegram_token: String, chat_id: Option<i64>, ntfy_topic: Option<String>, poll_interval: Duration, target_url: String, user_agent: String, fixture_path: Option<PathBuf> }`; `Config::from_map(&HashMap<String, String>) -> anyhow::Result<Config>`; `Config::from_env() -> anyhow::Result<Config>`; consts `DEFAULT_TARGET_URL`, `DEFAULT_USER_AGENT`.

Config parsing is split into a pure `from_map` plus a thin `from_env` wrapper. Process environment is global mutable state and makes parallel tests race; `from_map` is testable without touching it.

- [ ] **Step 1: Create `Cargo.toml`**

```toml
[package]
name = "sb-watcher"
version = "0.1.0"
edition = "2021"

[dependencies]
teloxide = { version = "0.17", default-features = false, features = ["macros", "ctrlc_handler", "rustls"] }
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls"] }
scraper = "0.27"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "time", "signal", "sync"] }
chrono = "0.4"
anyhow = "1"
async-trait = "0.1"
log = "0.4"
env_logger = "0.11"
```

- [ ] **Step 2: Write the failing config tests**

Create `src/config.rs` containing only the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --lib config`
Expected: FAIL — compile error, `Config` not found.

- [ ] **Step 4: Implement `Config`**

Prepend to `src/config.rs`:

```rust
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
            Some(v) => Some(v.parse::<i64>().with_context(|| {
                format!("TELEGRAM_CHAT_ID must be an integer, got {v:?}")
            })?),
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
```

- [ ] **Step 5: Create a placeholder `src/main.rs` so the crate builds**

```rust
mod config;

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let cfg = config::Config::from_env()?;
    println!("config loaded, target = {}", cfg.target_url);
    Ok(())
}
```

- [ ] **Step 6: Run tests and confirm they pass**

Run: `cargo test`
Expected: 6 passed.

- [ ] **Step 7: Verify the TLS wiring actually excludes OpenSSL**

Run: `cargo tree -i openssl-sys`
Expected: an error like `package ID specification ... did not match any packages`. If OpenSSL *does* appear, the teloxide or reqwest feature flags are wrong — fix them before continuing, because the Dockerfile in Task 9 omits `libssl3`.

Also run `cargo tree -d | grep -A2 reqwest` and confirm reqwest is not duplicated across two major versions.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock src/
git commit -m "feat: project skeleton and config parsing"
```

---

### Task 2: The detector (`parse.rs`)

This is the most important task in the plan. Everything else is plumbing around it.

**Files:**
- Create: `src/parse.rs`, `tests/fixtures/available_blob.html`, `tests/fixtures/available_no_alert.html`, `tests/fixtures/no_card.html`, `tests/fixtures/main_on_sale.html`
- Modify: `src/main.rs` (add the library target)
- Test: `tests/parse_tests.rs`
- Already committed, **do not edit**: `tests/fixtures/empty_resale.html` (real sbtix page),
  `tests/fixtures/real_available_many.html` (real fatoni.shop page, 10 tickets at 45,20 €),
  `tests/fixtures/real_available_one.html` (real berq-shop.de page, 1 ticket at 56,85 €)

**Interfaces:**
- Consumes: nothing.
- Produces: `ResaleState { Empty, Available }`; `Listing { id: String, name: String, price: String }`; `PageObservation { resale: ResaleState, resale_text: String, listings: Vec<Listing>, main_sold_out: bool }`; `ParseError { CardNotFound }`; `classify(html: &str) -> Result<PageObservation, ParseError>`; `normalize_ws(&str) -> String`; `fingerprint(&str) -> String`; consts `EMPTY_MARKER`, `CARD_HEADING`, `SOLD_OUT_MARKER`.

**Two rules that must not be weakened:**

1. `resale` is decided **only** by whether the card text contains `EMPTY_MARKER`. Listing parsing
   never influences it. A positive-pattern detector fails closed — unknown markup would read as
   "all quiet". The inverted rule fails open.
2. The card is located by `.card-header` **text**, not by `.card-header h2`. fatoni.shop renders
   that heading as a plain `div`; requiring an `h2` is one theme change away from blindness.

- [ ] **Step 1: Generate the synthesized fixtures**

The real fixtures above cover today's known markup. These cover the shapes no live sample provides.
Run as `bash -c` (the shell is fish):

```bash
bash -c '
cd tests/fixtures
python3 - <<PYEOF
src = open("empty_resale.html", encoding="utf-8").read()
marker = "Es gibt aktuell keine Tickets zum Weiterverkauf."

end = src.index(marker) + len(marker) + len("</div>")
start = src.rindex("<div class=\"alert alert-info", 0, end)
block = src[start:end]

# The fail-open case: markup nobody has ever seen.
blob = "<div class=\"totally-unexpected\"><span>???</span></div>"
open("available_blob.html", "w", encoding="utf-8").write(src.replace(block, blob))
open("available_no_alert.html", "w", encoding="utf-8").write(src.replace(block, ""))

# Remove the whole Ticketboerse card -> CardNotFound.
cstart = src.rindex("<div class=\"card", 0, src.index("Ticketbörse"))
cend = src.rindex("</div>", cstart, src.index("article-details", cstart))
open("no_card.html", "w", encoding="utf-8").write(src[:cstart] + src[cend:])

open("main_on_sale.html", "w", encoding="utf-8").write(
    src.replace("<div class=\"alert alert-danger \">Ausverkauft</div>", "")
)
print("fixtures written")
PYEOF
'
```

Verify every fixture before writing tests against it:

```bash
bash -c '
cd tests/fixtures
for f in *.html; do
  printf "%-26s card=%s empty=%s soldout=%s vouchers=%s\n" "$f" \
    "$(grep -c "Ticketbörse" $f)" "$(grep -c "keine Tickets zum Weiterverkauf" $f)" \
    "$(grep -c "Ausverkauft" $f)" "$(grep -o "id=\"voucher_swap_[0-9]*\"" $f | wc -l | tr -d " ")"
done'
```

Expected: `empty_resale` card=1 empty=1 soldout≥1 vouchers=0; `real_available_many` card=1 empty=0
vouchers=10; `real_available_one` card=1 empty=0 vouchers=1; `available_blob` and
`available_no_alert` card=1 empty=0 vouchers=0; `no_card` card=0; `main_on_sale` card=1 empty=1
soldout=0.

If any count is wrong the generation failed — fix it before continuing.

- [ ] **Step 2: Write the failing detector tests**

Create `tests/parse_tests.rs`:

```rust
use sb_watcher::parse::{classify, normalize_ws, ParseError, ResaleState};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("tests/fixtures/{name}"))
        .unwrap_or_else(|e| panic!("cannot read fixture {name}: {e}"))
}

// ---------- the real target page ----------

#[test]
fn real_live_sbtix_page_is_empty() {
    let obs = classify(&fixture("empty_resale.html")).expect("card must be found");
    assert_eq!(obs.resale, ResaleState::Empty);
    assert!(obs.main_sold_out, "the live page shows Ausverkauft");
    assert!(obs.listings.is_empty());
}

#[test]
fn card_text_excludes_the_rest_of_the_page() {
    let obs = classify(&fixture("empty_resale.html")).unwrap();
    assert!(obs.resale_text.starts_with("Ticketbörse"));
    assert!(
        !obs.resale_text.contains("Sichere dir jetzt dein Ticket"),
        "text bled outside the card into the Information section"
    );
    assert!(obs.resale_text.len() < 1000, "unexpectedly large: {}", obs.resale_text.len());
}

// ---------- real markup from shops that actually had stock ----------

#[test]
fn real_fatoni_page_reports_ten_tickets_with_prices() {
    let obs = classify(&fixture("real_available_many.html")).unwrap();
    assert_eq!(obs.resale, ResaleState::Available);
    assert_eq!(obs.listings.len(), 10, "fatoni.shop had 10 offers");
    assert!(obs.listings[0].name.contains("FATONI"), "got {:?}", obs.listings[0].name);
    assert!(obs.listings[0].price.contains("45,20"), "got {:?}", obs.listings[0].price);
    assert!(obs.listings[0].id.starts_with("voucher_swap_"));
}

#[test]
fn real_berq_page_reports_one_ticket() {
    // berq renders the Ticketboerse heading as <h2 class="fs-3">, and fatoni as a plain
    // <div>. Both must be found — this test is the guard on the header-text selector.
    let obs = classify(&fixture("real_available_one.html")).unwrap();
    assert_eq!(obs.resale, ResaleState::Available);
    assert_eq!(obs.listings.len(), 1);
    assert!(obs.listings[0].price.contains("56,85"), "got {:?}", obs.listings[0].price);
}

// ---------- fail-open: the property that matters most ----------

#[test]
fn unrecognized_markup_is_available_not_quiet() {
    // sbtix's own populated markup has never been observed. If it does not look
    // like fatoni's, this is the case that saves the tickets.
    let obs = classify(&fixture("available_blob.html")).unwrap();
    assert_eq!(obs.resale, ResaleState::Available);
    assert!(obs.listings.is_empty(), "nothing parseable, but still an alert");
}

#[test]
fn a_removed_alert_is_available() {
    assert_eq!(classify(&fixture("available_no_alert.html")).unwrap().resale, ResaleState::Available);
}

#[test]
fn a_missing_card_is_an_error_not_a_quiet_empty() {
    // Must never silently report "all quiet" — that would blind the watcher
    // permanently after a site redesign.
    assert_eq!(classify(&fixture("no_card.html")), Err(ParseError::CardNotFound));
}

#[test]
fn detects_main_product_back_on_sale() {
    assert!(!classify(&fixture("main_on_sale.html")).unwrap().main_sold_out);
}

#[test]
fn classification_is_stable_across_identical_input() {
    let html = fixture("empty_resale.html");
    assert_eq!(classify(&html).unwrap(), classify(&html).unwrap());
}

#[test]
fn garbage_input_is_an_error() {
    assert_eq!(classify("<html><body>maintenance</body></html>"), Err(ParseError::CardNotFound));
    assert_eq!(classify(""), Err(ParseError::CardNotFound));
}

#[test]
fn normalize_ws_collapses_all_whitespace_kinds() {
    assert_eq!(normalize_ws("  a \n\t b   c "), "a b c");
    assert_eq!(normalize_ws(""), "");
    // U+202F NARROW NO-BREAK SPACE separates price from currency in the real markup.
    assert_eq!(normalize_ws("45,20\u{202f}€"), "45,20 €");
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --test parse_tests`
Expected: FAIL — the `sb_watcher` library crate does not exist yet.

- [ ] **Step 4: Add a library target so integration tests can import the modules**

Create `src/lib.rs`:

```rust
pub mod config;
pub mod parse;
```

And change `src/main.rs` to use the library rather than declaring modules itself:

```rust
use sb_watcher::config::Config;

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let cfg = Config::from_env()?;
    println!("config loaded, target = {}", cfg.target_url);
    Ok(())
}
```

- [ ] **Step 5: Implement `parse.rs`**

```rust
use scraper::{ElementRef, Html, Selector};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::LazyLock;

pub const EMPTY_MARKER: &str = "Es gibt aktuell keine Tickets zum Weiterverkauf";
pub const CARD_HEADING: &str = "Ticketbörse";
pub const SOLD_OUT_MARKER: &str = "Ausverkauft";

static CARD: LazyLock<Selector> = LazyLock::new(|| Selector::parse("div.card").unwrap());
static CARD_HEADER: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".card-header").unwrap());
static TICKET_FRAME: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("turbo-frame#ticket_detail").unwrap());
static DANGER: LazyLock<Selector> = LazyLock::new(|| Selector::parse("div.alert-danger").unwrap());
static SWAP_ITEM: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(r#"li[id^="voucher_swap_"]"#).unwrap());
static SWAP_NAME: LazyLock<Selector> = LazyLock::new(|| Selector::parse("div.fs-6").unwrap());
static SWAP_PRICE: LazyLock<Selector> = LazyLock::new(|| Selector::parse("div.col-auto").unwrap());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResaleState {
    Empty,
    Available,
}

/// One offer on the exchange. Enrichment only — never used to decide `ResaleState`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub id: String,
    pub name: String,
    pub price: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageObservation {
    pub resale: ResaleState,
    pub resale_text: String,
    pub listings: Vec<Listing>,
    pub main_sold_out: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    CardNotFound,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::CardNotFound => write!(f, "Ticketbörse card not found on the page"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Collapse every run of whitespace to a single space and trim. Note U+202F
/// (narrow no-break space, used before € in the real markup) counts as
/// whitespace under Rust's `char::is_whitespace`.
pub fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Short non-cryptographic fingerprint, for logs and status messages only.
pub fn fingerprint(s: &str) -> String {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn text_of(el: &ElementRef) -> String {
    normalize_ws(&el.text().collect::<String>())
}

fn parse_listings(card: &ElementRef) -> Vec<Listing> {
    card.select(&SWAP_ITEM)
        .map(|li| {
            let id = li.value().id().unwrap_or_default().to_string();
            let name = li.select(&SWAP_NAME).next().map(|e| text_of(&e)).unwrap_or_default();
            // The price cell is the first div.col-auto that contains a currency symbol;
            // the trailing col-auto holds the buy button.
            let price = li
                .select(&SWAP_PRICE)
                .map(|e| text_of(&e))
                .find(|t| t.contains('€'))
                .unwrap_or_default();
            Listing { id, name, price }
        })
        .collect()
}

pub fn classify(html: &str) -> Result<PageObservation, ParseError> {
    let doc = Html::parse_document(html);

    // Locate the card by header TEXT, not by element type: the heading is an
    // <h2> on sbtix and berq but a plain <div> on fatoni. Where cards nest,
    // prefer the innermost (shortest text) match.
    let card = doc
        .select(&CARD)
        .filter(|c| c.select(&CARD_HEADER).any(|h| text_of(&h).contains(CARD_HEADING)))
        .min_by_key(|c| c.text().map(str::len).sum::<usize>())
        .ok_or(ParseError::CardNotFound)?;

    let resale_text = text_of(&card);

    // The ONLY thing that decides the alert. Listings below are enrichment.
    let resale = if resale_text.contains(EMPTY_MARKER) {
        ResaleState::Empty
    } else {
        ResaleState::Available
    };

    let listings = parse_listings(&card);

    let main_sold_out = doc.select(&TICKET_FRAME).next().is_some_and(|frame| {
        frame.select(&DANGER).any(|a| text_of(&a).contains(SOLD_OUT_MARKER))
    });

    Ok(PageObservation { resale, resale_text, listings, main_sold_out })
}
```

- [ ] **Step 6: Run the tests and confirm they pass**

Run: `cargo test --test parse_tests`
Expected: 11 passed.

Likely failure points, with fixes:
- `real_fatoni_page_reports_ten_tickets_with_prices` finding 0 listings → the `li[id^=...]`
  attribute selector is unsupported or the items sit outside the matched card. Print
  `obs.resale_text` and check the card boundary.
- `card_text_excludes_the_rest_of_the_page` failing on `starts_with` → an outer `div.card` wraps
  more than intended; the `min_by_key` should already prefer the innermost, so check whether the
  header text match is catching a parent.

- [ ] **Step 7: Commit**

```bash
git add src/lib.rs src/main.rs src/parse.rs tests/
git commit -m "feat: Ticketboerse detector with real and synthesized fixtures"
```

---

### Task 3: Transition logic (`state.rs`)

**Files:**
- Create: `src/state.rs`
- Modify: `src/lib.rs` (add `pub mod state;`)
- Test: `tests/transition_tests.rs`

**Interfaces:**
- Consumes: `parse::{PageObservation, ResaleState}`.
- Produces: `Severity { Max, Info }`; `AlertKind { ResaleAvailable, MainOnSale, ResaleGone, MainSoldOut, ResaleTextChanged, StructureChanged, FetchFailing, Heartbeat }`; `Alert { kind, severity, title, body }`; `Alert::new(kind, severity, title, body)`; `transitions(prev: Option<&PageObservation>, cur: &PageObservation, url: &str) -> Vec<Alert>`.

- [ ] **Step 1: Write the failing transition tests**

Create `tests/transition_tests.rs`:

```rust
use sb_watcher::parse::{Listing, PageObservation, ResaleState};
use sb_watcher::state::{transitions, AlertKind, Severity};

const URL: &str = "https://example.test/ticket";

fn obs(resale: ResaleState, text: &str, main_sold_out: bool) -> PageObservation {
    PageObservation { resale, resale_text: text.to_string(), listings: vec![], main_sold_out }
}

fn empty() -> PageObservation {
    obs(ResaleState::Empty, "Ticketbörse Es gibt aktuell keine Tickets zum Weiterverkauf.", true)
}

fn available() -> PageObservation {
    let mut o = obs(ResaleState::Available, "Ticketbörse Festivalticket 222,00 In den Warenkorb", true);
    o.listings = vec![Listing {
        id: "voucher_swap_1".into(),
        name: "Festivalticket".into(),
        price: "222,00 €".into(),
    }];
    o
}

fn kinds(v: &[sb_watcher::state::Alert]) -> Vec<AlertKind> {
    v.iter().map(|a| a.kind).collect()
}

#[test]
fn quiet_first_run_is_silent() {
    assert!(transitions(None, &empty(), URL).is_empty());
}

#[test]
fn first_run_already_available_alerts_immediately() {
    // A restart during a live drop must not swallow the drop.
    let a = transitions(None, &available(), URL);
    assert_eq!(kinds(&a), vec![AlertKind::ResaleAvailable]);
    assert_eq!(a[0].severity, Severity::Max);
}

#[test]
fn first_run_main_on_sale_alerts_immediately() {
    let a = transitions(None, &obs(ResaleState::Empty, "Ticketbörse Es gibt aktuell keine Tickets zum Weiterverkauf.", false), URL);
    assert_eq!(kinds(&a), vec![AlertKind::MainOnSale]);
    assert_eq!(a[0].severity, Severity::Max);
}

#[test]
fn empty_to_available_is_a_max_alert_carrying_the_link_and_text() {
    let a = transitions(Some(&empty()), &available(), URL);
    assert_eq!(kinds(&a), vec![AlertKind::ResaleAvailable]);
    assert_eq!(a[0].severity, Severity::Max);
    assert!(a[0].body.contains(URL), "alert must contain a clickable link");
    assert!(a[0].body.contains("In den Warenkorb"), "alert must carry the card text");
}

#[test]
fn steady_empty_state_produces_nothing() {
    assert!(transitions(Some(&empty()), &empty(), URL).is_empty());
}

#[test]
fn available_to_empty_is_informational_only() {
    let a = transitions(Some(&available()), &empty(), URL);
    assert_eq!(kinds(&a), vec![AlertKind::ResaleGone]);
    assert_eq!(a[0].severity, Severity::Info);
}

#[test]
fn wording_change_while_still_empty_is_informational() {
    let changed = obs(
        ResaleState::Empty,
        "Ticketbörse Neuer Text. Es gibt aktuell keine Tickets zum Weiterverkauf.",
        true,
    );
    let a = transitions(Some(&empty()), &changed, URL);
    assert_eq!(kinds(&a), vec![AlertKind::ResaleTextChanged]);
    assert_eq!(a[0].severity, Severity::Info);
}

#[test]
fn main_going_on_sale_is_a_max_alert() {
    let on_sale = obs(ResaleState::Empty, empty().resale_text.as_str(), false);
    let a = transitions(Some(&empty()), &on_sale, URL);
    assert_eq!(kinds(&a), vec![AlertKind::MainOnSale]);
    assert_eq!(a[0].severity, Severity::Max);
}

#[test]
fn main_selling_out_again_is_informational() {
    let on_sale = obs(ResaleState::Empty, empty().resale_text.as_str(), false);
    let a = transitions(Some(&on_sale), &empty(), URL);
    assert_eq!(kinds(&a), vec![AlertKind::MainSoldOut]);
    assert_eq!(a[0].severity, Severity::Info);
}

#[test]
fn resale_and_main_can_fire_together() {
    let both = obs(ResaleState::Available, "Ticketbörse tickets!", false);
    let a = transitions(Some(&empty()), &both, URL);
    assert_eq!(kinds(&a), vec![AlertKind::ResaleAvailable, AlertKind::MainOnSale]);
}

#[test]
fn alert_states_how_many_tickets_and_at_what_price() {
    // The goal is two tickets, so the count decides whether it is worth racing.
    let a = transitions(Some(&empty()), &available(), URL);
    assert!(a[0].body.contains("1 ticket available"), "got: {}", a[0].body);
    assert!(a[0].body.contains("222,00 €"), "got: {}", a[0].body);
}

#[test]
fn alert_still_fires_when_no_listings_could_be_parsed() {
    // Fail-open: unfamiliar markup must still wake the user, carrying raw text.
    let mut unparseable = available();
    unparseable.listings = vec![];
    unparseable.resale_text = "Ticketbörse ??? something new".into();
    let a = transitions(Some(&empty()), &unparseable, URL);
    assert_eq!(kinds(&a), vec![AlertKind::ResaleAvailable]);
    assert_eq!(a[0].severity, Severity::Max);
    assert!(a[0].body.contains("something new"), "raw text must survive: {}", a[0].body);
}

#[test]
fn text_change_is_not_reported_alongside_a_state_change() {
    // Empty -> Available always changes the text; reporting both would be noise.
    let a = transitions(Some(&empty()), &available(), URL);
    assert!(!kinds(&a).contains(&AlertKind::ResaleTextChanged));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test transition_tests`
Expected: FAIL — `sb_watcher::state` does not exist.

- [ ] **Step 3: Implement `state.rs`**

```rust
use crate::parse::{Listing, PageObservation, ResaleState};

/// Human summary of what is on offer. Falls back to the raw card text when no
/// listings could be parsed — the alert must never be suppressed or emptied
/// just because the markup was unfamiliar.
pub fn listing_summary(listings: &[Listing], raw_text: &str) -> String {
    if listings.is_empty() {
        return format!(
            "The Ticketbörse is no longer empty, but no offers could be parsed. \
             Check the page yourself:\n\n{raw_text}"
        );
    }
    let n = listings.len();
    let noun = if n == 1 { "ticket" } else { "tickets" };
    let lines: Vec<String> = listings
        .iter()
        .map(|l| format!("• {} — {}", l.name, l.price))
        .collect();
    format!("{n} {noun} available:\n{}", lines.join("\n"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Wake the user: repeats until acknowledged or capped.
    Max,
    /// Worth knowing, sent once.
    Info,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    ResaleAvailable,
    MainOnSale,
    ResaleGone,
    MainSoldOut,
    ResaleTextChanged,
    StructureChanged,
    FetchFailing,
    Heartbeat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub kind: AlertKind,
    pub severity: Severity,
    pub title: String,
    pub body: String,
}

impl Alert {
    pub fn new(kind: AlertKind, severity: Severity, title: impl Into<String>, body: impl Into<String>) -> Self {
        Alert { kind, severity, title: title.into(), body: body.into() }
    }
}

/// Compare the previous observation with the current one and produce alerts.
///
/// Pure: no clock, no I/O. `prev == None` means this is the first observation
/// after start-up.
pub fn transitions(prev: Option<&PageObservation>, cur: &PageObservation, url: &str) -> Vec<Alert> {
    let mut out = Vec::new();

    let resale_available = |out: &mut Vec<Alert>| {
        out.push(Alert::new(
            AlertKind::ResaleAvailable,
            Severity::Max,
            "🎟️ TICKETS AVAILABLE",
            format!("{}\n\n{}", listing_summary(&cur.listings, &cur.resale_text), url),
        ));
    };

    let main_on_sale = |out: &mut Vec<Alert>| {
        out.push(Alert::new(
            AlertKind::MainOnSale,
            Severity::Max,
            "🎟️ MAIN SHOP NO LONGER SOLD OUT",
            format!("The 'Ausverkauft' banner is gone from the main product.\n\n{url}"),
        ));
    };

    match prev {
        // First observation: stay silent unless something is already actionable.
        None => {
            if cur.resale == ResaleState::Available {
                resale_available(&mut out);
            }
            if !cur.main_sold_out {
                main_on_sale(&mut out);
            }
        }
        Some(p) => {
            match (p.resale, cur.resale) {
                (ResaleState::Empty, ResaleState::Available) => resale_available(&mut out),
                (ResaleState::Available, ResaleState::Empty) => out.push(Alert::new(
                    AlertKind::ResaleGone,
                    Severity::Info,
                    "Resale stock gone",
                    format!("The Ticketbörse is empty again.\n\n{url}"),
                )),
                // Same state, but the wording moved: worth knowing, since a
                // rewrite of the empty sentence would otherwise blind the detector.
                _ if p.resale_text != cur.resale_text => out.push(Alert::new(
                    AlertKind::ResaleTextChanged,
                    Severity::Info,
                    "⚠️ Ticketbörse wording changed",
                    format!("Before:\n{}\n\nAfter:\n{}\n\n{}", p.resale_text, cur.resale_text, url),
                )),
                _ => {}
            }

            match (p.main_sold_out, cur.main_sold_out) {
                (true, false) => main_on_sale(&mut out),
                (false, true) => out.push(Alert::new(
                    AlertKind::MainSoldOut,
                    Severity::Info,
                    "Main shop sold out again",
                    format!("The 'Ausverkauft' banner is back.\n\n{url}"),
                )),
                _ => {}
            }
        }
    }

    out
}
```

- [ ] **Step 4: Add the module and run the tests**

Add `pub mod state;` to `src/lib.rs`, then run: `cargo test --test transition_tests`
Expected: 13 passed.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/state.rs tests/transition_tests.rs
git commit -m "feat: alert transition logic"
```

---

### Task 4: Notification channels (`notify.rs`)

**Files:**
- Create: `src/notify.rs`
- Modify: `src/lib.rs` (add `pub mod notify;`)
- Test: inline `#[cfg(test)]` module in `src/notify.rs`

**Interfaces:**
- Consumes: `state::{Alert, AlertKind, Severity}`.
- Produces: `trait Notifier { async fn send(&self, alert: &Alert) -> anyhow::Result<()> }` (via `#[async_trait]`); `FakeNotifier::new()` with `.sent() -> Vec<Alert>` and `.fail_next()`; `TelegramNotifier::new(bot: Bot, chat: ChatId)`; `NtfyNotifier::new(client: reqwest::Client, topic: String)`; `MultiNotifier::new(Vec<Box<dyn Notifier>>)`.

`MultiNotifier` must attempt **every** channel even when one fails. If Telegram is down at 3am, ntfy is the entire point of having a second channel — a short-circuit here would silently defeat it.

- [ ] **Step 1: Write the failing notifier tests**

Append to `src/notify.rs`:

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib notify`
Expected: FAIL — nothing in `notify.rs` is defined yet.

- [ ] **Step 3: Add the tokio test feature**

In `Cargo.toml`:

```toml
[dev-dependencies]
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

- [ ] **Step 4: Implement `notify.rs`**

Prepend to `src/notify.rs`:

```rust
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
            // Deliberately no early return: every channel gets its attempt.
            if let Err(e) = c.send(alert).await {
                log::error!("notifier failed: {e:#}");
                errors.push(e.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow!("{} of {} channels failed: {}", errors.len(), self.channels.len(), errors.join("; ")))
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
        let mut fail = self.fail_next.lock().unwrap();
        if *fail {
            *fail = false;
            return Err(anyhow!("simulated failure"));
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
```

- [ ] **Step 5: Run the tests and confirm they pass**

Add `pub mod notify;` to `src/lib.rs`, then run: `cargo test --lib notify`
Expected: 5 passed.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/notify.rs
git commit -m "feat: Telegram and ntfy notification channels"
```

---

### Task 5: HTTP fetching (`fetch.rs`)

**Files:**
- Create: `src/fetch.rs`
- Modify: `src/lib.rs` (add `pub mod fetch;`)
- Test: inline `#[cfg(test)]` module in `src/fetch.rs`

**Interfaces:**
- Consumes: `config::Config`.
- Produces: `FetchError { Blocked(u16), Http(u16), Network(String), Io(String) }`; `Fetcher::from_config(&Config) -> anyhow::Result<Fetcher>`; `Fetcher::fetch(&self) -> Result<String, FetchError>`; `FetchError::is_blocked(&self) -> bool`.

The fixture short-circuit is what makes the end-to-end rehearsal possible. Since the live site cannot be made to show resale stock on demand, reading HTML from a local file is the only way to exercise the real alert path before it matters.

- [ ] **Step 1: Write the failing fetch tests**

Append to `src/fetch.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::collections::HashMap;

    fn cfg_with(k: &str, v: &str) -> Config {
        let mut m = HashMap::new();
        m.insert("TELOXIDE_TOKEN".to_string(), "123:ABC".to_string());
        m.insert(k.to_string(), v.to_string());
        Config::from_map(&m).unwrap()
    }

    #[tokio::test]
    async fn fixture_path_bypasses_the_network() {
        let cfg = cfg_with("SB_WATCHER_FIXTURE_PATH", "tests/fixtures/empty_resale.html");
        let html = Fetcher::from_config(&cfg).unwrap().fetch().await.unwrap();
        assert!(html.contains("Ticketbörse"));
    }

    #[tokio::test]
    async fn a_missing_fixture_is_an_io_error() {
        let cfg = cfg_with("SB_WATCHER_FIXTURE_PATH", "tests/fixtures/does-not-exist.html");
        assert!(matches!(
            Fetcher::from_config(&cfg).unwrap().fetch().await,
            Err(FetchError::Io(_))
        ));
    }

    #[test]
    fn blocking_statuses_are_recognised() {
        assert!(FetchError::Blocked(429).is_blocked());
        assert!(FetchError::Blocked(403).is_blocked());
        assert!(!FetchError::Http(500).is_blocked());
        assert!(!FetchError::Network("timeout".into()).is_blocked());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib fetch`
Expected: FAIL — `Fetcher` not defined.

- [ ] **Step 3: Implement `fetch.rs`**

Prepend to `src/fetch.rs`:

```rust
use crate::config::Config;
use anyhow::Result;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// 403 / 429 — we are probably being rate-limited or blocked, which means blind.
    Blocked(u16),
    Http(u16),
    Network(String),
    Io(String),
}

impl FetchError {
    pub fn is_blocked(&self) -> bool {
        matches!(self, FetchError::Blocked(_))
    }
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Blocked(c) => write!(f, "blocked by the site (HTTP {c})"),
            FetchError::Http(c) => write!(f, "HTTP {c}"),
            FetchError::Network(e) => write!(f, "network error: {e}"),
            FetchError::Io(e) => write!(f, "fixture read error: {e}"),
        }
    }
}

impl std::error::Error for FetchError {}

pub struct Fetcher {
    client: reqwest::Client,
    url: String,
    fixture: Option<PathBuf>,
}

impl Fetcher {
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(&cfg.user_agent)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Fetcher { client, url: cfg.target_url.clone(), fixture: cfg.fixture_path.clone() })
    }

    pub async fn fetch(&self) -> Result<String, FetchError> {
        // Rehearsal mode: read local HTML instead of hitting the site, so the
        // "tickets available" path can be exercised on demand.
        if let Some(path) = &self.fixture {
            return std::fs::read_to_string(path).map_err(|e| FetchError::Io(e.to_string()));
        }

        let resp = self
            .client
            .get(&self.url)
            .send()
            .await
            .map_err(|e| FetchError::Network(e.to_string()))?;

        let status = resp.status().as_u16();
        if status == 403 || status == 429 {
            return Err(FetchError::Blocked(status));
        }
        if !resp.status().is_success() {
            return Err(FetchError::Http(status));
        }

        resp.text().await.map_err(|e| FetchError::Network(e.to_string()))
    }
}
```

- [ ] **Step 4: Run the tests and confirm they pass**

Add `pub mod fetch;` to `src/lib.rs`, then run: `cargo test --lib fetch`
Expected: 3 passed.

- [ ] **Step 5: Verify against the live site**

```bash
bash -c 'cargo run --quiet --example probe 2>/dev/null || true'
```

Skip the example; instead add a temporary check via the test suite in Task 8's smoke run. (No example binary is part of this plan.)

- [ ] **Step 6: Commit**

```bash
git add src/lib.rs src/fetch.rs
git commit -m "feat: HTTP fetcher with fixture injection for rehearsals"
```

---

### Task 6: Watcher loop, backoff and repeat/cap (`watcher.rs`)

**Files:**
- Create: `src/watcher.rs`
- Modify: `src/lib.rs` (add `pub mod watcher;`)
- Test: inline `#[cfg(test)]` module in `src/watcher.rs`

**Interfaces:**
- Consumes: `config::Config`, `fetch::{Fetcher, FetchError}`, `parse::{classify, PageObservation, ParseError}`, `state::{transitions, Alert, AlertKind, Severity}`, `notify::Notifier`.
- Produces: `AlertRepeat { kind: AlertKind, sent_at: DateTime<Utc>, count: u32 }`; `AppState` with fields `last: Option<PageObservation>`, `checks: u64`, `failures_since: Option<DateTime<Utc>>`, `repeat: Option<AlertRepeat>`, `last_change: Option<DateTime<Utc>>`, `started: DateTime<Utc>`, `backoff: Option<Duration>`, `last_structure_warn: Option<DateTime<Utc>>`, `last_heartbeat: DateTime<Utc>`; `AppState::new(now)`; free functions `next_backoff`, `should_repeat`, `failure_warning_due`; `run_watcher(cfg, fetcher, notifier, shared) -> !`.

Constants: `REPEAT_EVERY = 5 min`, `REPEAT_CAP = 6`, `FAILURE_WARN_AFTER = 15 min`, `HEARTBEAT_EVERY = 24 h`, `BACKOFF_MAX = 600 s`, `BLOCKED_BACKOFF = 300 s`.

- [ ] **Step 1: Write the failing tests for the pure helpers**

Append to `src/watcher.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(min: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + min * 60, 0).unwrap()
    }

    #[test]
    fn backoff_doubles_from_the_poll_interval_and_caps() {
        let base = Duration::from_secs(60);
        assert_eq!(next_backoff(None, base), Duration::from_secs(60));
        assert_eq!(next_backoff(Some(Duration::from_secs(60)), base), Duration::from_secs(120));
        assert_eq!(next_backoff(Some(Duration::from_secs(120)), base), Duration::from_secs(240));
        assert_eq!(next_backoff(Some(Duration::from_secs(240)), base), Duration::from_secs(480));
        assert_eq!(next_backoff(Some(Duration::from_secs(480)), base), BACKOFF_MAX);
        assert_eq!(next_backoff(Some(BACKOFF_MAX), base), BACKOFF_MAX);
    }

    #[test]
    fn repeat_waits_the_interval() {
        let r = AlertRepeat { kind: AlertKind::ResaleAvailable, sent_at: t(0), count: 1 };
        assert!(!should_repeat(&r, t(4)), "too soon");
        assert!(should_repeat(&r, t(5)), "five minutes have passed");
    }

    #[test]
    fn repeat_stops_at_the_cap() {
        let r = AlertRepeat { kind: AlertKind::ResaleAvailable, sent_at: t(0), count: REPEAT_CAP };
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
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib watcher`
Expected: FAIL — helpers not defined.

- [ ] **Step 3: Implement the state types and pure helpers**

Prepend to `src/watcher.rs`:

```rust
use crate::config::Config;
use crate::fetch::{FetchError, Fetcher};
use crate::notify::Notifier;
use crate::parse::{classify, PageObservation, ParseError};
use crate::state::{transitions, Alert, AlertKind, Severity};
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
```

- [ ] **Step 4: Run the helper tests and confirm they pass**

Add `pub mod watcher;` to `src/lib.rs`, then run: `cargo test --lib watcher`
Expected: 4 passed.

- [ ] **Step 5: Write the failing test for a full poll cycle**

Append to the `tests` module in `src/watcher.rs`:

```rust
    use crate::notify::FakeNotifier;
    use crate::parse::ResaleState;

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
        assert_eq!(st.repeat.as_ref().unwrap().count, 1);

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
```

- [ ] **Step 6: Run to verify it fails**

Run: `cargo test --lib watcher`
Expected: FAIL — `apply_observation` not defined.

- [ ] **Step 7: Implement `apply_observation` and the loop**

Append to the non-test part of `src/watcher.rs`:

```rust
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
            st.repeat = Some(AlertRepeat { kind: alert.kind, sent_at: now, count: 1 });
        }
    }

    // A max-severity condition that is still true gets periodic reminders.
    if alerts.is_empty() {
        let still_alerting = obs.resale == crate::parse::ResaleState::Available || !obs.main_sold_out;
        if !still_alerting {
            st.repeat = None;
        } else if let Some(r) = st.repeat.clone() {
            if should_repeat(&r, now) {
                let alert = Alert::new(
                    r.kind,
                    Severity::Max,
                    "🎟️ STILL AVAILABLE",
                    format!(
                        "Reminder {} of {}.\n\n{}\n\n{}",
                        r.count,
                        REPEAT_CAP,
                        crate::state::listing_summary(&obs.listings, &obs.resale_text),
                        url
                    ),
                );
                if let Err(e) = notifier.send(&alert).await {
                    log::error!("failed to deliver reminder: {e:#}");
                }
                st.repeat = Some(AlertRepeat { kind: r.kind, sent_at: now, count: r.count + 1 });
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
                                "The Ticketbörse card could not be found. sb-watcher may be blind \
                                 and needs a code update.\n\n{}",
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
```

- [ ] **Step 8: Run all watcher tests**

Run: `cargo test --lib watcher`
Expected: 6 passed.

- [ ] **Step 9: Commit**

```bash
git add src/lib.rs src/watcher.rs
git commit -m "feat: poll loop with backoff, capped repeats and heartbeat"
```

---

### Task 7: Telegram commands (`bot.rs`)

**Files:**
- Create: `src/bot.rs`
- Modify: `src/lib.rs` (add `pub mod bot;`)
- Test: inline `#[cfg(test)]` module in `src/bot.rs`

**Interfaces:**
- Consumes: `watcher::AppState`, `parse::ResaleState`.
- Produces: `Command` enum (`Status`, `Ack`, `Help`); `status_text(&AppState, now: DateTime<Utc>, url: &str) -> String`; `build_handler()` returning the teloxide dispatch tree; `run_bot(bot, shared, chat_id: Option<i64>)`.

`/check` from the spec is dropped: forcing an immediate poll requires a wakeup channel into the loop, and with a 60s interval it saves at most 59 seconds. `/status` already answers the question it was really for. This is a deliberate YAGNI cut — note it in the README.

- [ ] **Step 1: Write the failing status-text test**

Append to `src/bot.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{PageObservation, ResaleState};
    use crate::watcher::AppState;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000, 0).unwrap()
    }

    #[test]
    fn status_reports_a_quiet_watcher() {
        let mut st = AppState::new(now());
        st.checks = 42;
        st.last = Some(PageObservation {
            resale: ResaleState::Empty,
            resale_text: "Ticketbörse Es gibt aktuell keine Tickets zum Weiterverkauf.".into(),
            listings: vec![],
            main_sold_out: true,
        });
        let s = status_text(&st, now(), "https://example.test");
        assert!(s.contains("42"), "check count must be visible: {s}");
        assert!(s.contains("no resale tickets"), "{s}");
    }

    #[test]
    fn status_reports_available_stock() {
        let mut st = AppState::new(now());
        st.last = Some(PageObservation {
            resale: ResaleState::Available,
            resale_text: "Ticketbörse In den Warenkorb".into(),
            listings: vec![],
            main_sold_out: true,
        });
        assert!(status_text(&st, now(), "u").contains("TICKETS AVAILABLE"));
    }

    #[test]
    fn status_before_the_first_check_says_so() {
        let st = AppState::new(now());
        assert!(status_text(&st, now(), "u").contains("no check completed yet"));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib bot`
Expected: FAIL — `status_text` not defined.

- [ ] **Step 3: Implement `bot.rs`**

Prepend to `src/bot.rs`:

```rust
use crate::parse::ResaleState;
use crate::watcher::AppState;
use chrono::{DateTime, Utc};
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;
use tokio::sync::Mutex;

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "sb-watcher commands:")]
pub enum Command {
    #[command(description = "show what the watcher currently sees")]
    Status,
    #[command(description = "stop repeating the current alert")]
    Ack,
    #[command(description = "show this text")]
    Help,
}

pub fn status_text(st: &AppState, now: DateTime<Utc>, url: &str) -> String {
    let uptime = now - st.started;
    let state = match &st.last {
        None => "no check completed yet".to_string(),
        Some(o) => {
            let resale = match o.resale {
                ResaleState::Empty => "no resale tickets".to_string(),
                ResaleState::Available => format!("🎟️ TICKETS AVAILABLE\n{}", o.resale_text),
            };
            let main = if o.main_sold_out { "main shop: sold out" } else { "main shop: ON SALE" };
            format!("{resale}\n{main}")
        }
    };
    let last_change = st
        .last_change
        .map(|c| c.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "none since start".into());
    let repeat = match &st.repeat {
        Some(r) => format!("\nalerting: reminder {} of {}", r.count, crate::watcher::REPEAT_CAP),
        None => String::new(),
    };
    format!(
        "{state}\n\nchecks: {}\nuptime: {}h {}m\nlast change: {last_change}{repeat}\n\n{url}",
        st.checks,
        uptime.num_hours(),
        uptime.num_minutes() % 60,
    )
}

async fn on_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    shared: Arc<Mutex<AppState>>,
    url: String,
) -> ResponseResult<()> {
    let text = match cmd {
        Command::Help => Command::descriptions().to_string(),
        Command::Status => {
            let st = shared.lock().await;
            status_text(&st, Utc::now(), &url)
        }
        Command::Ack => {
            let mut st = shared.lock().await;
            if st.repeat.take().is_some() {
                "Acknowledged — reminders stopped.".to_string()
            } else {
                "Nothing to acknowledge.".to_string()
            }
        }
    };
    bot.send_message(msg.chat.id, text).await?;
    Ok(())
}

/// Any non-command message replies with the chat id, so first-time setup does
/// not need a third-party bot to discover it.
async fn on_message(bot: Bot, msg: Message) -> ResponseResult<()> {
    bot.send_message(msg.chat.id, format!("This chat's id is: {}", msg.chat.id.0)).await?;
    Ok(())
}

pub async fn run_bot(bot: Bot, shared: Arc<Mutex<AppState>>, url: String) {
    let handler = Update::filter_message()
        .branch(
            dptree::entry()
                .filter_command::<Command>()
                .endpoint(on_command),
        )
        .branch(dptree::endpoint(on_message));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![shared, url])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}
```

Note: `dispatch()` with no listener uses long polling (`getUpdates`) — no webhook, no public URL.

- [ ] **Step 4: Run the tests and confirm they pass**

Add `pub mod bot;` to `src/lib.rs`, then run: `cargo test --lib bot`
Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/bot.rs
git commit -m "feat: Telegram /status and /ack commands with chat-id discovery"
```

---

### Task 8: Wiring (`main.rs`) and the live smoke test

**Files:**
- Modify: `src/main.rs`
- Test: manual, against the live site and a real bot token.

**Interfaces:**
- Consumes: everything above.
- Produces: the running binary.

- [ ] **Step 1: Implement `main.rs`**

```rust
use anyhow::{Context, Result};
use chrono::Utc;
use sb_watcher::bot::run_bot;
use sb_watcher::config::Config;
use sb_watcher::fetch::Fetcher;
use sb_watcher::notify::{MultiNotifier, Notifier, NtfyNotifier, TelegramNotifier};
use sb_watcher::watcher::{run_watcher, AppState};
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::ChatId;
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));

    let cfg = Config::from_env().context("invalid configuration")?;
    let bot = Bot::new(cfg.telegram_token.clone());
    let shared = Arc::new(Mutex::new(AppState::new(Utc::now())));

    let Some(chat_id) = cfg.chat_id else {
        // Discovery mode: no chat configured, so just help the user find theirs.
        log::warn!("TELEGRAM_CHAT_ID is not set — running in discovery mode.");
        log::warn!("Message the bot on Telegram and it will reply with the chat id to use.");
        run_bot(bot, shared, cfg.target_url.clone()).await;
        return Ok(());
    };

    let mut channels: Vec<Box<dyn Notifier>> =
        vec![Box::new(TelegramNotifier::new(bot.clone(), ChatId(chat_id)))];

    match &cfg.ntfy_topic {
        Some(topic) => {
            channels.push(Box::new(NtfyNotifier::new(reqwest::Client::new(), topic.clone())));
            log::info!("ntfy channel enabled");
        }
        None => log::warn!("NTFY_TOPIC not set — Telegram is the only channel"),
    }

    let notifier = Arc::new(MultiNotifier::new(channels));
    let fetcher = Fetcher::from_config(&cfg).context("failed to build HTTP client")?;

    if cfg.fixture_path.is_some() {
        log::warn!("SB_WATCHER_FIXTURE_PATH is set — reading local HTML, NOT the live site");
    }
    log::info!("watching {} every {:?}", cfg.target_url, cfg.poll_interval);

    let watcher = tokio::spawn(run_watcher(cfg.clone(), fetcher, notifier.clone(), shared.clone()));
    let commands = tokio::spawn(run_bot(bot, shared, cfg.target_url.clone()));

    tokio::select! {
        _ = watcher => log::error!("watcher task exited unexpectedly"),
        _ = commands => log::error!("bot task exited unexpectedly"),
    }
    Ok(())
}
```

- [ ] **Step 2: Build and run the whole suite**

Run: `cargo build --release && cargo test`
Expected: everything compiles, all tests pass.

- [ ] **Step 3: Check formatting and lints**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings`
Fix anything reported. Keep lines within 120 columns.

- [ ] **Step 4: Discovery-mode smoke test**

```bash
bash -c 'TELOXIDE_TOKEN=<token> cargo run'
```

Message the bot on Telegram; it must reply with the chat id. Record that id. Stop with Ctrl-C.

- [ ] **Step 5: Live smoke test against the real site**

```bash
bash -c 'TELOXIDE_TOKEN=<token> TELEGRAM_CHAT_ID=<id> POLL_INTERVAL_SECS=10 RUST_LOG=info cargo run'
```

Expected: logs show polls succeeding, and **no alert fires** (the site is quiet). Send `/status` — the reply must say `no resale tickets` and `main shop: sold out`, with a rising check count. This confirms the detector agrees with reality.

- [ ] **Step 6: Commit**

```bash
git add src/main.rs
git commit -m "feat: wire watcher and bot tasks together"
```

---

### Task 9: The rehearsal, Docker, fly.io and README

**Files:**
- Create: `Dockerfile`, `fly.toml`, `README.md`
- Reference: `../rust-telegram-bot-skeleton/Dockerfile` for the cargo-chef structure.

- [ ] **Step 1: The forced-alert rehearsal — the test that matters most**

The live site cannot be made to show resale stock on demand, so this is the only way to prove the alert path works before the night it has to.

```bash
bash -c 'TELOXIDE_TOKEN=<token> TELEGRAM_CHAT_ID=<id> NTFY_TOPIC=<topic> \
  SB_WATCHER_FIXTURE_PATH=tests/fixtures/real_available_many.html \
  POLL_INTERVAL_SECS=10 RUST_LOG=info cargo run'
```

Confirm all of the following, and do not proceed until each is observed:
1. A 🎟️ TICKETS AVAILABLE message arrives **on Telegram**, reading `10 tickets available` with
   `45,20 €` prices and the link. This is real markup from a shop that genuinely had 10 on offer.
2. The same alert arrives **on ntfy**, at max priority, audibly.
3. After 5 minutes a `STILL AVAILABLE` reminder arrives (reminder 1 of 6).
4. Sending `/ack` stops the reminders and replies "Acknowledged".
5. `/status` shows `TICKETS AVAILABLE`.

Then re-run twice more:
- `SB_WATCHER_FIXTURE_PATH=tests/fixtures/available_blob.html` — confirm the alert still fires,
  saying offers could not be parsed. This is the **fail-open** path, and the one most likely to be
  what sbtix actually does, since its populated markup has never been observed.
- `SB_WATCHER_FIXTURE_PATH=tests/fixtures/no_card.html` — confirm the ⚠️ Page structure changed
  warning arrives. This is the site-redesign blindness guard.

- [ ] **Step 2: Create the `Dockerfile`**

```dockerfile
FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --bin sb-watcher

# rustls means no OpenSSL in the runtime layer — only root certificates.
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/sb-watcher /usr/local/bin/
ENTRYPOINT ["/usr/local/bin/sb-watcher"]
```

- [ ] **Step 3: Create `fly.toml`**

No `[http_service]` and no `PORT`: long polling needs no inbound port, and omitting the service is what keeps the machine from being auto-stopped.

```toml
app = 'sb-watcher'
primary_region = 'fra'

[build]

[env]
  RUST_LOG = 'info'

[[vm]]
  memory = '256mb'
  cpu_kind = 'shared'
  cpus = 1
```

- [ ] **Step 4: Verify the image builds and contains no OpenSSL**

```bash
bash -c 'docker build -t sb-watcher:test . && docker run --rm --entrypoint sh sb-watcher:test -c "ldd /usr/local/bin/sb-watcher | grep -i ssl || echo NO-OPENSSL"'
```

Expected: `NO-OPENSSL`.

- [ ] **Step 5: Write `README.md`**

Cover: what it watches and why the detector is inverted (any change from the known-quiet state, because the available markup is unobservable); the env vars table from the spec; how to get a bot token and discover the chat id; how to run the rehearsal with `SB_WATCHER_FIXTURE_PATH`; the fly deploy commands; that `/check` was deliberately cut; and the reminder that **ntfy topics are public to anyone who knows the name**, so it must be unguessable.

- [ ] **Step 6: Deploy**

```bash
bash -c '
fly launch --no-deploy --copy-config --name sb-watcher --region fra
fly secrets set TELOXIDE_TOKEN=<token> TELEGRAM_CHAT_ID=<id> NTFY_TOPIC=<topic>
fly deploy
fly logs
'
```

Expected in `fly logs`: `watching https://www.sbtix.de/... every 60s` followed by successful polls.

- [ ] **Step 7: Confirm the deployment answers**

Send `/status` on Telegram. The reply must come from the deployed machine with a rising check count.

Run `fly status` and confirm exactly one machine in `started` state.

- [ ] **Step 8: Commit**

```bash
git add Dockerfile fly.toml README.md
git commit -m "feat: Docker image, fly.io config and README"
```

- [ ] **Step 9: Next-day check**

Confirm the ✅ heartbeat message arrives roughly 24 hours after start. If it does not, the dead-man's switch is broken and silence is no longer trustworthy — investigate before relying on the watcher.

---

## Self-Review

**Spec coverage.** Every spec section maps to a task: config → 1; detector, card location, `CardNotFound` → 2; transition table and first-run rule → 3; Telegram + ntfy, and the "one channel failing must not suppress the other" requirement → 4; fetch, timeouts, blocked detection, fixture injection → 5; backoff, repeat/cap, failure warning, heartbeat → 6; `/status`, `/ack`, discovery mode → 7; wiring → 8; rehearsal, Docker, fly, README → 9.

**Deliberate deviations, both noted above:** `sha2`/`resale_hash` replaced by direct string comparison plus a display-only fingerprint; `/check` dropped as YAGNI.

**Type consistency.** `PageObservation` fields (`resale`, `resale_text`, `main_sold_out`) are used identically in Tasks 2, 3, 6, 7. `Alert` is constructed only via `Alert::new(kind, severity, title, body)`. `AlertRepeat` fields (`kind`, `sent_at`, `count`) match between Tasks 6 and 7. `Notifier::send(&self, &Alert) -> Result<()>` is the single trait signature used in Tasks 4, 6, 8.

**Known risks to watch during execution:**
- `scraper` 0.27's `ElementRef::text()` on the card includes the heading — the tests assume this and assert `starts_with("Ticketbörse")`.
- If teloxide 0.17 and reqwest 0.12 disagree on rustls provider, Task 1 Step 7 catches it before the Dockerfile depends on it.
- `Selector::parse("turbo-frame#ticket_detail")` relies on CSS type selectors matching custom elements; the `detects_main_product_back_on_sale` test in Task 2 verifies this against real markup.
- The attribute selector `li[id^="voucher_swap_"]` must be supported by `scraper` 0.27. If it is not, fall back to selecting all `li.list-group-item` and filtering on the id prefix in Rust. Listing parsing is enrichment, so a failure here degrades the alert's detail but must never suppress the alert.
- The `min_by_key` innermost-card rule is new; if the Ticketbörse card ever stops nesting, it still selects the only match.

**Post-approval change, folded in:** a scan of all 68 sibling Tickettoaster shops located two events with live resale stock, so the populated markup is no longer unobserved. This produced two corrections to the original plan — the card must be matched on `.card-header` text rather than `.card-header h2` (fatoni.shop uses a plain `div`, which the original selector would have missed), and `voucher_swap` parsing now supplies ticket count and prices to the alert. The inverted detection rule is unchanged: the real markup is used for enrichment only, never as a gate.
