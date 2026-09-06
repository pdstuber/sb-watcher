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
                ResaleState::Available => format!(
                    "🎟️ TICKETS AVAILABLE\n{}",
                    crate::state::listing_summary(&o.listings, &o.resale_text)
                ),
            };
            let main = if o.main_sold_out {
                "main shop: sold out"
            } else {
                "main shop: ON SALE"
            };
            format!("{resale}\n{main}")
        }
    };
    let last_change = st
        .last_change
        .map(|c| c.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "none since start".into());
    let repeat = match &st.repeat {
        Some(r) => format!(
            "\nalerting: {} of {} reminders sent",
            r.count,
            crate::watcher::REPEAT_CAP
        ),
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
    // Logged for commands too, not just plain messages: in a group the bot's
    // privacy mode means only commands reach it, so this is the only way to
    // discover a group's chat id.
    log::info!("DISCOVERED CHAT ID: {}", msg.chat.id.0);

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
    log::info!("DISCOVERED CHAT ID: {}", msg.chat.id.0);
    bot.send_message(msg.chat.id, format!("This chat's id is: {}", msg.chat.id.0))
        .await?;
    Ok(())
}

pub async fn run_bot(bot: Bot, shared: Arc<Mutex<AppState>>, url: String) {
    let handler = Update::filter_message()
        .branch(dptree::entry().filter_command::<Command>().endpoint(on_command))
        .branch(dptree::endpoint(on_message));

    // No listener passed: teloxide defaults to long polling (getUpdates), so no
    // public URL or inbound port is required.
    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![shared, url])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::PageObservation;
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
