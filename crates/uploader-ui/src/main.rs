#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
mod desktop_updates;
mod game_process;
mod setup;

use azs_uploader_core::{Engine, StageOptions, validate_api};
use chrono::Datelike;
use gpui::{div, prelude::*, px, rgb, *};
use std::{path::PathBuf, sync::mpsc, time::Duration};

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Upload,
    Results,
    Settings,
}

enum Message {
    DesktopStatus(String),
    Setup(Box<setup::Snapshot>),
    GameState(game_process::State),
    Status(String),
    SignedIn,
    Account(String),
    Pending { generation: u64, count: usize },
    Finished(Vec<String>),
}
struct Uploader {
    api: String,
    state: PathBuf,
    combat: Option<PathBuf>,
    recorder: Option<PathBuf>,
    status: String,
    busy: bool,
    game_state: game_process::State,
    signed_in: bool,
    reports: Vec<String>,
    tab: Tab,
    show_help: bool,
    unseen_results: bool,
    nav_focus: FocusHandle,
    pending_count: usize,
    owner: Option<String>,
    account_generation: u64,
    sender: mpsc::Sender<Message>,
    receiver: mpsc::Receiver<Message>,
    _poll: Task<()>,
    native_monitor: Option<azs_uploader_core::native_history::Monitor>,
    native_history_ready: bool,
    game_root: Option<PathBuf>,
    setup_status: String,
    setup_busy: bool,
    updates_paused: bool,
    setup_sender: mpsc::Sender<setup::Command>,
    native_source: Option<(PathBuf, Option<std::time::SystemTime>)>,
    manual_sources: bool,
    desktop_status: String,
    health_reported: bool,
}
fn credential(api: &str) -> anyhow::Result<keyring::Entry> {
    Ok(keyring::Entry::new(
        "AzerothSurvivorsRecorder",
        &azs_uploader_core::sha256(api.as_bytes()),
    )?)
}
impl Uploader {
    fn new(api: String, state: PathBuf, cx: &mut Context<Self>) -> Self {
        let combat = None;
        let recorder = None;
        let (sender, receiver) = mpsc::channel();
        let setup_sender = setup::start(state.clone(), sender.clone());
        let game_sender = sender.clone();
        std::thread::spawn(move || {
            let mut previous = None;
            loop {
                let state = game_process::check();
                if previous != Some(state) {
                    if game_sender.send(Message::GameState(state)).is_err() {
                        break;
                    }
                    previous = Some(state);
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        });
        let signed_in = credential(&api)
            .and_then(|c| c.get_password().map_err(Into::into))
            .is_ok();
        let poll = cx.spawn(async |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        let request = this.state.join("open-request");
                        if request.exists() {
                            let _ = std::fs::remove_file(request);
                            cx.activate(true);
                            for window in cx.windows() {
                                let _ = window.update(cx, |_, window, _| window.activate_window());
                            }
                        }
                        while let Ok(message) = this.receiver.try_recv() {
                            match message {
                                Message::DesktopStatus(status) => this.desktop_status = status,
                                Message::Setup(snapshot) => {
                                    this.setup_busy = snapshot.busy;
                                    this.updates_paused = snapshot.paused;
                                    this.setup_status = snapshot.status;
                                    this.game_root = snapshot.root;
                                    if !this.manual_sources {
                                        this.combat = snapshot.combat;
                                        this.recorder = snapshot.recorder;
                                    }
                                    let native = this.game_root.as_ref().and_then(|root| {
                                        azs_uploader_core::native_save::discover(Some(
                                            root.as_path(),
                                        ))
                                    });
                                    let tag = native.as_ref().map(|(_, exe)| {
                                        (
                                            exe.clone(),
                                            std::fs::metadata(exe).and_then(|m| m.modified()).ok(),
                                        )
                                    });
                                    if tag != this.native_source {
                                        this.native_source = tag;
                                        if let Some(old) = this.native_monitor.take() {
                                            std::thread::spawn(move || drop(old));
                                        }
                                        this.native_monitor = native.map(|(save, exe)| {
                                            azs_uploader_core::native_history::Monitor::start(
                                                this.state.clone(),
                                                save,
                                                exe,
                                            )
                                        });
                                    }
                                }
                                Message::GameState(state) => this.game_state = state,
                                Message::Status(text) => this.status = text,
                                Message::SignedIn => {
                                    this.signed_in = true;
                                    this.refresh_pending();
                                    this.busy = false;
                                    this.status =
                                        "Signed in. Your recordings are discovered automatically."
                                            .into();
                                }
                                Message::Account(owner) => this.owner = Some(owner),
                                Message::Pending { generation, count } => {
                                    if this.signed_in && generation == this.account_generation {
                                        this.pending_count = count;
                                    }
                                }
                                Message::Finished(reports) => {
                                    this.refresh_pending();
                                    this.reports = reports;
                                    this.busy = false;
                                    this.unseen_results =
                                        !this.reports.is_empty() && this.tab != Tab::Results;
                                }
                            }
                            cx.notify();
                        }
                        let ready = this
                            .native_monitor
                            .as_ref()
                            .is_some_and(|monitor| monitor.is_ready());
                        if ready != this.native_history_ready {
                            this.native_history_ready = ready;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let native_monitor = None;
        let mut uploader = Self {
            api,
            state,
            combat,
            recorder,
            status: "Your runs, ready for analysis.".into(),
            busy: false,
            game_state: game_process::State::Checking,
            signed_in,
            reports: vec![],
            tab: Tab::Upload,
            show_help: false,
            unseen_results: false,
            nav_focus: cx.focus_handle(),
            pending_count: 0,
            owner: None,
            account_generation: 0,
            sender,
            receiver,
            _poll: poll,
            native_monitor,
            native_history_ready: false,
            game_root: None,
            setup_status: "Choose your game directory to get started.".into(),
            setup_busy: false,
            updates_paused: false,
            setup_sender,
            native_source: None,
            manual_sources: false,
            desktop_status: "Uploader updates are checked automatically.".into(),
            health_reported: false,
        };
        uploader.refresh_pending();
        uploader
    }
    fn refresh_pending(&mut self) {
        if !self.signed_in {
            return;
        }
        let api = self.api.clone();
        let state = self.state.clone();
        let tx = self.sender.clone();
        self.account_generation += 1;
        let generation = self.account_generation;
        let owner = self.owner.clone();
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<usize> {
                let engine = Engine::new(state)?;
                let pending = engine.pending()?;
                if !pending.iter().any(|job| job.api == api) {
                    return Ok(0);
                }
                let owner = match owner {
                    Some(owner) => owner,
                    None => {
                        let token = credential(&api)?.get_password()?;
                        let runtime = tokio::runtime::Runtime::new()?;
                        runtime.block_on(engine.account(&api, &token))?
                    }
                };
                Ok(pending
                    .iter()
                    .filter(|job| job.api == api && job.owner == owner)
                    .count())
            })();
            if let Ok(count) = result {
                tx.send(Message::Pending { generation, count }).ok();
            }
        });
    }
    fn choose_game(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.setup_busy {
            return;
        }
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose the folder containing AzerothSurvivors.exe".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                let _ = this.update(cx, |this, cx| {
                    if let Some(path) = paths.first() {
                        this.manual_sources = false;
                        this.setup_busy = true;
                        this.setup_status =
                            "Validating game directory and downloading recorder...".into();
                        this.setup_sender
                            .send(setup::Command::Choose(path.clone()))
                            .ok();
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }
    fn choose(&mut self, recorder: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.setup_busy {
            return;
        }
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(if recorder {
                "Choose saved export from benilla-config/saved (not AddOns)".into()
            } else {
                "Choose WoWCombatLog.txt".into()
            }),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                let _ = this.update(cx, |this, cx| {
                    let Some(path) = paths.first().cloned() else {
                        return;
                    };
                    if !this.manual_sources {
                        this.combat = None;
                        this.recorder = None;
                    }
                    this.manual_sources = true;
                    if recorder {
                        this.recorder = Some(path);
                        if let Some((save, exe)) =
                            azs_uploader_core::native_save::discover(this.recorder.as_deref())
                        {
                            this.native_monitor =
                                Some(azs_uploader_core::native_history::Monitor::start(
                                    this.state.clone(),
                                    save,
                                    exe,
                                ));
                        }
                    } else {
                        this.combat = Some(path);
                    }
                    let saved = (|| -> anyhow::Result<()> {
                        std::fs::create_dir_all(&this.state)?;
                        std::fs::write(
                            this.state.join("recording-files.json"),
                            serde_json::to_vec_pretty(&serde_json::json!({
                                "combat": this.combat,
                                "recorder": this.recorder,
                            }))?,
                        )?;
                        Ok(())
                    })();
                    this.status = match saved {
                        Ok(()) => "Recording files remembered for next time.".into(),
                        Err(error) => {
                            format!("Files selected, but could not remember them: {error}")
                        }
                    };
                    cx.notify();
                });
            }
        })
        .detach();
    }
    fn logout(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let result = credential(&self.api).and_then(|entry| match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.into()),
        });
        match result {
            Ok(()) => {
                self.signed_in = false;
                self.pending_count = 0;
                self.owner = None;
                self.reports.clear();
                self.unseen_results = false;
                self.account_generation += 1;
                self.status =
                    "Logged out. Sign in through your browser to upload recordings.".into();
            }
            Err(error) => self.status = format!("Could not log out: {error}"),
        }
        cx.notify();
    }
    fn login(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = "Opening browser approval…".into();
        cx.notify();
        let api = self.api.clone();
        let state = self.state.clone();
        let tx = self.sender.clone();
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let runtime = tokio::runtime::Runtime::new()?;
                runtime.block_on(async {
                    let engine = Engine::new(state)?;
                    let code = engine.device_code(&api).await?;
                    let user_code = code["user_code"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("Missing approval code"))?;
                    tx.send(Message::Status(format!(
                        "Approve this device in your browser. Code: {user_code}"
                    )))
                    .ok();
                    let url = code["verification_uri_complete"]
                        .as_str()
                        .or(code["verification_uri"].as_str())
                        .ok_or_else(|| anyhow::anyhow!("Missing approval URL"))?;
                    open::that(url)?;
                    let token = engine
                        .authorize(
                            &api,
                            code["device_code"].as_str().unwrap_or(""),
                            code["interval"].as_u64().unwrap_or(5),
                            |s| {
                                tx.send(Message::Status(format!("{s} · {user_code}"))).ok();
                            },
                        )
                        .await?;
                    credential(&api)?.set_password(&token)?;
                    tx.send(Message::SignedIn).ok();
                    Ok(())
                })
            })();
            if let Err(error) = result {
                tx.send(Message::Status(error.to_string())).ok();
                tx.send(Message::Finished(vec![])).ok();
            }
        });
    }
    fn upload(&mut self, resume: bool, cx: &mut Context<Self>) {
        self.game_state = game_process::check();
        if !self.game_state.permits_upload() {
            self.tab = Tab::Upload;
            cx.notify();
            return;
        }
        if self.busy
            || self.setup_busy
            || !self.signed_in
            || (resume && self.pending_count == 0)
            || (!resume
                && (self.combat.as_ref().is_none_or(|path| !path.is_file())
                    || self.recorder.as_ref().is_some_and(|path| !path.is_file())
                    || !self.manual_sources && self.game_root.is_some() && self.recorder.is_none()))
        {
            return;
        }
        self.busy = true;
        self.reports.clear();
        self.tab = Tab::Upload;
        self.unseen_results = false;
        self.status = "Preparing a durable upload…".into();
        cx.notify();
        let api = self.api.clone();
        let state = self.state.clone();
        let combat = self.combat.clone();
        let recorder = self.recorder.clone();
        let tx = self.sender.clone();
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<(Vec<String>, Vec<String>)> {
                let runtime = tokio::runtime::Runtime::new()?;
                runtime.block_on(async {
                    game_process::ensure_closed()?;
                    let token = credential(&api)?.get_password()?;
                    let engine = Engine::new(state)?;
                    let owner = engine.account(&api, &token).await?;
                    tx.send(Message::Account(owner.clone())).ok();
                    let app_origin = engine.application_origin(&api).await?;
                    let native = azs_uploader_core::native_save::discover(
                        recorder.as_deref().or(combat.as_deref()),
                    );
                    let jobs = if resume {
                        engine
                            .pending()?
                            .into_iter()
                            .filter(|j| j.api == api && j.owner == owner)
                            .collect()
                    } else {
                        vec![
                            engine.stage(StageOptions {
                                api: &api,
                                owner: &owner,
                                combat: combat.as_ref().unwrap(),
                                recorder: recorder.as_deref(),
                                native_save: native
                                    .as_ref()
                                    .map(|(save, exe)| (save.as_path(), exe.as_path())),
                                year: chrono::Local::now().year(),
                                offset: chrono::Local::now().offset().local_minus_utc() / 60,
                                visibility: "public",
                            })?,
                        ]
                    };
                    let mut reports = vec![];
                    let mut failures = vec![];
                    for job in jobs {
                        if let Err(error) = game_process::ensure_closed() {
                            failures.push(error.to_string());
                            break;
                        }
                        let result = engine
                            .upload(&job, &token, |s| {
                                tx.send(Message::Status(s)).ok();
                            })
                            .await;
                        let result = match result {
                            Ok(result) => result,
                            Err(error) => {
                                failures.push(error.to_string());
                                continue;
                            }
                        };
                        if let Some(runs) = result["newRuns"].as_array() {
                            for run in runs {
                                if let Some(id) = run.as_str() {
                                    reports.push(format!("{app_origin}/runs/{id}"));
                                }
                            }
                        }
                    }
                    Ok((reports, failures))
                })
            })();
            match result {
                Ok((reports, failures)) => {
                    tx.send(Message::Status(if !failures.is_empty() {
                        format!("{} reports ready; {} uploads failed. {} Invalid recordings are kept locally but cannot be resumed; select corrected files and use Upload recording. Other unfinished uploads remain resumable.", reports.len(), failures.len(), failures.join("; "))
                    } else if reports.is_empty() {
                        "No new logs. This recording is already uploaded, or there is nothing pending.".into()
                    } else {
                        format!("{} new {} ready to view", reports.len(), if reports.len() == 1 { "report" } else { "reports" })
                    }))
                    .ok();
                    tx.send(Message::Finished(reports)).ok();
                }
                Err(error) => {
                    tx.send(Message::Status(format!(
                        "{error}. Any saved unfinished uploads are kept on this computer."
                    )))
                    .ok();
                    tx.send(Message::Finished(vec![])).ok();
                }
            }
        });
    }
}
// Keep these neutrals and primary accent in sync with apps/web/src/styles.css.
mod palette {
    pub const PAGE: u32 = 0x141414;
    pub const HEADER: u32 = 0x191919;
    pub const PANEL: u32 = 0x1f1f1f;
    pub const RAISED: u32 = 0x292929;
    pub const HOVER: u32 = 0x333333;
    pub const BORDER: u32 = 0x3e3e3e;
    pub const TEXT: u32 = 0xeeeeee;
    pub const MUTED: u32 = 0xaaaaaa;
    pub const ACCENT: u32 = 0xc32121;
    pub const ACCENT_HOVER: u32 = 0xdf2b2b;
    pub const LINK: u32 = 0xffcc66;
}
impl Render for Uploader {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.health_reported {
            self.health_reported = true;
            let state = self.state.clone();
            window.on_next_frame(move |_, _| {
                std::thread::spawn(move || {
                    let _ = azs_uploader::recovery::acknowledge(&state);
                });
            });
        }
        use palette::*;
        let button = |id: &'static str, label: &str, primary: bool, enabled: bool| {
            div()
                .id(id)
                .px_4()
                .py_2()
                .rounded_sm()
                .border_1()
                .border_color(rgb(if primary { ACCENT } else { BORDER }))
                .bg(rgb(if primary { ACCENT } else { RAISED }))
                .when(enabled, |s| {
                    s.cursor_pointer()
                        .hover(move |s| s.bg(rgb(if primary { ACCENT_HOVER } else { HOVER })))
                })
                .when(!enabled, |s| s.opacity(0.45))
                .child(label.to_string())
        };
        let source = |id: &'static str, title: &str, path: Option<&PathBuf>, fallback: &str| {
            let selected = path.is_some_and(|path| path.is_file());
            let missing = path.is_some() && !selected;
            let color = if selected {
                0x64d77a
            } else if missing {
                LINK
            } else {
                MUTED
            };
            let status = if selected {
                "Found"
            } else if missing {
                "Missing"
            } else {
                "Waiting"
            };
            let action = "Automatic";
            div()
                .id(id)
                .w_full()
                .flex()
                .items_center()
                .gap_3()
                .p_4()
                .when(id == "recorder", |s| {
                    s.border_t_1().border_color(rgb(BORDER))
                })
                .when(self.busy, |s| s.opacity(0.6))
                .child(
                    div()
                        .w_8()
                        .h_8()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_sm()
                        .bg(rgb(RAISED))
                        .text_color(rgb(color))
                        .child(if selected {
                            "\u{2713}"
                        } else if missing {
                            "!"
                        } else {
                            "\u{25cb}"
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(title.to_string()),
                                )
                                .child(div().text_xs().text_color(rgb(MUTED)).child(
                                    if id == "combat" {
                                        "Required"
                                    } else {
                                        "Run details"
                                    },
                                )),
                        )
                        .child(
                            div().text_xs().text_color(rgb(MUTED)).truncate().child(
                                path.and_then(|p| p.file_name())
                                    .map(|name| name.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| fallback.to_string()),
                            ),
                        ),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(rgb(color))
                        .border_1()
                        .border_color(rgb(color))
                        .rounded_full()
                        .px_2()
                        .py_1()
                        .child(status),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_xs()
                        .rounded_sm()
                        .px_3()
                        .py_2()
                        .child(action),
                )
        };
        let content = match self.tab {
            Tab::Upload => div().flex().flex_col().gap_5()
                .child(div().flex().flex_col().gap_2()
                    .child(div().text_2xl().child(if self.game_root.is_some() { "Your recordings, found automatically" } else { "Set up recording" }))
                    .child(div().text_color(rgb(MUTED)).child("Choose your game directory once. We'll download and maintain the recorder addon, and find your recordings.")))
                .child(div().p_4().bg(rgb(PANEL)).flex().flex_col().gap_3()
                    .child(div().text_xs().text_color(rgb(MUTED)).truncate().child(self.game_root.as_ref().map(|p| p.display().to_string()).unwrap_or("No game directory chosen".into())))
                    .child(button("choose-game", if self.game_root.is_some() { "Change game directory" } else { "Choose game directory" }, self.game_root.is_none(), !self.busy && !self.setup_busy)
                        .on_click(cx.listener(|this, _, window, cx| this.choose_game(window, cx))))
                    .child(div().text_xs().text_color(rgb(MUTED)).child(self.setup_status.clone())))
                .child(div().flex().flex_col().bg(rgb(PANEL)).border_1().border_color(rgb(BORDER)).rounded_sm().overflow_hidden()
                    .child(source("combat", "Combat log", self.combat.as_ref(), "Waiting for your first combat log"))
                    .child(source("recorder", "Recorder export", self.recorder.as_ref(), "Waiting for your first saved recording")))
                .child(div().id("upload-help").text_xs().text_color(rgb(MUTED)).cursor_pointer()
                    .child(if self.show_help { "Hide upload instructions" } else { "Before you upload >" })
                    .on_click(cx.listener(|this, _, _, cx| { this.show_help = !this.show_help; cx.notify(); })))
                .when(self.show_help, |s| s.child(div().text_xs().text_color(rgb(MUTED))
                    .child("Exit the game normally before uploading so your latest run is saved. Recording files are discovered automatically. Keep this app open while playing to record talent history. Older runs are matched only where that history covers their start.")))
                .when(self.signed_in && self.pending_count > 0, |s| s.child(
                    div().p_3().bg(rgb(PANEL)).flex().items_center().gap_3()
                        .child(div().flex_1().text_xs().text_color(rgb(MUTED)).child(format!("{} unfinished uploads saved on this computer.", self.pending_count)))
                        .child(button("retry", "Resume uploads", false, !self.busy && self.game_state.permits_upload())
                            .on_click(cx.listener(|this, _, _, cx| this.upload(true, cx))))))
                .into_any_element(),
            Tab::Results => div().flex().flex_col().gap_4()
                .child(div().text_2xl().child(if self.reports.is_empty() { "No reports yet".to_string() } else { format!("{} {} ready", self.reports.len(), if self.reports.len() == 1 { "report" } else { "reports" }) }))
                .child(div().text_color(rgb(MUTED)).child(if self.reports.is_empty() { "Upload a recording to see its new reports here." } else { "Only reports from your latest upload appear here." }))
                .when(!self.reports.is_empty(), |s| s.child(
                    div().flex().flex_col().bg(rgb(PANEL)).border_1().border_color(rgb(BORDER)).rounded_sm()
                        .children(self.reports.iter().enumerate().map(|(i, url)| {
                            let url = url.clone();
                            let identifier = url.rsplit('/').next().unwrap_or("").to_string();
                            div().id(("report", i)).flex_shrink_0().cursor_pointer().px_4().py_3().flex().items_center().gap_3()
                                .when(i > 0, |s| s.border_t_1().border_color(rgb(BORDER)))
                                .hover(|s| s.bg(rgb(RAISED)))
                                .child(div().text_xs().text_color(rgb(MUTED)).child(format!("{:02}", i + 1)))
                                .child(div().flex_1().min_w_0().flex().flex_col().gap_1()
                                    .child(div().font_weight(FontWeight::MEDIUM).child(format!("Run report {}", i + 1)))
                                    .child(div().text_xs().text_color(rgb(MUTED)).truncate().child(identifier)))
                                .child(div().text_xs().text_color(rgb(LINK)).child("View report >"))
                                .on_click(move |_, _, _| { let _ = open::that(&url); })
                        }))))
                .into_any_element(),
            Tab::Settings => div().flex().flex_col().gap_5()
                .child(div().text_2xl().child("Settings"))
                .child(div().flex().items_center().justify_between().gap_3().p_4().bg(rgb(PANEL))
                    .child(div().flex().flex_col().gap_1().child("Account")
                        .child(div().text_xs().text_color(rgb(MUTED)).child(if self.signed_in { "Connected through your browser" } else { "Sign in to upload private reports" })))
                    .child(button("login", if self.signed_in { "Log out" } else { "Sign in" }, false, !self.busy)
                        .on_click(cx.listener(|this, _, _, cx| { if this.signed_in { this.logout(cx); } else { this.login(cx); } }))))
                .child(div().p_4().bg(rgb(PANEL)).flex().flex_col().gap_2()
                    .child("Talent history")
                    .child(div().text_xs().text_color(rgb(if self.native_history_ready { 0x64d77a } else { MUTED }))
                        .child(if self.native_history_ready { "Recording. Keep this app open while playing." } else { "Waiting for a readable game save. Choose your game directory and keep this app open before starting runs." })))
                .child(div().p_4().bg(rgb(PANEL)).flex().flex_col().gap_2()
                    .child("Recorder updates")
                    .child(div().text_xs().text_color(rgb(MUTED)).child(self.setup_status.clone()))
                    .child(button("check-addon", "Check / repair", false, !self.setup_busy && !self.busy && self.game_root.is_some())
                        .on_click(cx.listener(|this, _, _, _| { if !this.setup_busy && !this.busy && this.game_root.is_some() { this.setup_sender.send(setup::Command::Check).ok(); } })))
                    .child(button("pause-updates", if self.updates_paused { "Resume automatic updates" } else { "Pause automatic updates" }, false, !self.setup_busy && !self.busy && self.game_root.is_some())
                        .on_click(cx.listener(|this, _, _, _| { if !this.setup_busy && !this.busy && this.game_root.is_some() { this.setup_sender.send(setup::Command::Pause).ok(); } }))))
                .child(div().p_4().bg(rgb(PANEL)).flex().flex_col().gap_2()
                    .child("Game directory")
                    .child(div().text_xs().text_color(rgb(MUTED)).truncate().child(self.game_root.as_ref().map(|p| p.display().to_string()).unwrap_or("Choose a game directory to begin".into())))
                    .child(button("settings-game", "Choose game directory", false, !self.busy && !self.setup_busy)
                        .on_click(cx.listener(|this, _, window, cx| this.choose_game(window, cx)))))
                .child(div().flex().gap_3()
                    .child(button("manual-combat", "Import combat log", false, !self.busy && !self.setup_busy)
                        .on_click(cx.listener(|this, _, window, cx| this.choose(false, window, cx))))
                    .child(button("manual-recorder", "Import recorder export", false, !self.busy && !self.setup_busy)
                        .on_click(cx.listener(|this, _, window, cx| this.choose(true, window, cx)))))
                .child(div().p_4().bg(rgb(PANEL)).flex().flex_col().gap_2()
                    .child("Uploader updates")
                    .child(div().text_xs().text_color(rgb(MUTED)).child(self.desktop_status.clone())))
                .child(div().text_xs().text_color(rgb(MUTED)).child("Closing this window minimizes the app and keeps recording in the background. Use Quit uploader to stop it."))
                .child(button("quit-uploader", "Quit uploader", false, !self.busy)
                    .on_click(cx.listener(|this, _, _, cx| { if !this.busy { azs_uploader::recovery::clean_exit(&this.state); cx.quit(); } })))
                .child(div().text_xs().text_color(rgb(MUTED)).child(format!("Server: {}", self.api)))
                .child(div().text_xs().text_color(rgb(MUTED)).child(self.status.clone()))
                .into_any_element(),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(PAGE))
            .text_color(rgb(TEXT))
            .font_family("Segoe UI")
            .text_sm()
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_6()
                    .py_4()
                    .bg(rgb(HEADER))
                    .border_t_3()
                    .border_color(rgb(ACCENT))
                    .child(
                        div()
                            .w_9()
                            .h_9()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(rgb(ACCENT))
                            .font_weight(FontWeight::BOLD)
                            .child("AS"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .font_weight(FontWeight::BOLD)
                                    .child("AZEROTH SURVIVORS"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child("COMBAT LOG UPLOADER"),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(if self.signed_in { 0x64d77a } else { MUTED }))
                            .child(if self.signed_in {
                                "Connected"
                            } else {
                                "Not signed in"
                            }),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .px_6()
                    .flex()
                    .gap_5()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .track_focus(&self.nav_focus)
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        let next = match (event.keystroke.key.as_str(), this.tab) {
                            ("right", Tab::Upload) | ("left", Tab::Settings) => Tab::Results,
                            ("right", Tab::Results) | ("left", Tab::Upload) | ("end", _) => {
                                Tab::Settings
                            }
                            ("left", Tab::Results) | ("right", Tab::Settings) | ("home", _) => {
                                Tab::Upload
                            }
                            _ => return,
                        };
                        this.tab = next;
                        if next == Tab::Results {
                            this.unseen_results = false;
                        }
                        cx.notify();
                    }))
                    .children(
                        [
                            (Tab::Upload, "Upload"),
                            (Tab::Results, "Results"),
                            (Tab::Settings, "Settings"),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(i, (tab, label))| {
                            div()
                                .id(("tab", i))
                                .cursor_pointer()
                                .py_3()
                                .border_b_2()
                                .border_color(rgb(if self.tab == tab { ACCENT } else { PAGE }))
                                .text_color(rgb(if self.tab == tab { TEXT } else { MUTED }))
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(label)
                                .when(tab == Tab::Results && !self.reports.is_empty(), |s| {
                                    s.child(
                                        div()
                                            .text_xs()
                                            .px_2()
                                            .rounded_full()
                                            .bg(rgb(RAISED))
                                            .text_color(rgb(if self.unseen_results {
                                                0x64d77a
                                            } else {
                                                MUTED
                                            }))
                                            .child(self.reports.len().to_string()),
                                    )
                                })
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.tab = tab;
                                    if tab == Tab::Results {
                                        this.unseen_results = false;
                                    }
                                    window.focus(&this.nav_focus);
                                    cx.notify();
                                }))
                        }),
                    ),
            )
            .child(
                div()
                    .id(match self.tab {
                        Tab::Upload => "upload-page",
                        Tab::Results => "results-page",
                        Tab::Settings => "settings-page",
                    })
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_6()
                    .child(content),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .px_6()
                    .py_4()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .when(self.tab == Tab::Upload, |s| {
                        s.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .p_3()
                                .bg(rgb(PANEL))
                                .border_1()
                                .border_color(rgb(if !self.game_state.permits_upload() {
                                    LINK
                                } else if self.reports.is_empty() {
                                    BORDER
                                } else {
                                    0x64d77a
                                }))
                                .rounded_sm()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(div().font_weight(FontWeight::SEMIBOLD).child(
                                            if !self.game_state.permits_upload() {
                                                self.game_state.title()
                                            } else if self.busy {
                                                "Upload in progress"
                                            } else if !self.reports.is_empty() {
                                                "Upload complete"
                                            } else {
                                                "Upload status"
                                            },
                                        ))
                                        .child(
                                            div()
                                                .id("upload-status-detail")
                                                .max_h(px(64.))
                                                .overflow_y_scroll()
                                                .text_xs()
                                                .text_color(rgb(MUTED))
                                                .child(if self.game_state.permits_upload() {
                                                    self.status.clone()
                                                } else {
                                                    self.game_state.detail().to_string()
                                                }),
                                        ),
                                )
                                .when(!self.reports.is_empty(), |s| {
                                    s.child(
                                        button(
                                            "view-results",
                                            &format!("View {} reports >", self.reports.len()),
                                            false,
                                            true,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.tab = Tab::Results;
                                                this.unseen_results = false;
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                }),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child("Private reports. Source files stay on this computer."),
                            )
                            .child(
                                button(
                                    "primary-action",
                                    if self.tab != Tab::Upload {
                                        "Back to upload"
                                    } else if !self.signed_in {
                                        "Sign in to upload"
                                    } else if !self.game_state.permits_upload() {
                                        "Waiting for game exit"
                                    } else if self.busy {
                                        "Working..."
                                    } else {
                                        "Upload recording"
                                    },
                                    true,
                                    self.tab != Tab::Upload
                                        || (!self.busy
                                            && !self.setup_busy
                                            && (!self.signed_in
                                                || (self.game_state.permits_upload()
                                                    && self
                                                        .combat
                                                        .as_ref()
                                                        .is_some_and(|p| p.is_file())
                                                    && self
                                                        .recorder
                                                        .as_ref()
                                                        .is_none_or(|p| p.is_file())
                                                    && (self.manual_sources
                                                        || self.game_root.is_none()
                                                        || self.recorder.is_some())))),
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        if this.tab != Tab::Upload {
                                            this.tab = Tab::Upload;
                                        } else if !this.signed_in {
                                            this.login(cx);
                                        } else {
                                            this.upload(false, cx);
                                        }
                                        cx.notify();
                                    },
                                )),
                            ),
                    ),
            )
    }
}
fn main() {
    velopack::VelopackApp::build()
        .set_auto_apply_on_startup(false)
        .run();
    let args: Vec<String> = std::env::args().collect();
    let value = |name: &str| {
        args.iter()
            .position(|v| v == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let api = value("--api").unwrap_or_else(|| {
        if cfg!(debug_assertions) {
            "http://127.0.0.1:8787".into()
        } else {
            "https://azslogs.com".into()
        }
    });
    if validate_api(&api).is_err() {
        eprintln!("Invalid API origin");
        return;
    }
    let state = value("--state").map(PathBuf::from).unwrap_or_else(|| {
        std::env::var_os("LOCALAPPDATA")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("AzerothSurvivorsRecorder")
    });
    if let Err(e) = std::fs::create_dir_all(&state) {
        eprintln!("Cannot create uploader state: {e}");
        return;
    }
    let _instance = match azs_uploader_core::distribution::lock(&state.join("application.lock")) {
        Ok(lock) => lock,
        Err(_) => {
            let _ = std::fs::write(state.join("open-request"), b"open");
            return;
        }
    };
    if let Err(error) = azs_uploader::recovery::startup(&state) {
        eprintln!("Update startup status: {error:#}");
    }
    let update_error = desktop_updates::apply_on_safe_launch(&state, &args[1..])
        .err()
        .map(|e| format!("Update deferred: {e:#}"));
    if let Some(error) = &update_error {
        let _ = azs_uploader_core::distribution::atomic_json(
            &state.join("desktop-update-status.json"),
            &serde_json::json!({"status":"deferred","detail":error}),
        );
    }
    Application::new().run(move |cx: &mut App| {
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(640.), px(720.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(520.), px(620.))),
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title("Azeroth Survivors Recorder");
                window.on_window_should_close(cx, |window, _| {
                    window.minimize_window();
                    false
                });
                cx.new(|cx| {
                    let mut uploader = Uploader::new(api, state, cx);
                    if let Some(error) = update_error {
                        uploader.desktop_status = error;
                    }
                    uploader
                })
            },
        )
        .expect("Cannot open uploader window");
        cx.activate(true);
    });
}
