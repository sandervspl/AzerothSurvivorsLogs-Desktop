use gpui::{App, Window};
use std::path::Path;

pub struct CloseBehavior {
    pub to_tray: Option<bool>,
    pub error: String,
    #[cfg(windows)]
    tray: Option<tray_icon::TrayIcon>,
}
impl CloseBehavior {
    pub fn load(state: &Path) -> Self {
        let to_tray = std::fs::read(state.join("desktop-preferences.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| value["closeToTray"].as_bool());
        #[cfg(windows)]
        let tray = create_tray();
        Self {
            to_tray,
            error: String::new(),
            #[cfg(windows)]
            tray,
        }
    }
    pub fn save(&mut self, state: &Path, to_tray: bool) -> bool {
        if to_tray && !self.available() {
            self.error = "Tray icon unavailable. The app will stay open.".into();
            return false;
        }
        match azs_uploader_core::distribution::atomic_json(
            &state.join("desktop-preferences.json"),
            &serde_json::json!({"closeToTray": to_tray}),
        ) {
            Ok(()) => {
                self.to_tray = Some(to_tray);
                self.error.clear();
                true
            }
            Err(error) => {
                self.error = format!("Could not save close preference: {error}");
                false
            }
        }
    }
    pub fn available(&self) -> bool {
        #[cfg(windows)]
        {
            self.tray.is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
}
#[cfg(windows)]
fn create_tray() -> Option<tray_icon::TrayIcon> {
    use tray_icon::{
        Icon, TrayIconBuilder,
        menu::{Menu, MenuItem},
    };
    let menu = Menu::new();
    menu.append(&MenuItem::with_id(
        "open-uploader",
        "Open uploader",
        true,
        None,
    ))
    .ok()?;
    menu.append(&MenuItem::with_id(
        "quit-uploader",
        "Quit uploader",
        true,
        None,
    ))
    .ok()?;
    let pixels = include_bytes!("../assets/tray.rgba").to_vec();
    TrayIconBuilder::new()
        .with_tooltip("Azeroth Survivors Recorder")
        .with_menu(Box::new(menu))
        .with_icon(Icon::from_rgba(pixels, 32, 32).ok()?)
        .build()
        .ok()
}
pub fn tray_action() -> Option<bool> {
    #[cfg(windows)]
    {
        use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent, menu::MenuEvent};
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id.as_ref() == "quit-uploader" {
                return Some(true);
            }
            if event.id.as_ref() == "open-uploader" {
                return Some(false);
            }
        }
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                return Some(false);
            }
        }
    }
    None
}
pub fn hide(window: &mut Window) {
    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use windows::Win32::{
            Foundation::HWND,
            UI::WindowsAndMessaging::{SW_HIDE, ShowWindow},
        };
        if let Ok(handle) = window.window_handle()
            && let RawWindowHandle::Win32(handle) = handle.as_raw()
        {
            unsafe {
                let _ = ShowWindow(HWND(handle.hwnd.get() as *mut _), SW_HIDE);
            }
        }
    }
    #[cfg(not(windows))]
    window.minimize_window();
}
pub fn restore(cx: &mut App) {
    cx.activate(true);
    for handle in cx.windows() {
        let _ = handle.update(cx, |_, window, _| {
            #[cfg(windows)]
            {
                use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                use windows::Win32::{
                    Foundation::HWND,
                    UI::WindowsAndMessaging::{SW_RESTORE, ShowWindow},
                };
                if let Ok(handle) = window.window_handle()
                    && let RawWindowHandle::Win32(handle) = handle.as_raw()
                {
                    unsafe {
                        let _ = ShowWindow(HWND(handle.hwnd.get() as *mut _), SW_RESTORE);
                    }
                }
            }
            window.activate_window();
        });
    }
}
