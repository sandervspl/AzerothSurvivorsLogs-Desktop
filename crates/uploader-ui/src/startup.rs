#[cfg(windows)]
fn entry() -> anyhow::Result<std::path::PathBuf> {
    Ok(std::path::PathBuf::from(
        std::env::var_os("APPDATA").ok_or_else(|| anyhow::anyhow!("APPDATA is unavailable"))?,
    )
    .join("Microsoft/Windows/Start Menu/Programs/Startup/AzerothSurvivorsUploader.cmd"))
}

pub fn enabled() -> bool {
    #[cfg(windows)]
    {
        entry().is_ok_and(|path| path.is_file())
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub fn set(enabled: bool) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        let path = entry()?;
        if enabled {
            let exe = std::env::current_exe()?;
            let exe = exe
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("Invalid executable path"))?;
            anyhow::ensure!(
                !exe.contains(['%', '\r', '\n', '"']),
                "Unsupported executable path"
            );
            let mut command = format!("\"{exe}\"");
            for arg in std::env::args().skip(1) {
                anyhow::ensure!(
                    !arg.contains(['%', '\r', '\n', '"']),
                    "Unsupported startup argument"
                );
                command.push_str(&format!(" \"{arg}\""));
            }
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(path, format!("@echo off\r\nstart \"\" {command}\r\n"))?;
        } else if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
        anyhow::bail!("Windows startup is only available on Windows")
    }
}
