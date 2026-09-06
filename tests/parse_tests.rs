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
    assert!(obs.resale_text.starts_with("Ticketbörse"), "got: {}", obs.resale_text);
    assert!(
        !obs.resale_text.contains("Sichere dir jetzt dein Ticket"),
        "text bled outside the card into the Information section"
    );
    assert!(
        obs.resale_text.len() < 1000,
        "unexpectedly large: {}",
        obs.resale_text.len()
    );
}

// ---------- real markup from shops that actually had stock ----------

#[test]
fn real_fatoni_page_reports_ten_tickets_with_prices() {
    let obs = classify(&fixture("real_available_many.html")).unwrap();
    assert_eq!(obs.resale, ResaleState::Available);
    assert_eq!(obs.listings.len(), 10, "fatoni.shop had 10 offers");
    assert!(
        obs.listings[0].name.contains("FATONI"),
        "got {:?}",
        obs.listings[0].name
    );
    assert!(
        obs.listings[0].price.contains("45,20"),
        "got {:?}",
        obs.listings[0].price
    );
    assert!(obs.listings[0].id.starts_with("voucher_swap_"));
}

#[test]
fn real_berq_page_reports_one_ticket() {
    // berq renders the Ticketboerse heading as <h2 class="fs-3">, and fatoni as a plain
    // <div>. Both must be found — this test is the guard on the header-text selector.
    let obs = classify(&fixture("real_available_one.html")).unwrap();
    assert_eq!(obs.resale, ResaleState::Available);
    assert_eq!(obs.listings.len(), 1);
    assert!(
        obs.listings[0].price.contains("56,85"),
        "got {:?}",
        obs.listings[0].price
    );
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
    assert_eq!(
        classify(&fixture("available_no_alert.html")).unwrap().resale,
        ResaleState::Available
    );
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
    assert_eq!(
        classify("<html><body>maintenance</body></html>"),
        Err(ParseError::CardNotFound)
    );
    assert_eq!(classify(""), Err(ParseError::CardNotFound));
}

#[test]
fn normalize_ws_collapses_all_whitespace_kinds() {
    assert_eq!(normalize_ws("  a \n\t b   c "), "a b c");
    assert_eq!(normalize_ws(""), "");
    // U+202F NARROW NO-BREAK SPACE separates price from currency in the real markup.
    assert_eq!(normalize_ws("45,20\u{202f}€"), "45,20 €");
}
