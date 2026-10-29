//! Signed release delivery and recoverable addon installation. Never executes Lua.
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

pub const ADDON: &str = "AzerothSurvivorsRecorder";
const MAX_ARCHIVE: u64 = 8 * 1024 * 1024;
const MAX_EXPANDED: u64 = 32 * 1024 * 1024;
const MAX_CATALOG: u64 = 256 * 1024;

pub fn retryable(error: &anyhow::Error) -> bool {
    if let Some(network) = error.downcast_ref::<reqwest::Error>() {
        return network.is_connect()
            || network.is_timeout()
            || network.is_body()
            || network
                .status()
                .is_some_and(|s| s.is_server_error() || s.as_u16() == 429);
    }
    if let Some(io) = error.downcast_ref::<std::io::Error>() {
        return matches!(
            io.kind(),
            std::io::ErrorKind::Interrupted
                | std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::TimedOut
        ) || matches!(io.raw_os_error(), Some(32 | 33));
    }
    false
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    pub schema_version: u32,
    pub game_root: PathBuf,
    #[serde(default)]
    pub updates_paused: bool,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Installation {
    pub game_root: PathBuf,
    pub combat: Option<PathBuf>,
    pub recorder: Option<PathBuf>,
}
impl Settings {
    pub fn load(state: &Path) -> Result<Option<Self>> {
        let file = state.join("installation.json");
        match fs::read(&file) {
            Ok(bytes) => {
                let settings: Self = serde_json::from_slice(&bytes).context(
                    "Cannot read game directory settings. Choose your game directory again.",
                )?;
                ensure!(
                    settings.schema_version == 1,
                    "Unsupported installation settings"
                );
                Ok(Some(settings))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("Cannot read saved game directory"),
        }
    }
    pub fn choose(state: &Path, root: &Path) -> Result<Self> {
        let root = dunce::canonicalize(root).context("Game directory is unavailable")?;
        validate_game(&root)?;
        let settings = Self {
            schema_version: 1,
            game_root: root,
            updates_paused: false,
        };
        settings.save(state)?;
        Ok(settings)
    }
    pub fn save(&self, state: &Path) -> Result<()> {
        fs::create_dir_all(state)?;
        atomic_json(&state.join("installation.json"), self)
    }
    pub fn discover(&self) -> Result<Installation> {
        validate_game(&self.game_root)?;
        let combat = self.game_root.join("benilla-config/Logs/WoWCombatLog.txt");
        let recorder = self
            .game_root
            .join(format!("benilla-config/saved/{ADDON}.lua"));
        Ok(Installation {
            game_root: self.game_root.clone(),
            combat: combat.is_file().then_some(combat),
            recorder: recorder.is_file().then_some(recorder),
        })
    }
}
fn validate_game(root: &Path) -> Result<()> {
    ensure!(
        root.is_dir(),
        "Game directory is unavailable. Choose its new location."
    );
    ensure!(
        root.join("AzerothSurvivors.exe").is_file(),
        "Choose the folder containing AzerothSurvivors.exe"
    );
    safe_path(root, &root.join("AzerothSurvivors.exe"))?;
    safe_path(root, &root.join("benilla-config"))
}

#[derive(Clone, Debug)]
pub struct ReleaseConfig {
    pub url: String,
    pub keys: BTreeMap<String, VerifyingKey>,
}
impl ReleaseConfig {
    pub fn configured() -> Result<Self> {
        let mut url = option_env!("AZS_RELEASE_CATALOG_URL")
            .unwrap_or("https://downloads.azslogs.com/stable.json")
            .to_string();
        let mut keys = option_env!("AZS_RELEASE_PUBLIC_KEYS")
            .unwrap_or("")
            .to_string();
        // Runtime test roots and loopback HTTP are deliberately debug-only.
        if cfg!(debug_assertions) {
            if let Ok(value) = std::env::var("AZS_RELEASE_CATALOG_URL") {
                url = value;
            }
            if let Ok(value) = std::env::var("AZS_RELEASE_PUBLIC_KEYS") {
                keys = value;
            }
        }
        validate_url(&url)?;
        let mut parsed = BTreeMap::new();
        for entry in keys.split(',').filter(|entry| !entry.is_empty()) {
            let (id, hex) = entry
                .split_once(':')
                .context("Invalid configured release key")?;
            ensure!(
                !id.is_empty() && id.len() <= 64 && !parsed.contains_key(id),
                "Invalid release key ID"
            );
            let bytes = decode_hash(hex)?;
            parsed.insert(id.to_string(), VerifyingKey::from_bytes(&bytes)?);
        }
        ensure!(
            !parsed.is_empty(),
            "Release signing keys are not configured for this build. Addon setup is pending."
        );
        Ok(Self { url, keys: parsed })
    }
    pub fn client(&self) -> Result<reqwest::Client> {
        Ok(reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()?)
    }
    fn verify(&self, envelope: &Envelope) -> Result<Catalog> {
        let key = self
            .keys
            .get(&envelope.key_id)
            .context("Unknown release signing key; update rejected")?;
        let payload = STANDARD
            .decode(&envelope.payload)
            .context("Invalid catalog payload")?;
        ensure!(
            payload.len() as u64 <= MAX_CATALOG,
            "Release catalog is too large"
        );
        let signature = Signature::from_slice(&STANDARD.decode(&envelope.signature)?)?;
        key.verify_strict(&payload, &signature)
            .context("Release signature is invalid; installed files were preserved")?;
        let catalog: Catalog =
            serde_json::from_slice(&payload).context("Unsupported release catalog")?;
        ensure!(
            catalog.schema_version == 1 && catalog.channel == "stable",
            "Unsupported release channel or schema"
        );
        validate_addon(&catalog.addon)?;
        Ok(catalog)
    }
    pub async fn catalog(&self, state: &Path) -> Result<VerifiedCatalog> {
        let bytes = get_bounded(&self.client()?, &self.url, MAX_CATALOG).await?;
        let envelope: Envelope =
            serde_json::from_slice(&bytes).context("Invalid signed release envelope")?;
        let catalog = self.verify(&envelope)?;
        ensure!(
            catalog.expires_at > chrono::Utc::now().timestamp_millis(),
            "Release catalog has expired; installed files were preserved"
        );
        let digest = crate::sha256(&STANDARD.decode(&envelope.payload)?);
        let policy_path = state.join("release-policy.json");
        let policy: Option<Policy> = read_json(&policy_path)?;
        if let Some(previous) = policy {
            ensure!(
                catalog.sequence >= previous.sequence,
                "Replayed release catalog rejected"
            );
            ensure!(
                catalog.sequence != previous.sequence || digest == previous.digest,
                "Release catalog changed without a new sequence"
            );
        }
        let source = reqwest::Url::parse(&self.url)?;
        for url in std::iter::once(catalog.addon.url.as_str())
            .chain(catalog.desktop.iter().map(|d| d.url.as_str()))
        {
            validate_url(url)?;
            ensure!(
                reqwest::Url::parse(url)?.origin() == source.origin(),
                "Release assets must use the configured download origin"
            );
        }
        atomic_json(
            &policy_path,
            &Policy {
                sequence: catalog.sequence,
                digest,
            },
        )?;
        Ok(VerifiedCatalog { catalog, envelope })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Envelope {
    key_id: String,
    payload: String,
    signature: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Catalog {
    pub schema_version: u32,
    pub channel: String,
    pub sequence: u64,
    pub expires_at: i64,
    pub addon: AddonRelease,
    #[serde(default)]
    pub desktop: Option<DesktopRelease>,
}
#[derive(Clone, Debug)]
pub struct VerifiedCatalog {
    pub catalog: Catalog,
    envelope: Envelope,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DesktopRelease {
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub filename: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AddonRelease {
    pub version: String,
    pub interface: u32,
    pub min_uploader_version: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub files: BTreeMap<String, FileSpec>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSpec {
    pub sha256: String,
    pub size: u64,
}
#[derive(Serialize, Deserialize)]
struct Policy {
    sequence: u64,
    digest: String,
}
#[derive(Serialize, Deserialize)]
struct Journal {
    envelope: Envelope,
    had_previous: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddonStatus {
    pub status: String,
    pub version: String,
}

fn decode_hash(value: &str) -> Result<[u8; 32]> {
    ensure!(
        value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid SHA-256 or public key"
    );
    let mut bytes = [0; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16)?;
    }
    Ok(bytes)
}
fn validate_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url)?;
    let loopback = cfg!(debug_assertions)
        && parsed.scheme() == "http"
        && matches!(parsed.host_str(), Some("127.0.0.1" | "[::1]"));
    ensure!(
        parsed.scheme() == "https" || loopback,
        "Release delivery requires HTTPS"
    );
    ensure!(
        parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.fragment().is_none()
            && parsed.query().is_none(),
        "Invalid release URL"
    );
    Ok(())
}
fn validate_addon(addon: &AddonRelease) -> Result<()> {
    ensure!(
        addon.interface == 11200,
        "Recorder release targets an incompatible game interface"
    );
    let _ = semver::Version::parse(&addon.version)?;
    let _ = semver::Version::parse(&addon.min_uploader_version)?;
    decode_hash(&addon.sha256)?;
    ensure!(
        addon.size > 0
            && addon.size <= MAX_ARCHIVE
            && !addon.files.is_empty()
            && addon.files.len() <= 64,
        "Addon package exceeds supported limits"
    );
    let mut names = std::collections::BTreeSet::new();
    let mut total = 0u64;
    for (name, file) in &addon.files {
        let stem = name
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        ensure!(
            !matches!(
                stem.as_str(),
                "CON"
                    | "PRN"
                    | "AUX"
                    | "NUL"
                    | "COM1"
                    | "COM2"
                    | "COM3"
                    | "COM4"
                    | "COM5"
                    | "COM6"
                    | "COM7"
                    | "COM8"
                    | "COM9"
                    | "LPT1"
                    | "LPT2"
                    | "LPT3"
                    | "LPT4"
                    | "LPT5"
                    | "LPT6"
                    | "LPT7"
                    | "LPT8"
                    | "LPT9"
            ),
            "Reserved Windows filename in addon release"
        );
        ensure!(
            !name.starts_with('.')
                && name.len() <= 100
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                && (name.ends_with(".lua") || name.ends_with(".toc")),
            "Unsafe addon filename"
        );
        ensure!(
            names.insert(name.to_ascii_lowercase()),
            "Duplicate addon filename"
        );
        decode_hash(&file.sha256)?;
        total = total
            .checked_add(file.size)
            .context("Expanded addon exceeds limits")?;
    }
    ensure!(total <= MAX_EXPANDED, "Expanded addon exceeds limits");
    for required in [
        format!("{ADDON}.toc"),
        format!("{ADDON}.lua"),
        "SpellTooltips.lua".into(),
        "Jewelry.lua".into(),
    ] {
        ensure!(
            addon.files.contains_key(&required),
            "Addon package is missing a required runtime file"
        );
    }
    Ok(())
}
pub async fn get_bounded(client: &reqwest::Client, url: &str, limit: u64) -> Result<Vec<u8>> {
    let mut response = client
        .get(url)
        .send()
        .await
        .context("Release server is unavailable; retry when connected")?
        .error_for_status()
        .context("Release download is unavailable")?;
    ensure!(response.status().is_success(), "Release redirect rejected");
    ensure!(
        response.content_length().is_none_or(|size| size <= limit),
        "Release download exceeds size limit"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("Release download was interrupted; retry to continue setup")?
    {
        ensure!(
            (bytes.len() as u64) + chunk.len() as u64 <= limit,
            "Release download exceeds size limit"
        );
        bytes.extend(chunk);
    }
    Ok(bytes)
}
pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    atomic_bytes(path, &serde_json::to_vec_pretty(value)?)
}
fn atomic_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Missing state directory")?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .map_err(|e| e.error)
        .context("Could not save update state")?;
    Ok(())
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    if let Ok(metadata) = fs::metadata(path) {
        ensure!(
            metadata.len() <= MAX_CATALOG,
            "Update state exceeds its size limit; preserve it for repair"
        );
    }
    match fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes)
                .context("Update state is damaged; preserve it and use repair")?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}
fn safe_path(root: &Path, path: &Path) -> Result<()> {
    ensure!(
        path.starts_with(root),
        "Update destination is outside the game directory"
    );
    for ancestor in path.ancestors() {
        if !ancestor.starts_with(root) {
            break;
        }
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => ensure!(
                !is_link(&metadata),
                "Linked or redirected game paths cannot be managed safely"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
pub fn lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .context("Cannot access update lock; check folder permissions")?;
    file.try_lock().map_err(|_| {
        anyhow::anyhow!("Another uploader is managing this installation. Retry after it finishes.")
    })?;
    Ok(file)
}
fn matches_files(root: &Path, addon: &AddonRelease) -> Result<bool> {
    if !root.is_dir() {
        return Ok(false);
    }
    let entries = fs::read_dir(root)?.collect::<std::io::Result<Vec<_>>>()?;
    if entries.len() != addon.files.len() {
        return Ok(false);
    }
    for (name, expected) in &addon.files {
        let path = root.join(name);
        safe_path(root, &path)?;
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() && metadata.len() == expected.size => {}
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        }
        if crate::sha256(&fs::read(path)?) != expected.sha256.to_ascii_lowercase() {
            return Ok(false);
        }
    }
    Ok(true)
}
fn extract(bytes: &[u8], destination: &Path, addon: &AddonRelease) -> Result<()> {
    // Bound the central directory before the ZIP library allocates from its count.
    // These small recorder archives never need multi-disk or ZIP64 containers.
    let end = bytes
        .windows(4)
        .rposition(|b| b == b"PK\x05\x06")
        .context("Missing ZIP directory")?;
    ensure!(end + 22 <= bytes.len(), "Truncated ZIP directory");
    let word = |offset| u16::from_le_bytes([bytes[end + offset], bytes[end + offset + 1]]);
    ensure!(
        word(4) == 0 && word(6) == 0 && word(8) == word(10) && word(10) != u16::MAX,
        "Multi-disk and ZIP64 addon archives are unsupported"
    );
    ensure!(
        word(10) as usize == addon.files.len() && end + 22 + word(20) as usize == bytes.len(),
        "Unexpected ZIP directory entries or trailing data"
    );
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    ensure!(
        archive.len() == addon.files.len(),
        "Unexpected entries in addon archive"
    );
    let mut found = std::collections::BTreeSet::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry
            .name()
            .strip_prefix(&format!("{ADDON}/"))
            .context("Archive path is outside the addon folder")?
            .to_string();
        let expected = addon
            .files
            .get(&name)
            .context("Unexpected or unsafe addon archive entry")?;
        ensure!(
            found.insert(name.to_ascii_lowercase()),
            "Duplicate addon archive entry"
        );
        ensure!(
            entry
                .unix_mode()
                .is_none_or(|mode| mode & 0o170000 == 0 || mode & 0o170000 == 0o100000),
            "Addon archive contains a link or directory"
        );
        ensure!(
            entry.size() == expected.size,
            "Addon expanded size mismatch"
        );
        let mut output = Vec::new();
        (&mut entry)
            .take(expected.size + 1)
            .read_to_end(&mut output)?;
        ensure!(
            output.len() as u64 == expected.size
                && crate::sha256(&output) == expected.sha256.to_ascii_lowercase(),
            "Addon runtime file hash mismatch"
        );
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination.join(name))?;
        file.write_all(&output)?;
        file.sync_all()?;
    }
    let toc = fs::read_to_string(destination.join(format!("{ADDON}.toc")))?;
    ensure!(
        toc.lines().any(|line| line.trim() == "## Interface: 11200")
            && toc
                .lines()
                .any(|line| line.trim() == format!("## Version: {}", addon.version)),
        "Addon TOC does not match its release"
    );
    Ok(())
}
fn fault(name: &str) {
    if cfg!(debug_assertions) && std::env::var("AZS_UPDATE_FAULT").ok().as_deref() == Some(name) {
        std::process::exit(86);
    }
}

fn remove_owned_directory(root: &Path, path: &Path) -> Result<()> {
    safe_path(root, path)?;
    let resolved = dunce::canonicalize(path)?;
    let boundary = dunce::canonicalize(root)?;
    ensure!(
        resolved != boundary && resolved.starts_with(&boundary),
        "Refusing cleanup outside the managed directory"
    );
    fs::remove_dir_all(path)?;
    Ok(())
}

pub async fn maintain_addon(
    state: &Path,
    settings: &Settings,
    config: &ReleaseConfig,
) -> Result<AddonStatus> {
    validate_game(&settings.game_root)?;
    fs::create_dir_all(state)?;
    let _state_lock = lock(&state.join("release.lock"))?;
    let base = settings.game_root.join("benilla-config");
    let managed = base.join(".azs-recorder");
    let live = base.join("AddOns").join(ADDON);
    let stage = managed.join("stage");
    let backup = managed.join("previous");
    let journal_path = managed.join("transaction.json");
    let receipt = managed.join("installed.json");
    let previous_receipt = managed.join("previous.json");
    for path in [
        &managed,
        &live,
        &stage,
        &backup,
        &journal_path,
        &receipt,
        &previous_receipt,
    ] {
        safe_path(&settings.game_root, path)?;
    }
    fs::create_dir_all(&managed)
        .context("Cannot create addon update directory; check game-folder permissions")?;
    let lock_path = managed.join("install.lock");
    safe_path(&settings.game_root, &lock_path)?;
    let _game_lock = lock(&lock_path)?;
    // Recover before networking: a crash must not leave the addon unavailable offline.
    if let Some(journal) = read_json::<Journal>(&journal_path)? {
        let previous = config.verify(&journal.envelope)?;
        if crate::game_process::check() != crate::game_process::State::Closed {
            return Ok(AddonStatus {
                status: "waiting-for-game".into(),
                version: previous.addon.version,
            });
        }
        if matches_files(&live, &previous.addon)? {
            atomic_json(&receipt, &previous.addon)?;
            fs::remove_file(&journal_path)?;
        } else if !live.exists() && backup.exists() {
            fs::rename(&backup, &live).context("Cannot restore previous addon; close programs locking the game directory and retry")?;
            fs::remove_file(&journal_path)?;
        } else if !live.exists() && !journal.had_previous {
            fs::remove_file(&journal_path)?;
        } else if live.exists() && !backup.exists() && journal.had_previous {
            // The crash/locked-file failure happened before the previous version moved.
            fs::remove_file(&journal_path)?;
        } else if live.exists() && backup.exists() {
            let failed = managed.join(format!(
                "failed-{}",
                chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
            ));
            safe_path(&settings.game_root, &failed)?;
            fs::rename(&live, failed)?;
            fs::rename(&backup, &live).context(
                "Previous addon restoration failed; preserved files are available for repair",
            )?;
            fs::remove_file(&journal_path)?;
        } else {
            bail!(
                "Addon recovery needs repair. Existing files and backup were preserved; close the game and retry."
            );
        }
    }
    let installed: Option<AddonRelease> = read_json(&receipt)?;
    if settings.updates_paused {
        return Ok(AddonStatus {
            status: "paused".into(),
            version: installed.map(|a| a.version).unwrap_or_default(),
        });
    }
    let verified = config.catalog(state).await?;
    let addon = &verified.catalog.addon;
    ensure!(
        semver::Version::parse(env!("CARGO_PKG_VERSION"))?
            >= semver::Version::parse(&addon.min_uploader_version)?,
        "This addon requires a newer uploader. The working addon is preserved while the uploader updates."
    );
    if let Some(previous) = &installed {
        ensure!(
            semver::Version::parse(&addon.version)? >= semver::Version::parse(&previous.version)?,
            "Addon downgrade rejected; publish a corrective release with a higher version"
        );
        ensure!(
            previous.version != addon.version
                || previous.sha256.eq_ignore_ascii_case(&addon.sha256),
            "This release reused an installed addon version with different bytes. Existing files were preserved; the publisher must issue a new version."
        );
    }
    if matches_files(&live, addon)? {
        atomic_json(&receipt, addon)?;
        return Ok(AddonStatus {
            status: "ready".into(),
            version: addon.version.clone(),
        });
    }
    let cache = state.join("downloads");
    fs::create_dir_all(&cache)?;
    let archive_path = cache.join(format!("{}.zip", addon.sha256));
    let mut bytes = if fs::metadata(&archive_path).is_ok_and(|m| m.len() == addon.size) {
        fs::read(&archive_path).unwrap_or_default()
    } else {
        Vec::new()
    };
    if bytes.len() as u64 != addon.size
        || crate::sha256(&bytes) != addon.sha256.to_ascii_lowercase()
    {
        bytes = get_bounded(&config.client()?, &addon.url, addon.size).await?;
        ensure!(
            bytes.len() as u64 == addon.size
                && crate::sha256(&bytes) == addon.sha256.to_ascii_lowercase(),
            "Addon archive hash mismatch; installed files were preserved"
        );
        atomic_bytes(&archive_path, &bytes)?;
    }
    // Staging is disposable, but only this validated manager-owned path is removed.
    if stage.exists() {
        remove_owned_directory(&managed, &stage)?;
    }
    fs::create_dir(&stage)?;
    extract(&bytes, &stage, addon)?;
    if crate::game_process::check() != crate::game_process::State::Closed {
        return Ok(AddonStatus {
            status: "waiting-for-game".into(),
            version: addon.version.clone(),
        });
    }
    fs::create_dir_all(live.parent().context("Missing addon parent")?)?;
    // Retain the previous directory rather than deleting possibly unowned files.
    if backup.exists() {
        let known = read_json::<AddonRelease>(&previous_receipt).ok().flatten();
        if known
            .as_ref()
            .is_some_and(|a| matches_files(&backup, a).unwrap_or(false))
        {
            remove_owned_directory(&managed, &backup)?;
        } else {
            let retained = managed.join(format!(
                "retained-{}",
                chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
            ));
            safe_path(&settings.game_root, &retained)?;
            fs::rename(&backup, retained)?;
        }
    }
    if let Some(previous) = installed
        .as_ref()
        .filter(|a| matches_files(&live, a).unwrap_or(false))
    {
        atomic_json(&previous_receipt, previous)?;
    } else if previous_receipt.exists() {
        fs::remove_file(&previous_receipt)?;
    }
    crate::game_process::ensure_closed()?;
    let journal = Journal {
        envelope: verified.envelope,
        had_previous: live.exists(),
    };
    atomic_json(&journal_path, &journal)?;
    fault("after-journal");
    if live.exists() {
        fs::rename(&live, &backup)
            .context("Cannot replace addon; close programs locking its files and retry")?;
    }
    fault("after-backup");
    if let Err(error) = fs::rename(&stage, &live) {
        if backup.exists() && !live.exists() {
            let _ = fs::rename(&backup, &live);
        }
        return Err(error).context(
            "Addon activation failed; previous files and recovery journal were preserved",
        );
    }
    fault("after-activate");
    ensure!(
        matches_files(&live, addon)?,
        "Installed addon verification failed; backup preserved for repair"
    );
    atomic_json(&receipt, addon)?;
    fs::remove_file(journal_path)?;
    Ok(AddonStatus {
        status: "ready".into(),
        version: addon.version.clone(),
    })
}
