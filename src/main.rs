use anyhow::{Context, Result};
use chrono::Utc;
use sb_watcher::bot::{run_bot, AllowedChat};
use sb_watcher::config::Config;
use sb_watcher::fetch::Fetcher;
use sb_watcher::notify::{ntfy_client, MultiNotifier, Notifier, NtfyNotifier, TelegramNotifier, NTFY_DEFAULT_BASE_URL};
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

    if cfg.discovery {
        // Explicit opt-in only: an accidentally blank TELEGRAM_CHAT_ID must be a
        // startup error, never a silent bot-only deployment with no watcher.
        log::warn!("SB_WATCHER_DISCOVERY is set — running in discovery mode, NOT watching.");
        log::warn!("Message the bot on Telegram and it will reply with the chat id to use.");
        run_bot(bot, shared, cfg.target_url.clone(), AllowedChat(None)).await;
        anyhow::bail!("bot task exited unexpectedly — restarting");
    }
    let Some(chat_id) = cfg.chat_id else {
        anyhow::bail!("TELEGRAM_CHAT_ID missing outside discovery mode; Config::from_map should have rejected this");
    };

    let mut channels: Vec<Box<dyn Notifier>> = vec![Box::new(TelegramNotifier::new(bot.clone(), ChatId(chat_id)))];

    match &cfg.ntfy_topic {
        Some(topic) => {
            let client = ntfy_client().context("failed to build ntfy HTTP client")?;
            channels.push(Box::new(NtfyNotifier::new(
                client,
                NTFY_DEFAULT_BASE_URL,
                topic.clone(),
            )));
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
    let commands = tokio::spawn(run_bot(
        bot,
        shared,
        cfg.target_url.clone(),
        AllowedChat(Some(ChatId(chat_id))),
    ));

    // Neither task should ever finish: run_watcher loops forever and the bot
    // dispatcher runs until shutdown. If one does return, the process MUST exit
    // non-zero. fly.toml sets the restart policy to "always" as a belt, but the
    // default is "on-failure", and a clean Ok(()) exit previously left a dead
    // watcher looking like a deliberate shutdown (CLAUDE.md invariant 5).
    tokio::select! {
        _ = watcher => anyhow::bail!("watcher task exited unexpectedly — restarting"),
        _ = commands => anyhow::bail!("bot task exited unexpectedly — restarting"),
    }
}
