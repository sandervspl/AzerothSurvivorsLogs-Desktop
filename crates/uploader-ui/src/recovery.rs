use anyhow::{Context, Result, ensure};
use azs_uploader_core::distribution::{atomic_json, lock};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
use velopack::locator::{LocationContext, auto_locate_app_manifest};

pub const PACKAGE_ID: &str = if cfg!(debug_assertions) {
    "AZSLogsUploaderDevelopment"
} else {
    "AZSLogsUploader"
};

#[derive(Clone, Serialize, Deserialize)]
pub struct Attempt {
    id: String,
    state: PathBuf,
    root: PathBuf,
    parent: u32,
    old_version: String,
    new_version: String,
    previous: PathBuf,
    previous_hash: String,
    previous_size: u64,
    restart_args: Vec<String>,
}
#[derive(Serialize, Deserialize)]
struct Health {
    id: String,
    version: String,
    pid: u32,
    ready: bool,
    #[serde(default)]
    clean_exit: bool,
}

pub fn fingerprint(path: &Path) -> Result<(String, u64)> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    ensure!(size <= 512 * 1024 * 1024, "Recovery package exceeds limits");
    let mut hasher = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok((format!("{:x}", hasher.finalize()), size))
}
pub fn blocked(state: &Path, version: &str) -> bool {
    fs::read(state.join("blocked-desktop.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .is_some_and(|v| v["version"] == version)
}
fn hidden(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    #[cfg(not(windows))]
    {
        let _ = command;
    }
}
pub fn arm(state: &Path, new_version: &str, restart_args: &[String]) -> Result<PathBuf> {
    drop(
        lock(&state.join("guard.lock"))
            .context("An earlier update is still recovering; current version retained")?,
    );
    let locator = auto_locate_app_manifest(LocationContext::FromCurrentExe)?;
    let recovery = state.join("desktop-recovery");
    fs::create_dir_all(&recovery)?;
    let previous = velopack::locator::find_local_full_packages(&locator.get_packages_dir())
        .into_iter()
        .find(|(_, manifest)| {
            manifest.id == PACKAGE_ID && manifest.version == locator.get_manifest_version()
        })
        .map(|(path, _)| path);
    let old_version = locator.get_manifest_version_full_string();
    let previous = match previous {
        Some(path) => path,
        None => snapshot_current(&locator.get_current_bin_dir(), &recovery)?,
    };
    let (previous_hash, previous_size) = fingerprint(&previous)
        .context("Previous desktop package is unavailable; update deferred to preserve recovery")?;
    let saved = recovery.join(format!("{previous_hash}-full.nupkg"));
    if fingerprint(&saved).ok().as_ref() != Some(&(previous_hash.clone(), previous_size)) {
        fs::copy(&previous, &saved)?;
    }
    ensure!(
        fingerprint(&saved)? == (previous_hash.clone(), previous_size),
        "Cannot preserve the previous desktop package"
    );
    let guard_source = std::env::current_exe()?.with_file_name("azs-update-guard.exe");
    let (guard_hash, _) = fingerprint(&guard_source)?;
    let guard = recovery.join(format!("guard-{guard_hash}.exe"));
    if !guard.exists() {
        fs::copy(&guard_source, &guard)?;
    }
    ensure!(
        fingerprint(&guard)?.0 == guard_hash,
        "Recovery helper integrity check failed"
    );
    let id = azs_uploader_core::sha256(
        format!(
            "{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        )
        .as_bytes(),
    );
    let attempt = Attempt {
        id,
        state: state.to_path_buf(),
        root: locator.get_root_dir(),
        parent: std::process::id(),
        old_version,
        new_version: new_version.into(),
        previous: saved,
        previous_hash,
        previous_size,
        restart_args: restart_args.into(),
    };
    let plan = state.join("desktop-attempt.json");
    atomic_json(&plan, &attempt)?;
    let mut command = Command::new(&guard);
    command.arg(&plan);
    hidden(&mut command);
    command
        .spawn()
        .context("Could not start recovery helper; update deferred")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let ready = fs::read_to_string(state.join("guard-ready")).unwrap_or_default();
        if ready == attempt.id {
            return Ok(plan);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::bail!("Recovery helper did not start; current version retained")
}
fn snapshot_current(current: &Path, recovery: &Path) -> Result<PathBuf> {
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};
    let destination = recovery.join("installed-snapshot-full.nupkg");
    let temporary = recovery.join("installed-snapshot.partial");
    let mut zip = ZipWriter::new(File::create(&temporary)?);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let manifest = fs::read(current.join("sq.version"))?;
    zip.start_file(format!("{PACKAGE_ID}.nuspec"), options)?;
    zip.write_all(&manifest)?;
    zip.start_file("[Content_Types].xml", options)?;
    zip.write_all(b"<?xml version=\"1.0\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"exe\" ContentType=\"application/octet-stream\"/><Default Extension=\"version\" ContentType=\"text/xml\"/><Default Extension=\"nuspec\" ContentType=\"text/xml\"/></Types>")?;
    let mut total = 0;
    let entries = fs::read_dir(current)?.collect::<std::io::Result<Vec<_>>>()?;
    ensure!(
        entries.len() <= 64,
        "Unexpected installed files; automatic recovery snapshot is unavailable"
    );
    for entry in entries {
        let metadata = fs::symlink_metadata(entry.path())?;
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                metadata.file_attributes() & 0x400 == 0,
                "Cannot snapshot redirected application files"
            );
        }
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Cannot snapshot unexpected application subdirectories"
        );
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Unsupported installed filename"))?;
        ensure!(
            name.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b" ._-".contains(&b)),
            "Unsafe installed filename"
        );
        total += metadata.len();
        ensure!(
            total <= 512 * 1024 * 1024,
            "Installed recovery snapshot exceeds size limits"
        );
        zip.start_file(format!("lib/app/{name}"), options)?;
        std::io::copy(&mut File::open(entry.path())?, &mut zip)?;
        let after = fs::metadata(entry.path())?;
        ensure!(
            after.len() == metadata.len() && after.modified()? == metadata.modified()?,
            "Installed application changed during snapshot"
        );
    }
    zip.finish()?.sync_all()?;
    fs::rename(&temporary, &destination)?;
    Ok(destination)
}
fn attempt(state: &Path) -> Result<Option<Attempt>> {
    match fs::read(state.join("desktop-attempt.json")) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
pub fn cancel(state: &Path) {
    if let Ok(Some(plan)) = attempt(state) {
        let _ = fs::write(state.join("guard-cancel"), plan.id);
    }
}
pub fn startup(state: &Path) -> Result<()> {
    let Some(plan) = attempt(state)? else {
        return Ok(());
    };
    let locator = auto_locate_app_manifest(LocationContext::FromCurrentExe)?;
    let version = locator.get_manifest_version_full_string();
    if version != plan.new_version {
        return Ok(());
    }
    atomic_json(
        &state.join("desktop-health.json"),
        &Health {
            id: plan.id,
            version: version.clone(),
            pid: std::process::id(),
            ready: false,
            clean_exit: false,
        },
    )?;
    if cfg!(debug_assertions)
        && std::env::var("AZS_DESKTOP_FAIL_VERSION").ok().as_deref() == Some(version.as_str())
    {
        std::process::exit(87);
    }
    Ok(())
}
pub fn acknowledge(state: &Path) -> Result<()> {
    let Some(plan) = attempt(state)? else {
        return Ok(());
    };
    let locator = auto_locate_app_manifest(LocationContext::FromCurrentExe)?;
    let version = locator.get_manifest_version_full_string();
    if version == plan.new_version {
        atomic_json(
            &state.join("desktop-health.json"),
            &Health {
                id: plan.id,
                version,
                pid: std::process::id(),
                ready: true,
                clean_exit: false,
            },
        )?;
    }
    Ok(())
}
#[cfg(windows)]
fn process(pid: u32, terminate_if_path: Option<&Path>) -> Result<bool> {
    use windows::Win32::{
        Foundation::{CloseHandle, WAIT_TIMEOUT},
        System::Threading::{
            OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, QueryFullProcessImageNameW,
            TerminateProcess, WaitForSingleObject,
        },
    };
    let access = PROCESS_QUERY_LIMITED_INFORMATION
        | PROCESS_ACCESS_RIGHTS(0x100000)
        | if terminate_if_path.is_some() {
            PROCESS_TERMINATE
        } else {
            PROCESS_ACCESS_RIGHTS(0)
        };
    let handle = match unsafe { OpenProcess(access, false, pid) } {
        Ok(handle) => handle,
        Err(e) if e.code().0 as u32 == 0x80070057 => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let result = (|| -> Result<bool> {
        if unsafe { WaitForSingleObject(handle, 0) } != WAIT_TIMEOUT {
            return Ok(false);
        }
        if let Some(expected) = terminate_if_path {
            let mut buffer = vec![0u16; 32768];
            let mut length = buffer.len() as u32;
            unsafe {
                QueryFullProcessImageNameW(
                    handle,
                    PROCESS_NAME_WIN32,
                    windows::core::PWSTR(buffer.as_mut_ptr()),
                    &mut length,
                )?;
            }
            let actual = PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize]));
            ensure!(
                fs::canonicalize(actual)? == fs::canonicalize(expected)?,
                "Recovery refused to stop an unrelated process"
            );
            unsafe {
                TerminateProcess(handle, 88)?;
                WaitForSingleObject(handle, 5000);
            }
            return Ok(false);
        }
        Ok(true)
    })();
    unsafe {
        let _ = CloseHandle(handle);
    }
    result
}
#[cfg(not(windows))]
fn process(_pid: u32, _terminate_if_path: Option<&Path>) -> Result<bool> {
    anyhow::bail!("Windows desktop recovery is unavailable on this platform")
}

pub fn run_guard(plan_path: &Path) -> Result<()> {
    let plan: Attempt = serde_json::from_slice(&fs::read(plan_path)?)?;
    let _lock = lock(&plan.state.join("guard.lock"))?;
    atomic_json(
        &plan.state.join("desktop-recovery-status.json"),
        &serde_json::json!({"status":"armed","from":plan.old_version,"to":plan.new_version}),
    )?;
    fs::write(plan.state.join("guard-ready"), &plan.id)?;
    let exit_deadline = Instant::now() + Duration::from_secs(90);
    while process(plan.parent, None)? {
        if fs::read_to_string(plan.state.join("guard-cancel"))
            .ok()
            .as_deref()
            == Some(&plan.id)
        {
            return Ok(());
        }
        if Instant::now() >= exit_deadline {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut healthy_since = None;
    let mut latest = None;
    while Instant::now() < deadline {
        let health = fs::read(plan.state.join("desktop-health.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Health>(&b).ok());
        if let Some(health) = health.filter(|h| h.id == plan.id && h.version == plan.new_version) {
            let alive = process(health.pid, None)?;
            if health.ready && (alive || health.clean_exit) {
                let since = *healthy_since.get_or_insert_with(Instant::now);
                if health.clean_exit || since.elapsed() >= Duration::from_secs(10) {
                    atomic_json(
                        &plan.state.join("desktop-recovery-status.json"),
                        &serde_json::json!({"status":"healthy","version":plan.new_version}),
                    )?;
                    fs::remove_file(plan_path)?;
                    return Ok(());
                }
            } else if !alive {
                latest = Some(health);
                break;
            }
            latest = Some(health);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    atomic_json(
        &plan.state.join("blocked-desktop.json"),
        &serde_json::json!({"version":plan.new_version,"reason":"Startup health check failed"}),
    )?;
    ensure!(
        fingerprint(&plan.previous)? == (plan.previous_hash.clone(), plan.previous_size),
        "Previous recovery package is damaged; preserved files require repair"
    );
    if let Some(health) = latest {
        let _ = process(
            health.pid,
            Some(&plan.root.join("current/azs-uploader.exe")),
        )?;
    }
    // Wait rather than restart a working game session during rollback.
    while azs_uploader_core::game_process::check() != azs_uploader_core::game_process::State::Closed
    {
        std::thread::sleep(Duration::from_secs(2));
    }
    atomic_json(
        &plan.state.join("desktop-recovery-status.json"),
        &serde_json::json!({"status":"restoring","version":plan.old_version,"failedVersion":plan.new_version}),
    )?;
    let mut command = Command::new(plan.root.join("Update.exe"));
    command
        .args(["apply", "--silent", "--package"])
        .arg(&plan.previous)
        .arg("--root")
        .arg(&plan.root)
        .arg("--packageDir")
        .arg(plan.root.join("packages"))
        .arg("--")
        .args(&plan.restart_args);
    hidden(&mut command);
    ensure!(
        command.status()?.success(),
        "Automatic desktop restoration failed; recovery package retained"
    );
    atomic_json(
        &plan.state.join("desktop-recovery-status.json"),
        &serde_json::json!({"status":"restored","version":plan.old_version,"failedVersion":plan.new_version}),
    )?;
    fs::remove_file(plan_path)?;
    Ok(())
}

pub fn clean_exit(state: &Path) {
    let path = state.join("desktop-health.json");
    let Ok(bytes) = fs::read(&path) else {
        return;
    };
    let Ok(mut health) = serde_json::from_slice::<Health>(&bytes) else {
        return;
    };
    if health.pid == std::process::id() && health.ready {
        health.clean_exit = true;
        let _ = atomic_json(&path, &health);
    }
}

pub fn record_guard_error(plan_path: &Path, error: &anyhow::Error) {
    let Ok(bytes) = fs::read(plan_path) else {
        return;
    };
    let Ok(plan) = serde_json::from_slice::<Attempt>(&bytes) else {
        return;
    };
    let _ = atomic_json(
        &plan.state.join("desktop-recovery-status.json"),
        &serde_json::json!({"status":"repair-required","version":plan.old_version,"failedVersion":plan.new_version,"detail":format!("{error:#}")}),
    );
}
