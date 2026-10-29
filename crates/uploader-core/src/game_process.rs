use anyhow::Result;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum State {
    Checking,
    Running,
    Closed,
    Unknown,
}

impl State {
    pub fn permits_upload(self) -> bool {
        self == Self::Closed
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Checking => "Checking whether the game is open",
            Self::Running => "Exit the game before uploading",
            Self::Unknown => "Cannot check whether the game is open",
            Self::Closed => "Game closed",
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            Self::Checking => "Upload will be available after the game check finishes.",
            Self::Running => {
                "Exit Azeroth Survivors normally so your latest run is saved. Uploading unlocks automatically when the game closes."
            }
            Self::Unknown => {
                "Uploading is paused until the game check succeeds. The uploader will keep checking automatically."
            }
            Self::Closed => "",
        }
    }
}

pub fn check() -> State {
    match running() {
        Ok(true) => State::Running,
        Ok(false) => State::Closed,
        Err(_) => State::Unknown,
    }
}

pub fn ensure_closed() -> Result<()> {
    let state = check();
    anyhow::ensure!(
        state.permits_upload(),
        "{}. {}",
        state.title(),
        state.detail()
    );
    Ok(())
}

#[cfg(windows)]
fn running() -> Result<bool> {
    use std::mem::size_of;
    use windows::Win32::{
        Foundation::{CloseHandle, ERROR_NO_MORE_FILES, HANDLE},
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
    };
    struct Snapshot(HANDLE);
    impl Drop for Snapshot {
        fn drop(&mut self) {
            // This handle is owned by this snapshot and closed exactly once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
    // Toolhelp reads process names without opening game processes or reading memory.
    let snapshot = Snapshot(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)? });
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut result = unsafe { Process32FirstW(snapshot.0, &mut entry) };
    loop {
        if let Err(error) = result {
            if error.code() == ERROR_NO_MORE_FILES.to_hresult() {
                return Ok(false);
            }
            return Err(error.into());
        }
        let end = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
        if name.eq_ignore_ascii_case("AzerothSurvivors.exe") || name.eq_ignore_ascii_case("WoW.exe")
        {
            return Ok(true);
        }
        result = unsafe { Process32NextW(snapshot.0, &mut entry) };
    }
}

#[cfg(not(windows))]
fn running() -> Result<bool> {
    anyhow::bail!("Game process detection is only supported on Windows")
}
