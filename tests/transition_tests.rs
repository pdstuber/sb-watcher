use sb_watcher::parse::{Listing, MainStock, PageObservation, ResaleState};
use sb_watcher::state::{transitions, Alert, AlertKind, Severity};

const URL: &str = "https://example.test/ticket";

fn obs(resale: ResaleState, text: &str, main: MainStock) -> PageObservation {
    PageObservation {
        resale,
        resale_text: text.to_string(),
        listings: vec![],
        main,
    }
}

fn empty() -> PageObservation {
    obs(
        ResaleState::Empty,
        "Ticketbörse Es gibt aktuell keine Tickets zum Weiterverkauf.",
        MainStock::SoldOut,
    )
}

fn available() -> PageObservation {
    let mut o = obs(
        ResaleState::Available,
        "Ticketbörse Festivalticket 222,00 In den Warenkorb",
        MainStock::SoldOut,
    );
    o.listings = vec![Listing {
        id: "voucher_swap_1".into(),
        name: "Festivalticket".into(),
        price: "222,00 €".into(),
    }];
    o
}

fn kinds(v: &[Alert]) -> Vec<AlertKind> {
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
    let cur = obs(
        ResaleState::Empty,
        "Ticketbörse Es gibt aktuell keine Tickets zum Weiterverkauf.",
        MainStock::OnSale,
    );
    let a = transitions(None, &cur, URL);
    assert_eq!(kinds(&a), vec![AlertKind::MainOnSale]);
    assert_eq!(a[0].severity, Severity::Max);
}

#[test]
fn empty_to_available_is_a_max_alert_carrying_the_link_and_text() {
    let a = transitions(Some(&empty()), &available(), URL);
    assert_eq!(kinds(&a), vec![AlertKind::ResaleAvailable]);
    assert_eq!(a[0].severity, Severity::Max);
    assert!(a[0].body.contains(URL), "alert must contain a clickable link");
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
    assert!(
        a[0].body.contains("something new"),
        "raw text must survive: {}",
        a[0].body
    );
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
        MainStock::SoldOut,
    );
    let a = transitions(Some(&empty()), &changed, URL);
    assert_eq!(kinds(&a), vec![AlertKind::ResaleTextChanged]);
    assert_eq!(a[0].severity, Severity::Info);
}

#[test]
fn main_going_on_sale_is_a_max_alert() {
    let on_sale = obs(ResaleState::Empty, empty().resale_text.as_str(), MainStock::OnSale);
    let a = transitions(Some(&empty()), &on_sale, URL);
    assert_eq!(kinds(&a), vec![AlertKind::MainOnSale]);
    assert_eq!(a[0].severity, Severity::Max);
}

#[test]
fn main_selling_out_again_is_informational() {
    let on_sale = obs(ResaleState::Empty, empty().resale_text.as_str(), MainStock::OnSale);
    let a = transitions(Some(&on_sale), &empty(), URL);
    assert_eq!(kinds(&a), vec![AlertKind::MainSoldOut]);
    assert_eq!(a[0].severity, Severity::Info);
}

#[test]
fn resale_and_main_can_fire_together() {
    let both = obs(ResaleState::Available, "Ticketbörse tickets!", MainStock::OnSale);
    let a = transitions(Some(&empty()), &both, URL);
    assert_eq!(kinds(&a), vec![AlertKind::ResaleAvailable, AlertKind::MainOnSale]);
}

#[test]
fn text_change_is_not_reported_alongside_a_state_change() {
    // Empty -> Available always changes the text; reporting both would be noise.
    let a = transitions(Some(&empty()), &available(), URL);
    assert!(!kinds(&a).contains(&AlertKind::ResaleTextChanged));
}

#[test]
fn listing_churn_while_available_is_not_a_wording_change() {
    // During a drop the card text changes every time an offer sells. That is
    // not the detector going blind and must not spam Info alerts.
    let mut before = available();
    before.resale_text = "Ticketbörse Festivalticket 222,00 Festivalticket 222,00 In den Warenkorb".into();
    let after = available();
    assert!(transitions(Some(&before), &after, URL).is_empty());
}

#[test]
fn unknown_main_frame_is_not_an_on_sale_alert() {
    let cur = obs(ResaleState::Empty, empty().resale_text.as_str(), MainStock::Unknown);
    assert!(
        transitions(Some(&empty()), &cur, URL).is_empty(),
        "Unknown is a structure problem, not a drop"
    );
    assert!(transitions(None, &cur, URL).is_empty());
}

#[test]
fn recovering_from_unknown_straight_to_on_sale_fails_open() {
    let prev = obs(ResaleState::Empty, empty().resale_text.as_str(), MainStock::Unknown);
    let cur = obs(ResaleState::Empty, empty().resale_text.as_str(), MainStock::OnSale);
    assert_eq!(kinds(&transitions(Some(&prev), &cur, URL)), vec![AlertKind::MainOnSale]);
}
