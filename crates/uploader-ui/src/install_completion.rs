use gpui::{div, prelude::*, px, rgb, *};
use std::os::windows::process::CommandExt;

// Data travels in environment variables, never in PowerShell source.
const SHORTCUTS: &str = r#"
$ErrorActionPreference = 'Stop'
$shell = New-Object -ComObject WScript.Shell
foreach ($item in @(@('Programs', $env:AZS_INSTALL_START), @('DesktopDirectory', $env:AZS_INSTALL_DESKTOP))) {
    $folder = [Environment]::GetFolderPath($item[0])
    if (-not $folder) { throw 'Windows shortcut folder is unavailable.' }
    $path = Join-Path $folder ($env:AZS_INSTALL_TITLE + '.lnk')
    if ($item[1] -eq '1') {
        $link = $shell.CreateShortcut($path)
        $link.TargetPath = $env:AZS_INSTALL_EXE
        $link.WorkingDirectory = Split-Path -Parent $env:AZS_INSTALL_EXE
        $link.Description = $env:AZS_INSTALL_TITLE
        $link.Save()
    } elseif (Test-Path -LiteralPath $path) {
        $link = $shell.CreateShortcut($path)
        if ($link.TargetPath.StartsWith($env:AZS_INSTALL_ROOT, [StringComparison]::OrdinalIgnoreCase)) { Remove-Item -LiteralPath $path }
    }
}
"#;

struct Completion {
    start: bool,
    desktop: bool,
    launch: bool,
    error: Option<String>,
    busy: bool,
}

fn apply(start: bool, desktop: bool, launch: bool) -> anyhow::Result<()> {
    let locator = velopack::locator::auto_locate_app_manifest(
        velopack::locator::LocationContext::FromCurrentExe,
    )?;
    let exe = locator.get_main_exe_path();
    let result = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", SHORTCUTS])
        .env("AZS_INSTALL_EXE", &exe)
        .env(
            "AZS_INSTALL_ROOT",
            format!("{}\\", locator.get_root_dir().display()),
        )
        .env("AZS_INSTALL_TITLE", locator.get_manifest().title)
        .env("AZS_INSTALL_START", if start { "1" } else { "0" })
        .env("AZS_INSTALL_DESKTOP", if desktop { "1" } else { "0" })
        .creation_flags(0x08000000)
        .output()?;
    anyhow::ensure!(
        result.status.success(),
        "Windows could not save your shortcut choices. Check your folder permissions and try again."
    );
    if launch {
        std::process::Command::new(exe).spawn()?;
    }
    Ok(())
}

impl Completion {
    fn toggle(&mut self, index: usize) {
        match index {
            0 => self.start = !self.start,
            1 => self.desktop = !self.desktop,
            _ => self.launch = !self.launch,
        }
    }
    fn finish(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        let (start, desktop, launch) = (self.start, self.desktop, self.launch);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { apply(start, desktop, launch) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(()) => cx.quit(),
                    Err(error) => this.error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl Render for Completion {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut content = div()
            .size_full()
            .p_8()
            .flex()
            .flex_col()
            .gap_5()
            .bg(rgb(0x15171c))
            .text_color(rgb(0xf0f1f4))
            .child(div().text_2xl().child("Installation complete"))
            .child("Azeroth Survivors Uploader is ready to use.");
        for (index, label, checked) in [
            (0usize, "Create a Start menu shortcut", self.start),
            (1, "Create a desktop shortcut", self.desktop),
            (2, "Launch Azeroth Survivors Uploader", self.launch),
        ] {
            content = content.child(
                div()
                    .id(("install-choice", index))
                    .tab_index(0)
                    .cursor_pointer()
                    .p_2()
                    .rounded_sm()
                    .hover(|s| s.bg(rgb(0x292d36)))
                    .focus(|s| s.bg(rgb(0x343b4b)))
                    .child(format!(
                        "{}  {label}",
                        if checked { "\u{2611}" } else { "\u{2610}" }
                    ))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if !this.busy && matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.toggle(index);
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.busy {
                            this.toggle(index);
                            cx.notify();
                        }
                    })),
            );
        }
        if let Some(error) = &self.error {
            content = content.child(div().text_color(rgb(0xffa8a8)).child(error.clone()));
        }
        content.child(
            div()
                .id("install-finish")
                .tab_index(0)
                .p_3()
                .rounded_sm()
                .bg(rgb(0x3765d6))
                .focus(|s| s.bg(rgb(0x527fe9)))
                .cursor_pointer()
                .child(if self.busy { "Finishing..." } else { "Finish" })
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        this.finish(cx);
                    }
                }))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.finish(cx);
                })),
        )
    }
}

pub fn run() {
    Application::new().run(|cx: &mut App| {
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(540.), px(410.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(540.), px(410.))),
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title("Azeroth Survivors Uploader Setup");
                cx.new(|_| Completion {
                    start: true,
                    desktop: false,
                    launch: true,
                    error: None,
                    busy: false,
                })
            },
        )
        .expect("Cannot open installation completion window");
        cx.activate(true);
    });
}
