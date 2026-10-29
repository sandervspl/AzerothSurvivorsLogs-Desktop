use anyhow::{Context, Result};
use azs_uploader_core::{Engine, StageOptions};
use chrono::Datelike;
use std::{collections::HashMap, path::Path};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    let mut options = HashMap::new();
    while let Some(key) = args.next() {
        options.insert(key, args.next().context("Missing option value")?);
    }
    let option = |name: &str| options.get(name).map(String::as_str);
    let state = Path::new(option("--state").unwrap_or(".state/uploader"));
    match command.as_str() {
        "inspect-game" => {
            println!(
                "{}",
                azs_uploader_core::client_build::inspect(Path::new(
                    option("--game-exe").context("Pass --game-exe")?
                ))?
            );
            return Ok(());
        }
        "release-config" => {
            let config = azs_uploader_core::distribution::ReleaseConfig::configured()?;
            println!(
                "{}",
                serde_json::json!({"catalogUrl":config.url,"keyIds":config.keys.keys().collect::<Vec<_>>()})
            );
            return Ok(());
        }
        "setup-game" => {
            let settings = azs_uploader_core::distribution::Settings::choose(
                state,
                Path::new(option("--game").context("Pass --game")?),
            )?;
            println!("{}", serde_json::to_string(&settings.discover()?)?);
            return Ok(());
        }
        "installation" | "maintain-addon" => {
            let settings = azs_uploader_core::distribution::Settings::load(state)?
                .context("Choose a game directory first")?;
            if command == "installation" {
                println!("{}", serde_json::to_string(&settings.discover()?)?);
            } else {
                let config = azs_uploader_core::distribution::ReleaseConfig::configured()?;
                let status =
                    azs_uploader_core::distribution::maintain_addon(state, &settings, &config)
                        .await?;
                println!("{}", serde_json::to_string(&status)?);
            }
            return Ok(());
        }
        _ => {}
    }
    if command == "watch-talents" {
        let monitor = azs_uploader_core::native_history::Monitor::start(
            option("--state").unwrap_or(".state/uploader").into(),
            Path::new(option("--save").context("Pass --save")?).into(),
            Path::new(option("--game-exe").context("Pass --game-exe")?).into(),
        );
        for _ in 0..600 {
            if monitor.is_ready() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        if !monitor.is_ready() {
            anyhow::bail!("Native talent monitoring could not read a valid save");
        }
        println!("Native talent history monitoring started");
        tokio::time::sleep(std::time::Duration::from_secs(
            option("--watch-seconds").unwrap_or("86400").parse()?,
        ))
        .await;
        drop(monitor);
        return Ok(());
    }
    if command == "inspect-talents" {
        let snapshot = azs_uploader_core::native_save::snapshot(
            Path::new(option("--save").context("Pass --save")?),
            Path::new(option("--game-exe").context("Pass --game-exe")?),
        )?;
        println!("{}", serde_json::to_string(&snapshot)?);
        return Ok(());
    }
    let engine = Engine::new(option("--state").unwrap_or(".state/uploader"))?;
    let api = option("--api").unwrap_or("http://127.0.0.1:8787");
    let token_file = option("--token-file")
        .context("Pass --token-file containing a scoped device credential")?;
    let token = std::fs::read_to_string(token_file)?.trim().to_string();
    let progress = |text: String| eprintln!("{text}");
    match command.as_str() {
        "stage" | "upload" => {
            let discovered = azs_uploader_core::distribution::Settings::load(state)?
                .map(|s| s.discover())
                .transpose()?;
            let combat = option("--combat")
                .map(Path::new)
                .or_else(|| discovered.as_ref().and_then(|d| d.combat.as_deref()))
                .context("No combat log found. Choose a game directory or pass --combat.")?;
            let recorder = option("--recorder")
                .map(Path::new)
                .or_else(|| discovered.as_ref().and_then(|d| d.recorder.as_deref()));
            let owner = engine.account(api, &token).await?;
            let year = option("--year")
                .map(str::parse)
                .transpose()?
                .unwrap_or(chrono::Local::now().year());
            let offset = option("--offset")
                .map(str::parse)
                .transpose()?
                .unwrap_or(chrono::Local::now().offset().local_minus_utc() / 60);
            let job = engine.stage(StageOptions {
                api,
                owner: &owner,
                combat,
                recorder,
                native_save: match (option("--save"), option("--game-exe")) {
                    (Some(save), Some(exe)) => Some((Path::new(save), Path::new(exe))),
                    (None, None) => None,
                    _ => anyhow::bail!("Pass both --save and --game-exe for native talents"),
                },
                year,
                offset,
                visibility: option("--visibility").unwrap_or("public"),
            })?;
            if command == "stage" {
                println!("{}", serde_json::to_string(&job)?);
            } else {
                println!("{}", engine.upload(&job, &token, progress).await?);
            }
        }
        "resume" => {
            for job in engine.pending()? {
                if job.api == api {
                    println!("{}", engine.upload(&job, &token, progress).await?);
                }
            }
        }
        _ => anyhow::bail!(
            "Use stage, upload, resume, inspect-talents or watch-talents. Options: --combat PATH --recorder PATH --save PATH --game-exe PATH --api ORIGIN --token-file PATH --state PATH --year YYYY --offset MINUTES --visibility public (default), private or unlisted"
        ),
    }
    Ok(())
}
