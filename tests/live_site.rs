//! Checks the detector against the real sbtix.de page.
//!
//! Ignored by default because it needs network access. Run explicitly with:
//!     cargo test --test live_site -- --ignored --nocapture
//!
//! Worth running after any change to `parse.rs`, and occasionally on its own:
//! if the committed fixture has drifted from the live page, this fails while
//! the fixture-based tests still pass.

use sb_watcher::config::{Config, DEFAULT_TARGET_URL};
use sb_watcher::fetch::Fetcher;
use sb_watcher::parse::{classify, ResaleState};
use std::collections::HashMap;

#[tokio::test]
#[ignore]
async fn live_page_still_parses() {
    let mut env = HashMap::new();
    env.insert("TELOXIDE_TOKEN".to_string(), "unused:for-parsing-only".to_string());
    let cfg = Config::from_map(&env).unwrap();

    let html = Fetcher::from_config(&cfg)
        .unwrap()
        .fetch()
        .await
        .expect("the live site must be reachable");

    println!("fetched {} bytes from {}", html.len(), DEFAULT_TARGET_URL);

    let obs = classify(&html).expect("the Ticketbörse card must still be findable");

    println!("resale        : {:?}", obs.resale);
    println!("main_sold_out : {}", obs.main_sold_out);
    println!("listings      : {}", obs.listings.len());
    println!("card text     : {}", obs.resale_text);

    // If this ever fails, it is either a false alarm or the real thing — check
    // the printed card text before assuming the test is wrong.
    assert_eq!(
        obs.resale,
        ResaleState::Empty,
        "resale is no longer empty — either the detector broke or tickets are actually available"
    );
}
