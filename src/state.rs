use crate::parse::{Listing, MainStock, PageObservation, ResaleState};

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
    let lines: Vec<String> = listings.iter().map(|l| format!("• {} — {}", l.name, l.price)).collect();
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
    Started,
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
        Alert {
            kind,
            severity,
            title: title.into(),
            body: body.into(),
        }
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
            if cur.main == MainStock::OnSale {
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
                // Still empty, but the wording moved: worth knowing, since a
                // rewrite of the empty sentence would otherwise blind the detector.
                // Deliberately NOT for Available→Available: listings churn while
                // stock is up, and that is not the detector going blind.
                (ResaleState::Empty, ResaleState::Empty) if p.resale_text != cur.resale_text => out.push(Alert::new(
                    AlertKind::ResaleTextChanged,
                    Severity::Info,
                    "⚠️ Ticketbörse wording changed",
                    format!("Before:\n{}\n\nAfter:\n{}\n\n{}", p.resale_text, cur.resale_text, url),
                )),
                (ResaleState::Empty, ResaleState::Empty) | (ResaleState::Available, ResaleState::Available) => {}
            }

            match (p.main, cur.main) {
                // Fail open: coming back from Unknown straight to OnSale is
                // treated as a drop rather than swallowed.
                (MainStock::SoldOut | MainStock::Unknown, MainStock::OnSale) => main_on_sale(&mut out),
                (MainStock::OnSale, MainStock::SoldOut) => out.push(Alert::new(
                    AlertKind::MainSoldOut,
                    Severity::Info,
                    "Main shop sold out again",
                    format!("The 'Ausverkauft' banner is back.\n\n{url}"),
                )),
                (MainStock::SoldOut, MainStock::SoldOut)
                | (MainStock::SoldOut, MainStock::Unknown)
                | (MainStock::OnSale, MainStock::OnSale)
                | (MainStock::OnSale, MainStock::Unknown)
                | (MainStock::Unknown, MainStock::SoldOut)
                | (MainStock::Unknown, MainStock::Unknown) => {}
            }
        }
    }

    out
}
