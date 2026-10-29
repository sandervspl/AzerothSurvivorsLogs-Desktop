use azs_uploader_core::distribution::{ReleaseConfig, Settings, maintain_addon};
use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

pub enum Command {
    Choose(PathBuf),
    Check,
    Pause,
}
pub struct Snapshot {
    pub root: Option<PathBuf>,
    pub combat: Option<PathBuf>,
    pub recorder: Option<PathBuf>,
    pub status: String,
    pub busy: bool,
    pub paused: bool,
}
pub fn start(state: PathBuf, sender: mpsc::Sender<super::Message>) -> mpsc::Sender<Command> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut error = None;
        let mut settings = match Settings::load(&state) {
            Ok(value) => value,
            Err(e) => {
                error = Some(e.to_string());
                None
            }
        };
        let mut status = error.unwrap_or_else(|| {
            "Choose your game directory to download and install the recorder.".into()
        });
        let mut next = Instant::now();
        let mut attempts = 0u32;
        let mut waiting_for_game = false;
        let mut force_check = false;
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(e) => {
                let _ = sender.send(super::Message::Setup(Box::new(Snapshot {
                    root: None,
                    combat: None,
                    recorder: None,
                    status: format!("Cannot start setup: {e}. Restart the uploader."),
                    busy: false,
                    paused: false,
                })));
                return;
            }
        };
        loop {
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok(Command::Choose(root)) => match Settings::choose(&state, &root) {
                    Ok(chosen) => {
                        settings = Some(chosen);
                        next = Instant::now();
                        attempts = 0;
                        status = "Downloading recorder...".into();
                    }
                    Err(e) => status = format!("{e:#}"),
                },
                Ok(Command::Check) => {
                    next = Instant::now();
                    attempts = 0;
                    force_check = true;
                }
                Ok(Command::Pause) => {
                    if let Some(current) = settings.as_mut() {
                        let paused = current.updates_paused;
                        current.updates_paused = !paused;
                        if let Err(e) = current.save(&state) {
                            current.updates_paused = paused;
                            status = format!("Cannot save update setting: {e:#}");
                        } else {
                            next = Instant::now();
                            status = if current.updates_paused {
                                "Automatic updates paused."
                            } else {
                                "Checking recorder updates..."
                            }
                            .into();
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            let emit = |status: &str, busy: bool| {
                let discovered = settings.as_ref().and_then(|s| s.discover().ok());
                sender.send(super::Message::Setup(Box::new(Snapshot {
                    root: settings.as_ref().map(|s| s.game_root.clone()),
                    combat: discovered.as_ref().and_then(|d| d.combat.clone()),
                    recorder: discovered.as_ref().and_then(|d| d.recorder.clone()),
                    status: status.into(),
                    busy,
                    paused: settings.as_ref().is_some_and(|s| s.updates_paused),
                })))
            };
            let should_check = settings.is_some()
                && (force_check
                    || settings.as_ref().is_some_and(|s| !s.updates_paused)
                        && (Instant::now() >= next
                            || waiting_for_game
                                && super::game_process::check()
                                    == super::game_process::State::Closed));
            if should_check {
                if emit("Checking and downloading recorder updates...", true).is_err() {
                    return;
                }
                let mut effective = settings.clone().unwrap();
                if force_check {
                    effective.updates_paused = false;
                }
                force_check = false;
                let result = ReleaseConfig::configured().and_then(|config| {
                    runtime.block_on(maintain_addon(&state, &effective, &config))
                });
                match result {
                    Ok(result) => {
                        attempts = 0;
                        waiting_for_game = result.status == "waiting-for-game";
                        status = if waiting_for_game {
                            "Recorder downloaded. Installation will finish automatically when the game closes.".into()
                        } else {
                            format!(
                                "Recorder {} installed. Updates are automatic.",
                                result.version
                            )
                        };
                        next = Instant::now() + Duration::from_secs(6 * 60 * 60);
                    }
                    Err(e) => {
                        waiting_for_game = false;
                        let delay = (30u64 * 2u64.pow(attempts.min(6))).min(1800)
                            + (chrono::Utc::now().timestamp().unsigned_abs() % 17);
                        attempts = attempts.saturating_add(1);
                        if azs_uploader_core::distribution::retryable(&e) && attempts <= 6 {
                            status = format!(
                                "{e:#}. Retrying automatically; use Check / repair to retry now."
                            );
                            next = Instant::now() + Duration::from_secs(delay);
                        } else {
                            status = format!(
                                "{e:#}. Setup or updates need attention. Use Check / repair after resolving the problem. Regular checks continue every six hours."
                            );
                            next = Instant::now() + Duration::from_secs(6 * 60 * 60);
                        }
                    }
                }
                if emit(&status, false).is_err() {
                    return;
                }
                let desktop = super::desktop_updates::prepare(&state).unwrap_or_else(|e| format!("Uploader update check failed: {e:#}. Current version remains available; checks retry automatically."));
                if sender.send(super::Message::DesktopStatus(desktop)).is_err() {
                    return;
                }
            }
            if emit(&status, false).is_err() {
                return;
            }
        }
    });
    tx
}
