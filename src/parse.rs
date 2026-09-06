use scraper::{ElementRef, Html, Selector};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::LazyLock;

pub const EMPTY_MARKER: &str = "Es gibt aktuell keine Tickets zum Weiterverkauf";
pub const CARD_HEADING: &str = "Ticketbörse";
pub const SOLD_OUT_MARKER: &str = "Ausverkauft";

static CARD: LazyLock<Selector> = LazyLock::new(|| Selector::parse("div.card").unwrap());
static CARD_HEADER: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".card-header").unwrap());
static TICKET_FRAME: LazyLock<Selector> = LazyLock::new(|| Selector::parse("turbo-frame#ticket_detail").unwrap());
static DANGER: LazyLock<Selector> = LazyLock::new(|| Selector::parse("div.alert-danger").unwrap());
static SWAP_ITEM: LazyLock<Selector> = LazyLock::new(|| Selector::parse(r#"li[id^="voucher_swap_"]"#).unwrap());
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
            // The price cell is the first div.col-auto containing a currency symbol;
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
}
