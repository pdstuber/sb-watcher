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

    // Validate the token before anything else. Without this, a bad token
    // surfaces as a panic from deep inside teloxide's dispatcher
    // ("Couldn't prepare dispatching context: Api(InvalidToken)"), which says
    // nothing about what to actually do — and on fly it becomes a crashloop.
    match bot.get_me().await {
        Ok(me) => log::info!("authenticated as @{}", me.username()),
        Err(e) => {
            anyhow::bail!(
                "Telegram rejected TELOXIDE_TOKEN ({e}).\n\
                 Re-copy it from @BotFather: send /mybots, pick the bot, then 'API Token'.\n\
                 Note that /revoke or /token issues a NEW token and invalidates the old one."
            );
        }
    }

    let shared = Arc::new(Mutex::new(AppState::new(Utc::now())));

    let Some(chat_id) = cfg.chat_id else {
        // Discovery mode: no chat configured, so just help the user find theirs.
        log::warn!("TELEGRAM_CHAT_ID is not set — running in discovery mode.");
        log::warn!("Message the bot on Telegram and it will reply with the chat id to use.");
        run_bot(bot, shared, cfg.target_url.clone()).await;
        return Ok(());
    };

    let mut channels: Vec<Box<dyn Notifier>> = vec![Box::new(TelegramNotifier::new(bot.clone(), ChatId(chat_id)))];

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

    // Neither task should ever finish: run_watcher loops forever and the bot
    // dispatcher runs until shutdown. If one does return, the process MUST exit
    // non-zero — fly's default restart policy is "on-failure", so returning
    // Ok(()) here would look like a clean shutdown and the machine would never
    // be restarted, leaving the watcher silently dead.
    tokio::select! {
        _ = watcher => anyhow::bail!("watcher task exited unexpectedly — restarting"),
        _ = commands => anyhow::bail!("bot task exited unexpectedly — restarting"),
    }
}
