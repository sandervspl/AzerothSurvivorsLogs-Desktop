//! Desktop packages are offered only by an authenticated catalog, never an unsigned feed.
use anyhow::{Context, Result, ensure};
use azs_uploader_core::distribution::{DesktopRelease, ReleaseConfig, Settings, lock};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::mpsc::Sender,
};
use velopack::{
    UpdateCheck, UpdateManager, VelopackAsset, VelopackAssetFeed,
    locator::{LocationContext, auto_locate_app_manifest},
    sources::UpdateSource,
};
const MAX_PACKAGE: u64 = 512 * 1024 * 1024;
pub fn ready() -> bool {
    UpdateManager::new(velopack::sources::NoneSource {}, None, None)
        .ok()
        .is_some_and(|manager| manager.get_update_pending_restart().is_some())
}
use azs_uploader::recovery::PACKAGE_ID;

struct VerifiedSource {
    release: DesktopRelease,
    config: ReleaseConfig,
}
impl VerifiedSource {
    fn asset(&self) -> VelopackAsset {
        VelopackAsset {
            PackageId: PACKAGE_ID.into(),
            Version: self.release.version.clone(),
            Type: "Full".into(),
            FileName: self.release.filename.clone(),
            SHA256: self.release.sha256.clone(),
            Size: self.release.size,
            ..Default::default()
        }
    }
}
fn verified_file(path: &Path, release: &DesktopRelease) -> Result<()> {
    let mut file = File::open(path)?;
    ensure!(
        file.metadata()?.len() == release.size,
        "Desktop package size mismatch"
    );
    let mut hasher = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    ensure!(
        format!("{:x}", hasher.finalize()).eq_ignore_ascii_case(&release.sha256),
        "Desktop package hash mismatch"
    );
    Ok(())
}
impl UpdateSource for VerifiedSource {
    fn get_release_feed(
        &self,
        channel: &str,
        _app: &velopack::bundle::Manifest,
        _id: &str,
    ) -> Result<VelopackAssetFeed, velopack::Error> {
        if channel != "stable" {
            return Err(velopack::Error::Other("Unexpected update channel".into()));
        }
        Ok(VelopackAssetFeed {
            Assets: vec![self.asset()],
        })
    }
    fn download_release_entry(
        &self,
        asset: &VelopackAsset,
        local_file: &Path,
        progress: Option<Sender<i16>>,
    ) -> Result<(), velopack::Error> {
        let download = || -> Result<()> {
            ensure!(
                asset.FileName == self.release.filename
                    && asset.SHA256 == self.release.sha256
                    && asset.Version == self.release.version,
                "Unapproved desktop update"
            );
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async {
                let mut response = self
                    .config
                    .client()?
                    .get(&self.release.url)
                    .send()
                    .await?
                    .error_for_status()?;
                ensure!(
                    response.status().is_success()
                        && response
                            .content_length()
                            .is_none_or(|s| s == self.release.size),
                    "Desktop package response rejected"
                );
                let mut file = File::create(local_file)?;
                let mut received = 0;
                while let Some(bytes) = response.chunk().await? {
                    received += bytes.len() as u64;
                    ensure!(
                        received <= self.release.size,
                        "Desktop package exceeds signed size"
                    );
                    file.write_all(&bytes)?;
                    if let Some(sender) = &progress {
                        let _ = sender.send((received * 100 / self.release.size) as i16);
                    }
                }
                file.sync_all()?;
                verified_file(local_file, &self.release)
            })
        };
        download().map_err(|e| velopack::Error::Other(format!("Desktop download rejected: {e:#}")))
    }
}
fn approved(state: &Path) -> Result<Option<(UpdateManager, DesktopRelease, PathBuf)>> {
    let locator = match auto_locate_app_manifest(LocationContext::FromCurrentExe) {
        Ok(locator) => locator,
        Err(velopack::Error::NotInstalled(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        locator.get_manifest_id() == PACKAGE_ID,
        "Unexpected installed application ID"
    );
    let config = ReleaseConfig::configured()?;
    let runtime = tokio::runtime::Runtime::new()?;
    let catalog = runtime.block_on(config.catalog(state))?;
    let Some(release) = catalog.catalog.desktop else {
        return Ok(None);
    };
    ensure!(
        release.size > 0
            && release.size <= MAX_PACKAGE
            && release.sha256.len() == 64
            && release.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid desktop package limits"
    );
    ensure!(
        release.filename.len() < 150
            && release.filename.ends_with("-full.nupkg")
            && release
                .filename
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            && !release.filename.starts_with('.'),
        "Invalid desktop package filename"
    );
    let source = VerifiedSource {
        release: release.clone(),
        config,
    };
    let manager = UpdateManager::new(source, None, None)?;
    let package = locator.get_packages_dir().join(&release.filename);
    Ok(Some((manager, release, package)))
}
pub fn prepare(state: &Path) -> Result<String> {
    let _lock = lock(&state.join("release.lock"))?;
    let Some((manager, release, package)) = approved(state)? else {
        return Ok("Installer-managed updates are available in installed builds.".into());
    };
    if azs_uploader::recovery::blocked(state, &release.version) {
        return Ok(format!(
            "Uploader {} failed its update check and is blocked; waiting for a corrected release.",
            release.version
        ));
    }
    match manager.check_for_updates()? {
        UpdateCheck::UpdateAvailable(update) => {
            manager.download_updates(&update, None)?;
            verified_file(&package, &release)?;
            Ok(format!(
                "Uploader {} downloaded. It will update next time you open the app with the game closed.",
                release.version
            ))
        }
        _ => Ok(format!(
            "Uploader {} is up to date.",
            manager.get_current_version_as_string()
        )),
    }
}
/// Startup-only handoff. No native monitoring or upload can have started yet.
pub fn apply_on_safe_launch(state: &Path, restart_args: &[String]) -> Result<()> {
    if !state.is_dir() || Settings::load(state)?.is_some_and(|s| s.updates_paused) {
        return Ok(());
    }
    // Do not put network latency on an ordinary launch without a staged update.
    let manager = match UpdateManager::new(velopack::sources::NoneSource {}, None, None) {
        Ok(manager) => manager,
        Err(velopack::Error::NotInstalled(_)) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if manager.get_update_pending_restart().is_none() {
        return Ok(());
    }
    azs_uploader_core::game_process::ensure_closed()?;
    let _lock = lock(&state.join("release.lock"))?;
    let Some((manager, release, package)) = approved(state)? else {
        return Ok(());
    };
    if azs_uploader::recovery::blocked(state, &release.version) {
        return Ok(());
    }
    let Some(pending) = manager.get_update_pending_restart() else {
        return Ok(());
    };
    if pending.FileName != release.filename || pending.Version != release.version {
        return Ok(());
    }
    verified_file(&package, &release)?;
    let settings = Settings::load(state)?;
    if let Some(settings) = settings {
        ensure!(
            !settings
                .game_root
                .join("benilla-config/.azs-recorder/transaction.json")
                .exists(),
            "Addon recovery must finish before updating the uploader"
        );
    }
    azs_uploader_core::game_process::ensure_closed()?;
    azs_uploader::recovery::arm(state, &release.version, restart_args)?;
    if let Err(error) = manager.wait_exit_then_apply_updates(pending, true, true, restart_args) {
        azs_uploader::recovery::cancel(state);
        return Err(error)
            .context("Cannot start desktop update; the current version is still available");
    }
    // Release our application/state locks before the helper replaces files.
    drop(_lock);
    std::process::exit(0);
}
