use sb_watcher::config::Config;

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let cfg = Config::from_env()?;
    println!("config loaded, target = {}", cfg.target_url);
    Ok(())
}
