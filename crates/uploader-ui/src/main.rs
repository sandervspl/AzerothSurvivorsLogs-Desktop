#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
mod close_behavior;
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
    Approval { code: String, url: String },
    SignInFailed(String),
    SignedIn,
    CloseChoice(bool),
    SessionLoaded(bool),
    Account(String),
    Pending { generation: u64, count: usize },
    Finished(Vec<String>),
}
struct Uploader {
    close_behavior: close_behavior::CloseBehavior,
    close_prompt_open: bool,
    session_loaded: bool,
    setup_loaded: bool,
    wheel_animation: Option<(std::time::Instant, Pixels, Pixels)>,
    wheel_frame_pending: bool,
    page_scroll: ScrollHandle,
    scrollbar_drag: Option<Pixels>,
    upload_attempted: bool,
    visibility_bounds: std::rc::Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
    visibility_open: bool,
    visibility_focus: FocusHandle,
    public_upload: bool,
    approval: Option<(String, String)>,
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
    native_compatibility: u8,
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
        let session_api = api.clone();
        let session_sender = sender.clone();
        std::thread::spawn(move || {
            let signed_in = credential(&session_api)
                .and_then(|c| c.get_password().map_err(Into::into))
                .is_ok();
            let _ = session_sender.send(Message::SessionLoaded(signed_in));
        });
        let poll = cx.spawn(async |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        if let Some(quit) = close_behavior::tray_action() {
                            if quit {
                                azs_uploader::recovery::clean_exit(&this.state);
                                cx.quit();
                                return;
                            }
                            close_behavior::restore(cx);
                        }
                        let request = this.state.join("open-request");
                        if request.exists() {
                            let _ = std::fs::remove_file(request);
                            close_behavior::restore(cx);
                        }
                        while let Ok(message) = this.receiver.try_recv() {
                            match message {
                                Message::DesktopStatus(status) => this.desktop_status = status,
                                Message::Setup(snapshot) => {
                                    this.setup_loaded = true;
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
                                            azs_uploader_core::native_history::Monitor::start_with_api(
                                                this.state.clone(),
                                                save,
                                                exe,
                                                Some(this.api.clone()),
                                            )
                                        });
                                    }
                                }
                                Message::GameState(state) => this.game_state = state,
                                Message::Status(text) => this.status = text,
                                Message::Approval { code, url } => {
                                    this.approval = Some((code, url))
                                }
                                Message::SignInFailed(error) => {
                                    this.approval = None;
                                    this.busy = false;
                                    this.status = format!("Sign-in interrupted. {error}");
                                }
                                Message::SignedIn => {
                                    this.approval = None;
                                    this.signed_in = true;
                                    this.refresh_pending();
                                    this.busy = false;
                                    this.status = "Signed in.".into();
                                }
                                Message::CloseChoice(to_tray) => {
                                    this.close_prompt_open = false;
                                    if this.close_behavior.save(&this.state, to_tray) {
                                        if to_tray {
                                            for handle in cx.windows() {
                                                let _ = handle.update(cx, |_, window, _| {
                                                    close_behavior::hide(window)
                                                });
                                            }
                                        } else {
                                            azs_uploader::recovery::clean_exit(&this.state);
                                            cx.quit();
                                        }
                                    } else {
                                        this.status = this.close_behavior.error.clone();
                                    }
                                }
                                Message::SessionLoaded(signed_in) => {
                                    this.signed_in = signed_in;
                                    this.session_loaded = true;
                                    this.refresh_pending();
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
                        let compatibility = this.native_monitor.as_ref().map_or(0, |monitor| monitor.compatibility());
                        if compatibility != this.native_compatibility {
                            this.native_compatibility = compatibility;
                            cx.notify();
                        }
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
        Self {
            close_behavior: close_behavior::CloseBehavior::load(&state),
            close_prompt_open: false,
            session_loaded: false,
            setup_loaded: false,
            wheel_animation: None,
            wheel_frame_pending: false,
            page_scroll: ScrollHandle::new(),
            scrollbar_drag: None,
            upload_attempted: false,
            visibility_bounds: Default::default(),
            visibility_open: false,
            visibility_focus: cx.focus_handle(),
            public_upload: true,
            approval: None,
            api,
            state,
            combat,
            recorder,
            status: String::new(),
            busy: false,
            game_state: game_process::State::Checking,
            signed_in: false,
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
            native_compatibility: 0,
            game_root: None,
            setup_status: "Choose your game directory to get started.".into(),
            setup_busy: false,
            updates_paused: false,
            setup_sender,
            native_source: None,
            manual_sources: false,
            desktop_status: "Uploader updates are checked automatically.".into(),
            health_reported: false,
        }
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
                                Some(azs_uploader_core::native_history::Monitor::start_with_api(
                                    this.state.clone(),
                                    save,
                                    exe,
                                    Some(this.api.clone()),
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
    fn wheel_input(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = event.delta.pixel_delta(window.line_height()).y;
        let max = self.page_scroll.max_offset().height;
        let current = self.page_scroll.offset().y;
        let smooth = matches!(event.delta, ScrollDelta::Lines(lines) if lines.y.abs() >= 1. && lines.y.fract().abs() < 0.001)
            && motion_enabled();
        if !smooth {
            self.wheel_animation = None;
            self.page_scroll
                .set_offset(point(px(0.), (current + delta).clamp(-max, px(0.))));
        } else {
            let base = self
                .wheel_animation
                .filter(|(_, _, target)| {
                    (*target > current && delta > px(0.)) || (*target < current && delta < px(0.))
                })
                .map(|(_, _, target)| target)
                .unwrap_or(current);
            self.wheel_animation = Some((
                std::time::Instant::now(),
                current,
                (base + delta).clamp(-max, px(0.)),
            ));
            self.schedule_wheel_frame(window, cx);
        }
        cx.stop_propagation();
        cx.notify();
    }
    fn schedule_wheel_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.wheel_frame_pending {
            return;
        }
        self.wheel_frame_pending = true;
        let entity = cx.entity();
        window.on_next_frame(move |window, cx| {
            entity.update(cx, |this, cx| {
                this.wheel_frame_pending = false;
                if let Some((started, from, target)) = this.wheel_animation {
                    let t = (started.elapsed().as_secs_f32() / 0.14).min(1.);
                    let eased = 1. - (1. - t).powi(3);
                    let max = this.page_scroll.max_offset().height;
                    this.page_scroll.set_offset(point(
                        px(0.),
                        (from + (target - from) * eased).clamp(-max, px(0.)),
                    ));
                    if t >= 1. {
                        this.wheel_animation = None;
                    } else {
                        this.schedule_wheel_frame(window, cx);
                    }
                    cx.notify();
                }
            });
        });
        cx.notify();
    }
    fn page_scrollbar(&self, cx: &Context<Self>) -> AnyElement {
        let viewport = self.page_scroll.bounds();
        let height = viewport.size.height;
        let max = self.page_scroll.max_offset().height;
        if height <= px(0.) || max <= px(0.) {
            return div().into_any_element();
        }
        let thumb_height = (height * (height / (height + max)))
            .max(px(28.))
            .min(height);
        let travel = height - thumb_height;
        let top = travel * (-self.page_scroll.offset().y / max).clamp(0., 1.);
        let entity = cx.entity();
        let handle = self.page_scroll.clone();
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .right_0()
            .w(px(12.))
            .bg(rgb(palette::HEADER))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                this.wheel_input(event, window, cx)
            }))
            .child(
                div()
                    .absolute()
                    .top(top)
                    .left(px(3.))
                    .w(px(6.))
                    .h(thumb_height)
                    .rounded_full()
                    .bg(rgb(palette::MUTED)),
            )
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        window.on_mouse_event({
                            let entity = entity.clone();
                            let handle = handle.clone();
                            move |event: &MouseDownEvent, phase, _, cx| {
                                if phase != DispatchPhase::Bubble
                                    || event.button != MouseButton::Left
                                    || !bounds.contains(&event.position)
                                {
                                    return;
                                }
                                let y = event.position.y - bounds.origin.y;
                                let inside = y >= top && y <= top + thumb_height;
                                entity.update(cx, |this, cx| {
                                    this.wheel_animation = None;
                                    this.scrollbar_drag = inside.then_some(y - top);
                                    if !inside {
                                        let page = height * 0.9;
                                        let offset =
                                            handle.offset().y + if y < top { page } else { -page };
                                        handle
                                            .set_offset(point(px(0.), offset.clamp(-max, px(0.))));
                                    }
                                    cx.notify();
                                });
                            }
                        });
                        window.on_mouse_event({
                            let entity = entity.clone();
                            move |_: &MouseUpEvent, _, _, cx| {
                                entity.update(cx, |this, _| this.scrollbar_drag = None);
                            }
                        });
                        window.on_mouse_event(move |event: &MouseMoveEvent, _, _, cx| {
                            if !event.dragging() || travel <= px(0.) {
                                return;
                            }
                            entity.update(cx, |this, cx| {
                                if let Some(grab) = this.scrollbar_drag {
                                    handle.set_offset(point(
                                        px(0.),
                                        -max * ((event.position.y - bounds.origin.y - grab)
                                            / travel)
                                            .clamp(0., 1.),
                                    ));
                                    cx.notify();
                                }
                            });
                        });
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }
    fn login(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.approval = None;
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
                    let url = code["verification_uri_complete"]
                        .as_str()
                        .or(code["verification_uri"].as_str())
                        .ok_or_else(|| anyhow::anyhow!("Missing approval URL"))?;
                    tx.send(Message::Approval {
                        code: user_code.to_string(),
                        url: url.to_string(),
                    })
                    .ok();
                    tx.send(Message::Status("Waiting for browser approval".into()))
                        .ok();
                    if open::that(url).is_err() {
                        tx.send(Message::Status("Could not open your browser. Use Open browser to try again; approval is still waiting.".into())).ok();
                    }
                    let token = engine
                        .authorize(
                            &api,
                            code["device_code"].as_str().unwrap_or(""),
                            code["interval"].as_u64().unwrap_or(5),
                            |s| {
                                tx.send(Message::Status(s.to_string())).ok();
                            },
                        )
                        .await?;
                    credential(&api)?.set_password(&token)?;
                    tx.send(Message::SignedIn).ok();
                    Ok(())
                })
            })();
            if let Err(error) = result {
                tx.send(Message::SignInFailed(error.to_string())).ok();
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
        self.upload_attempted = true;
        self.status = "Preparing a durable upload…".into();
        cx.notify();
        let api = self.api.clone();
        let visibility = if self.public_upload {
            "public"
        } else {
            "private"
        };
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
                                visibility,
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
                    azs_uploader_core::monitoring::capture(&error);
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
fn readable_upload_status(raw: &str) -> String {
    if let Some(start) = raw.find('[') {
        let mut stream =
            serde_json::Deserializer::from_str(&raw[start..]).into_iter::<serde_json::Value>();
        if let Some(Ok(serde_json::Value::Array(issues))) = stream.next() {
            let messages: Vec<&str> = issues
                .iter()
                .filter_map(|issue| issue["message"].as_str())
                .collect();
            if !messages.is_empty() {
                let mut text = format!("{}\n\n{}", raw[..start].trim(), messages.join("\n\n"));
                if messages
                    .iter()
                    .any(|message| message.contains("unsupported executable"))
                {
                    text.push_str("\n\nThe server does not recognize this game build. Its native talent catalog needs to be refreshed before this recording can be processed.");
                }
                let tail = raw[start + stream.byte_offset()..].trim();
                if !tail.is_empty() {
                    text.push_str("\n\n");
                    text.push_str(tail);
                }
                return text;
            }
        }
    }
    raw.to_string()
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
fn visibility_globe() -> impl IntoElement {
    canvas(
        |_, _, _| (),
        |bounds, _, window, _| {
            let center = bounds.center();
            let mut builder = PathBuilder::stroke(px(1.3));
            for width in [6., 2.7] {
                for step in 0..=40 {
                    let angle = step as f32 * std::f32::consts::TAU / 40.;
                    let p = point(
                        center.x + px(width * angle.cos()),
                        center.y + px(6. * angle.sin()),
                    );
                    if step == 0 {
                        builder.move_to(p);
                    } else {
                        builder.line_to(p);
                    }
                }
            }
            builder.move_to(point(center.x - px(6.), center.y));
            builder.line_to(point(center.x + px(6.), center.y));
            if let Ok(path) = builder.build() {
                window.paint_path(path, rgb(palette::TEXT));
            }
        },
    )
    .w(px(16.))
    .h(px(16.))
    .flex_shrink_0()
}
fn motion_enabled() -> bool {
    #[cfg(target_os = "windows")]
    let animate = {
        use windows::Win32::UI::WindowsAndMessaging::{
            SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
        };
        let mut enabled = 0i32;
        unsafe {
            let _ = SystemParametersInfoW(
                SPI_GETCLIENTAREAANIMATION,
                0,
                Some((&mut enabled as *mut i32).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
        }
        enabled != 0
    };
    #[cfg(not(target_os = "windows"))]
    let animate = false;
    animate
}
fn approval_spinner() -> AnyElement {
    let animate = motion_enabled();
    div()
        .relative()
        .w(px(16.))
        .h(px(16.))
        .flex_shrink_0()
        .children((0usize..8).map(move |index| {
            let angle = index as f32 * std::f32::consts::TAU / 8.;
            let dot = div()
                .absolute()
                .left(px(6.5 + angle.sin() * 6.))
                .top(px(6.5 - angle.cos() * 6.))
                .w(px(3.))
                .h(px(3.))
                .rounded_full()
                .bg(rgb(palette::MUTED));
            if animate {
                dot.with_animation(
                    ("approval-spinner", index),
                    Animation::new(Duration::from_secs(1)).repeat(),
                    move |dot, progress| {
                        dot.opacity(
                            0.2 + 0.8 * (1. - (progress - index as f32 / 8.).rem_euclid(1.)),
                        )
                    },
                )
                .into_any_element()
            } else {
                dot.opacity(0.3 + index as f32 / 12.).into_any_element()
            }
        }))
        .into_any_element()
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
        let scroll_entity = cx.entity();
        let scroll_bounds = self.page_scroll.clone();
        let wheel_capture = canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
                    if phase == DispatchPhase::Capture
                        && scroll_bounds.bounds().contains(&event.position)
                        && !event.modifiers.shift
                        && event.delta.pixel_delta(window.line_height()).y != px(0.)
                    {
                        scroll_entity.update(cx, |this, cx| this.wheel_input(event, window, cx));
                    }
                });
            },
        )
        .absolute()
        .size_full();
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
        let loading = !self.session_loaded || !self.setup_loaded;
        let onboarding = loading || self.game_root.is_none() || !self.signed_in;
        let content = if loading {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .gap_3()
                .child(approval_spinner())
                .child("Loading uploader...")
                .into_any_element()
        } else if onboarding {
            let choose_folder = self.game_root.is_none();
            div()
                .flex()
                .flex_col()
                .gap_5()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child(if choose_folder {
                            "Step 1 of 2: Game folder"
                        } else {
                            "Step 2 of 2: Sign in"
                        }),
                )
                .child(div().text_2xl().child(if choose_folder {
                    "Choose your game folder"
                } else if self.approval.is_some() {
                    "Enter this code in your browser"
                } else {
                    "Sign in to continue"
                }))
                .child(div().text_color(rgb(MUTED)).child(if choose_folder {
                    "Select your Azeroth Survivors installation."
                } else {
                    "Continue in your browser."
                }))
                .when(!choose_folder && self.approval.is_some(), |s| {
                    let (code, url) = self.approval.as_ref().unwrap();
                    let copy = code.clone();
                    let url = url.clone();
                    let keyboard_url = url.clone();
                    s.child(
                        div()
                            .p_6()
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .rounded_sm()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap_4()
                            .child(div().text_color(rgb(MUTED)).child("Your approval code"))
                            .child(
                                div()
                                    .text_size(px(52.))
                                    .font_weight(FontWeight::BOLD)
                                    .child(code.clone()),
                            )
                            .child(button("copy-approval", "Copy code", false, true).on_click(
                                move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                                },
                            ))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child(approval_spinner())
                                    .child(self.status.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .justify_center()
                            .items_center()
                            .gap_1()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child("Browser didn't open? Click")
                            .child(
                                div()
                                    .id("reopen-approval")
                                    .track_focus(&self.nav_focus)
                                    .on_key_down(move |event: &KeyDownEvent, _, _| {
                                        if event.keystroke.key == "enter"
                                            || event.keystroke.key == "space"
                                        {
                                            let _ = open::that(&keyboard_url);
                                        }
                                    })
                                    .cursor_pointer()
                                    .text_color(rgb(TEXT))
                                    .underline()
                                    .child("Open browser")
                                    .on_click(move |_, _, _| {
                                        let _ = open::that(&url);
                                    }),
                            )
                            .child("to continue."),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .text_center()
                            .child("Keep this app open while you approve."),
                    )
                })
                .when(self.approval.is_none() || !self.busy, |s| {
                    s.child(
                        button(
                            "setup-next",
                            if choose_folder {
                                "Choose game directory"
                            } else if self.status.starts_with("Sign-in interrupted.") {
                                "Try again"
                            } else {
                                "Sign in"
                            },
                            true,
                            !self.busy && !self.setup_busy,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            if this.game_root.is_none() {
                                this.choose_game(window, cx);
                            } else {
                                this.login(cx);
                            }
                        })),
                    )
                })
                .when(
                    choose_folder || (self.approval.is_none() && !self.status.is_empty()),
                    |s| {
                        s.child(
                            div()
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .child(if choose_folder {
                                    self.setup_status.clone()
                                } else {
                                    self.status.clone()
                                }),
                        )
                    },
                )
                .into_any_element()
        } else {
            match self.tab {
            Tab::Upload => div().w_full().min_w_0().flex().flex_col().gap_5()
                .child(div().flex().flex_col().gap_2()
                    .child(div().text_2xl().child("Upload"))
                    .child(div().text_color(rgb(MUTED)).child("Exit the game before uploading.")))
                .child(div().flex().flex_col().bg(rgb(PANEL)).border_1().border_color(rgb(BORDER)).rounded_sm().overflow_hidden()
                    .child(source("combat", "Combat log", self.combat.as_ref(), "No combat log found"))
                    .child(source("recorder", "Recorder export", self.recorder.as_ref(), "No recorder export found")))
                .child(div().p_4().bg(rgb(PANEL)).flex().flex_col().gap_2()
                    .child("Build history")
                    .child(div().text_xs().text_color(rgb(if self.native_history_ready { 0x64d77a } else { MUTED }))
                        .child(if self.native_history_ready { match self.native_compatibility {
                            1 => "Recording. Talent reports supported. Keep this app open while playing.",
                            2 => "Recording locally. This game build needs talent verification on the server. Keep this app open; saved uploads can be recovered after support is added.",
                            _ => "Recording locally. Server talent support has not been confirmed. Keep this app open while playing.",
                        } } else { "Waiting for a readable game save." })))
                .child(div().id("upload-help").text_xs().text_color(rgb(MUTED)).cursor_pointer()
                    .child(if self.show_help { "Hide upload instructions" } else { "Before you upload >" })
                    .on_click(cx.listener(|this, _, _, cx| { this.show_help = !this.show_help; cx.notify(); })))
                .when(self.show_help, |s| s.child(div().w_full().min_w_0().whitespace_normal().text_xs().text_color(rgb(MUTED))
                    .child("Exit the game normally before uploading so your latest run is saved. Recording files are discovered automatically. Keep this app open while playing to record talent and equipment history. Older runs are matched only where that history covers their start. Uploaded originals are deleted after 30 days; generated reports remain available.")))
                .when(self.signed_in && self.pending_count > 0, |s| s.child(
                    div().p_3().bg(rgb(PANEL)).flex().items_center().gap_3()
                        .child(div().flex_1().text_xs().text_color(rgb(MUTED)).child(format!("{} unfinished uploads saved on this computer.", self.pending_count)))
                        .child(button("retry", "Resume uploads", false, !self.busy && self.game_state.permits_upload())
                            .on_click(cx.listener(|this, _, _, cx| this.upload(true, cx))))))
                .into_any_element(),
            Tab::Results => div().flex().flex_col().gap_4()
                .child(div().text_2xl().child(if self.reports.is_empty() { "Reports".to_string() } else { format!("{} {}", self.reports.len(), if self.reports.len() == 1 { "report" } else { "reports" }) }))
                .child(div().text_color(rgb(MUTED)).child(if self.reports.is_empty() { "No uploaded reports." } else { "Reports from the latest upload." }))
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
                    .child("Recorder updates")
                    .child(div().text_xs().text_color(rgb(MUTED)).child(self.setup_status.clone()))
                    .child(button("check-addon", "Check / repair", false, !self.setup_busy && !self.busy && self.game_root.is_some())
                        .on_click(cx.listener(|this, _, _, _| { if !this.setup_busy && !this.busy && this.game_root.is_some() { this.setup_sender.send(setup::Command::Check).ok(); } })))
                    .child(button("pause-updates", if self.updates_paused { "Resume automatic updates" } else { "Pause automatic updates" }, false, !self.setup_busy && !self.busy && self.game_root.is_some())
                        .on_click(cx.listener(|this, _, _, _| { if !this.setup_busy && !this.busy && this.game_root.is_some() { this.setup_sender.send(setup::Command::Pause).ok(); } }))))
                .child(div().p_4().bg(rgb(PANEL)).flex().flex_col().gap_2()
                    .child("Game directory")
                    .child(div().text_xs().text_color(rgb(MUTED)).truncate().child(self.game_root.as_ref().map(|p| p.display().to_string()).unwrap_or("Choose a game directory to begin".into())))
                    .child(button("settings-game", "Change game directory", false, !self.busy && !self.setup_busy)
                        .on_click(cx.listener(|this, _, window, cx| this.choose_game(window, cx)))))
                .child(div().flex().gap_3()
                    .child(button("manual-combat", "Import combat log", false, !self.busy && !self.setup_busy)
                        .on_click(cx.listener(|this, _, window, cx| this.choose(false, window, cx))))
                    .child(button("manual-recorder", "Import recorder export", false, !self.busy && !self.setup_busy)
                        .on_click(cx.listener(|this, _, window, cx| this.choose(true, window, cx)))))
                .child(div().p_4().bg(rgb(PANEL)).flex().flex_col().gap_2()
                    .child("Uploader updates")
                    .child(div().text_xs().text_color(rgb(MUTED)).child(self.desktop_status.clone())))
                .child(div().p_4().bg(rgb(PANEL)).flex().flex_col().gap_2()
                    .child("When closing the window")
                    .child(div().flex().gap_2()
                        .child(button("close-exit", "Exit app", self.close_behavior.to_tray == Some(false), true).on_click(cx.listener(|this, _, _, cx| { this.close_behavior.save(&this.state, false); cx.notify(); })))
                        .child(button("close-tray", "Minimize to tray", self.close_behavior.to_tray == Some(true), self.close_behavior.available()).on_click(cx.listener(|this, _, _, cx| { this.close_behavior.save(&this.state, true); cx.notify(); }))))
                    .child(div().text_xs().text_color(rgb(MUTED)).child("Tray mode keeps recording in the background. Use the tray icon to reopen or quit."))
                    .when(!self.close_behavior.error.is_empty(), |s| s.child(div().text_color(rgb(LINK)).child(self.close_behavior.error.clone()))))
                .child(button("quit-uploader", "Quit uploader", false, !self.busy)
                    .on_click(cx.listener(|this, _, _, cx| { if !this.busy { azs_uploader::recovery::clean_exit(&this.state); cx.quit(); } })))
                .child(div().text_xs().text_color(rgb(MUTED)).child(format!("Server: {}", self.api)))
                .child(div().text_xs().text_color(rgb(MUTED)).child(self.status.clone()))
                .into_any_element(),
        }
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
                        img(std::sync::Arc::new(Image::from_bytes(
                            ImageFormat::Png,
                            include_bytes!("../assets/app.png").to_vec(),
                        )))
                        .w_9()
                        .h_9(),
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
                            .child(if loading {
                                "Loading..."
                            } else if self.signed_in {
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
                    .when(onboarding, |s| s.hidden())
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
                        this.wheel_animation = None;
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
                                    this.wheel_animation = None;
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
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .id(match self.tab {
                                Tab::Upload => "upload-page",
                                Tab::Results => "results-page",
                                Tab::Settings => "settings-page",
                            })
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .overflow_y_scroll()
                            .track_scroll(&self.page_scroll)
                            .p_6()
                            .child(content),
                    )
                    .child(wheel_capture)
                    .child(self.page_scrollbar(cx)),
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
                    .when(onboarding || self.tab != Tab::Upload, |s| s.hidden())
                    .when(
                        self.upload_attempted && (self.busy || self.reports.is_empty()),
                        |s| {
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
                                                    .max_h(px(240.))
                                                    .overflow_y_scroll()
                                                    .text_sm()
                                                    .text_color(rgb(TEXT))
                                                    .child(if self.game_state.permits_upload() {
                                                        readable_upload_status(&self.status)
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
                        },
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div().flex_1().child(
                                    div()
                                        .id("visibility-menu")
                                        .relative()
                                        .track_focus(&self.visibility_focus)
                                        .on_mouse_down_out(cx.listener(
                                            |this, event: &MouseDownEvent, _, cx| {
                                                if this.visibility_open
                                                    && !this.visibility_bounds.get().is_some_and(
                                                        |bounds| bounds.contains(&event.position),
                                                    )
                                                {
                                                    this.visibility_open = false;
                                                    cx.notify();
                                                }
                                            },
                                        ))
                                        .on_key_down(cx.listener(
                                            |this, event: &KeyDownEvent, _, cx| {
                                                if this.busy {
                                                    return;
                                                }
                                                match event.keystroke.key.as_str() {
                                                    "escape" => this.visibility_open = false,
                                                    "enter" | "space" => {
                                                        this.visibility_open = !this.visibility_open
                                                    }
                                                    "up" | "down" if this.visibility_open => {
                                                        this.public_upload = !this.public_upload
                                                    }
                                                    _ => return,
                                                }
                                                cx.notify();
                                            },
                                        ))
                                        .child(
                                            div()
                                                .id("visibility-trigger")
                                                .flex()
                                                .items_center()
                                                .gap_2()
                                                .py_2()
                                                .px_2()
                                                .rounded_sm()
                                                .when(!self.busy, |s| {
                                                    s.cursor_pointer().hover(|s| s.bg(rgb(RAISED)))
                                                })
                                                .when(self.busy, |s| s.opacity(0.45))
                                                .child(visibility_globe())
                                                .child(if self.public_upload {
                                                    "Public upload"
                                                } else {
                                                    "Private upload"
                                                })
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(rgb(MUTED))
                                                        .child("\u{25be}"),
                                                )
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    if !this.busy {
                                                        this.visibility_open =
                                                            !this.visibility_open;
                                                        window.focus(&this.visibility_focus);
                                                        cx.notify();
                                                    }
                                                })),
                                        )
                                        .when(self.visibility_open && !self.busy, |s| {
                                            let popup_bounds = self.visibility_bounds.clone();
                                            s.child(
                                                div()
                                                    .absolute()
                                                    .bottom(px(42.))
                                                    .left_0()
                                                    .w(px(190.))
                                                    .p_1()
                                                    .bg(rgb(PANEL))
                                                    .border_1()
                                                    .border_color(rgb(BORDER))
                                                    .rounded_sm()
                                                    .shadow_md()
                                                    .child(
                                                        canvas(
                                                            |_, _, _| (),
                                                            move |bounds, _, _, _| {
                                                                popup_bounds.set(Some(bounds))
                                                            },
                                                        )
                                                        .absolute()
                                                        .size_full(),
                                                    )
                                                    .children(
                                                        [
                                                            (false, "Private upload"),
                                                            (true, "Public upload"),
                                                        ]
                                                        .into_iter()
                                                        .enumerate()
                                                        .map(|(index, (public, label))| {
                                                            div()
                                                                .id(("visibility-option", index))
                                                                .px_3()
                                                                .py_2()
                                                                .rounded_sm()
                                                                .cursor_pointer()
                                                                .bg(rgb(
                                                                    if self.public_upload == public
                                                                    {
                                                                        RAISED
                                                                    } else {
                                                                        PANEL
                                                                    },
                                                                ))
                                                                .hover(|s| s.bg(rgb(HOVER)))
                                                                .child(label)
                                                                .on_click(cx.listener(
                                                                    move |this, _, _, cx| {
                                                                        if !this.busy {
                                                                            this.public_upload =
                                                                                public;
                                                                            this.visibility_open =
                                                                                false;
                                                                            cx.notify();
                                                                        }
                                                                    },
                                                                ))
                                                        }),
                                                    ),
                                            )
                                        }),
                                ),
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
    let _sentry = azs_uploader_core::monitoring::init();
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
                let uploader = cx.new(|cx| {
                    let mut uploader = Uploader::new(api, state, cx);
                    if let Some(error) = update_error {
                        uploader.desktop_status = error;
                    }
                    uploader
                });
                let close_state = uploader.clone();
                window.on_window_should_close(cx, move |window, cx| {
                    close_state.update(cx, |this, cx| {
                        match this.close_behavior.to_tray {
                            Some(false) => { azs_uploader::recovery::clean_exit(&this.state); true }
                            Some(true) if this.close_behavior.available() => { close_behavior::hide(window); false }
                            _ => {
                                if !this.close_prompt_open {
                                    this.close_prompt_open = true;
                                    let answer = window.prompt(PromptLevel::Info, "What should closing the window do?", Some("Exit stops recording. Tray mode keeps recording in the background. You can change this in Settings."), &["Exit app", "Minimize to tray", "Cancel"], cx);
                                    cx.spawn(async move |this, cx| {
                                        if let Ok(choice) = answer.await {
                                            let _ = this.update(cx, |this, cx| {
                                                if choice < 2 { let _ = this.sender.send(Message::CloseChoice(choice == 1)); }
                                                else { this.close_prompt_open = false; }
                                                cx.notify();
                                            });
                                        } else { let _ = this.update(cx, |this, cx| { this.close_prompt_open = false; cx.notify(); }); }
                                    }).detach();
                                }
                                false
                            }
                        }
                    })
                });
                uploader
            },
        )
        .expect("Cannot open uploader window");
        cx.activate(true);
    });
}
