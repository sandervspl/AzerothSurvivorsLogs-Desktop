pub mod client_build;
pub mod distribution;
pub mod game_process;
pub mod native_history;
pub mod native_save;
mod run_filter;
use anyhow::{Context, Result, bail};
use flate2::{Compression, GzBuilder};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub kind: String,
    pub filename: String,
    pub sha256: String,
    pub compressed_bytes: u64,
    pub original_bytes: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub schema_version: u32,
    pub sources: Vec<Source>,
    pub year: i32,
    pub utc_offset_minutes: i32,
    pub visibility: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub api: String,
    pub owner: String,
    pub manifest: Manifest,
}
pub struct Engine {
    state: PathBuf,
    client: reqwest::Client,
}
pub struct StageOptions<'a> {
    pub api: &'a str,
    pub owner: &'a str,
    pub combat: &'a Path,
    pub recorder: Option<&'a Path>,
    pub native_save: Option<(&'a Path, &'a Path)>,
    pub year: i32,
    pub offset: i32,
    pub visibility: &'a str,
}
pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

// Only known immutable input errors are terminal; transport/server failures stay resumable.
fn invalid_recording(error: &str) -> bool {
    error.starts_with("Processing failed: Wrong recorder file.")
        || error == "Processing failed: Missing run baseline"
}

fn recorder_header(mut bytes: &[u8]) -> bool {
    if let Some(rest) = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        bytes = rest;
    }
    loop {
        bytes = bytes.trim_ascii_start();
        if bytes.starts_with(b"--") {
            let Some(end) = bytes.iter().position(|b| *b == b'\n') else {
                return false;
            };
            bytes = &bytes[end + 1..];
        } else {
            return bytes
                .strip_prefix(b"AZSRecorderDB")
                .is_some_and(|rest| rest.trim_ascii_start().starts_with(b"="));
        }
    }
}

impl Engine {
    pub fn new(state: impl AsRef<Path>) -> Result<Self> {
        fs::create_dir_all(state.as_ref().join("spool"))?;
        let engine = Self {
            state: state.as_ref().into(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()?,
        };
        engine.db()?.execute_batch("CREATE TABLE IF NOT EXISTS jobs (id TEXT PRIMARY KEY, payload TEXT NOT NULL, status TEXT NOT NULL, server_id TEXT, error TEXT);")?;
        // Older versions treated these immutable input failures as retryable.
        engine.db()?.execute(
            "UPDATE jobs SET status = 'invalid' WHERE status = 'pending' AND (error LIKE 'Processing failed: Wrong recorder file.%' OR error = 'Processing failed: Missing run baseline')",
            [],
        )?;
        Ok(engine)
    }
    fn db(&self) -> Result<Connection> {
        let db = Connection::open(self.state.join("uploads.db"))?;
        db.busy_timeout(Duration::from_secs(5))?;
        Ok(db)
    }
    pub async fn application_origin(&self, api: &str) -> Result<String> {
        let config = checked(
            self.client
                .get(format!("{}/api/config", validate_api(api)?))
                .send()
                .await?,
        )
        .await?;
        validate_api(
            config["appOrigin"]
                .as_str()
                .context("Missing website origin")?,
        )
    }
    pub async fn account(&self, api: &str, token: &str) -> Result<String> {
        let value: Value = checked(
            self.client
                .get(format!("{}/api/v1/me", validate_api(api)?))
                .bearer_auth(token)
                .send()
                .await?,
        )
        .await?;
        Ok(value["user"]["id"]
            .as_str()
            .context("API returned no account")?
            .into())
    }
    pub fn stage(&self, options: StageOptions<'_>) -> Result<Job> {
        let StageOptions {
            api,
            owner,
            combat,
            recorder,
            native_save,
            year,
            offset,
            visibility,
        } = options;
        let api = validate_api(api)?;
        if !(2020..=2100).contains(&year) || !(-840..=840).contains(&offset) {
            bail!("Invalid timestamp settings");
        }
        if !["private", "public", "unlisted"].contains(&visibility) {
            bail!("Invalid visibility");
        }
        let mut sources = vec![self.snapshot(combat, "combat")?];
        if let Some(path) = recorder {
            sources.push(self.snapshot(path, "recorder")?);
        }
        if let Some((save, exe)) = native_save {
            let data = native_history::bundle(&self.state, save, exe)?;
            let temporary = self
                .state
                .join("spool")
                .join(format!("native-talents-{}.json", std::process::id()));
            fs::write(&temporary, serde_json::to_vec(&data)?)?;
            let source = self.snapshot(&temporary, "talents");
            fs::remove_file(&temporary)?;
            let mut source = source?;
            source.filename = "native-talents.json".into();
            sources.push(source);
        }
        let manifest = Manifest {
            schema_version: 1,
            sources,
            year,
            utc_offset_minutes: offset,
            visibility: visibility.into(),
        };
        let id = sha256(
            serde_json::to_string(&json!({"api":api,"owner":owner,"manifest":manifest}))?
                .as_bytes(),
        );
        let job = Job {
            id: id.clone(),
            api,
            owner: owner.into(),
            manifest,
        };
        let job = self.filter_job(&job)?.unwrap_or(Job {
            manifest: Manifest {
                sources: vec![],
                ..job.manifest.clone()
            },
            ..job
        });
        if job.manifest.sources.is_empty() {
            return Ok(job);
        }
        self.db()?.execute(
            "INSERT OR IGNORE INTO jobs (id, payload, status) VALUES (?, ?, 'pending')",
            params![id, serde_json::to_string(&job)?],
        )?;
        Ok(job)
    }
    fn filter_job(&self, job: &Job) -> Result<Option<Job>> {
        if is_local_api(&reqwest::Url::parse(&job.api)?) {
            return Ok(Some(job.clone()));
        }
        if job.manifest.sources.is_empty() {
            return Ok(None);
        }
        let read = |kind: &str| -> Result<Option<Vec<u8>>> {
            job.manifest
                .sources
                .iter()
                .find(|source| source.kind == kind)
                .map(|source| {
                    let file = fs::File::open(
                        self.state
                            .join("spool")
                            .join(format!("{}.gz", source.sha256)),
                    )?;
                    let mut bytes = Vec::new();
                    flate2::read::GzDecoder::new(file)
                        .take(source.original_bytes + 1)
                        .read_to_end(&mut bytes)?;
                    anyhow::ensure!(
                        bytes.len() as u64 == source.original_bytes,
                        "Invalid saved source length"
                    );
                    Ok(bytes)
                })
                .transpose()
        };
        let combat = read("combat")?.context("Missing combat source")?;
        let recorder = read("recorder")?;
        let filtered = run_filter::filter(
            &combat,
            recorder.as_deref(),
            job.manifest.year,
            job.manifest.utc_offset_minutes,
        )?;
        if filtered.ignored {
            return Ok(None);
        }
        let mut result = job.clone();
        result.manifest.year = filtered.year;
        for (kind, bytes) in [
            ("combat", Some(filtered.combat)),
            ("recorder", filtered.recorder),
        ] {
            if let Some(bytes) = bytes {
                let original = result
                    .manifest
                    .sources
                    .iter_mut()
                    .find(|source| source.kind == kind)
                    .context("Missing source")?;
                let temporary = self
                    .state
                    .join("spool")
                    .join(format!("filtered-{}-{kind}", std::process::id()));
                fs::write(&temporary, bytes)?;
                let snapshot = self.snapshot(&temporary, kind);
                fs::remove_file(&temporary)?;
                let mut source = snapshot?;
                source.filename = original.filename.clone();
                *original = source;
            }
        }
        Ok(Some(result))
    }
    fn snapshot(&self, path: &Path, kind: &str) -> Result<Source> {
        let mut file =
            fs::File::open(path).with_context(|| format!("Cannot read {}", path.display()))?;
        let before = file.metadata()?;
        if before.len() == 0 || before.len() > 128 * 1024 * 1024 {
            bail!("Source must be between 1 byte and 128 MiB");
        }
        if kind == "recorder" && before.len() > 16 * 1024 * 1024 {
            bail!("Recorder exceeds 16 MiB");
        }
        if kind == "recorder" {
            let mut header = [0u8; 64 * 1024];
            let count = file.read(&mut header)?;
            file.rewind()?;
            if !recorder_header(&header[..count]) {
                bail!(
                    "Wrong recorder file. Select benilla-config/saved/AzerothSurvivorsRecorder.lua, not the addon code in AddOns. Reload the game's UI or exit normally to save the export."
                );
            }
        }
        let temporary =
            self.state
                .join("spool")
                .join(format!("{}-{}.partial", std::process::id(), kind));
        let output = fs::File::create(&temporary)?;
        let mut encoder = GzBuilder::new().mtime(0).write(output, Compression::new(6));
        let mut buffer = [0u8; 64 * 1024];
        let mut original = 0;
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            original += count as u64;
            if original > before.len() {
                bail!("Source changed while copying; save and retry");
            }
            encoder.write_all(&buffer[..count])?;
        }
        let output = encoder.finish()?;
        output.sync_all()?;
        let after = file.metadata()?;
        if original != before.len()
            || before.len() != after.len()
            || before.modified()? != after.modified()?
        {
            bail!("Source changed while copying; save and retry");
        }
        let mut compressed = fs::File::open(&temporary)?;
        if compressed.metadata()?.len() > 16 * 1024 * 1024 {
            bail!("Compressed source exceeds 16 MiB");
        }
        let mut hasher = Sha256::new();
        loop {
            let n = compressed.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        let hash = format!("{:x}", hasher.finalize());
        let bytes = compressed.metadata()?.len();
        drop(compressed);
        let destination = self.state.join("spool").join(format!("{hash}.gz"));
        if destination.exists() {
            fs::remove_file(&temporary)?;
        } else {
            fs::rename(&temporary, &destination)?;
        }
        Ok(Source {
            kind: kind.into(),
            filename: path
                .file_name()
                .context("Invalid source name")?
                .to_string_lossy()
                .into(),
            sha256: hash,
            compressed_bytes: bytes,
            original_bytes: original,
        })
    }
    pub fn pending(&self) -> Result<Vec<Job>> {
        let db = self.db()?;
        let mut statement = db.prepare(
            "SELECT payload FROM jobs WHERE status NOT IN ('ready', 'invalid') ORDER BY rowid",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }
    pub async fn upload(&self, job: &Job, token: &str, progress: impl Fn(String)) -> Result<Value> {
        if self.account(&job.api, token).await? != job.owner {
            bail!("This queued upload belongs to a different account");
        }
        self.db()?.execute(
            "UPDATE jobs SET status = 'uploading', error = NULL WHERE id = ?",
            [&job.id],
        )?;
        let result = self.upload_inner(job, token, progress).await;
        if let Err(error) = &result {
            self.db()?.execute(
                "UPDATE jobs SET status = ?, error = ? WHERE id = ?",
                params![
                    if invalid_recording(&error.to_string()) {
                        "invalid"
                    } else {
                        "pending"
                    },
                    error.to_string(),
                    job.id
                ],
            )?;
        }
        result
    }
    async fn upload_inner(
        &self,
        job: &Job,
        token: &str,
        progress: impl Fn(String),
    ) -> Result<Value> {
        let Some(filtered) = self.filter_job(job)? else {
            progress("No runs reached five minutes; no source files uploaded".into());
            self.db()?.execute(
                "UPDATE jobs SET status = 'ready', error = NULL WHERE id = ?",
                [&job.id],
            )?;
            return Ok(json!({"status":"ready", "runs":[], "newRuns":[], "ignored":true}));
        };
        let job = &filtered;
        progress("Preparing upload".into());
        let upload: Value = checked(
            self.client
                .post(format!("{}/api/v1/uploads", job.api))
                .bearer_auth(token)
                .json(&job.manifest)
                .send()
                .await?,
        )
        .await?;
        let id = upload["id"].as_str().context("Missing upload ID")?;
        self.db()?.execute(
            "UPDATE jobs SET server_id = ? WHERE id = ?",
            params![id, job.id],
        )?;
        if upload["status"] != "ready"
            && upload["status"] != "processing"
            && upload["status"] != "queued"
        {
            for source in &job.manifest.sources {
                let remote = upload["sources"]
                    .as_array()
                    .context("Missing source destinations")?
                    .iter()
                    .find(|s| s["kind"] == source.kind)
                    .context("Missing source destination")?;
                if remote["ready"] == true {
                    continue;
                }
                let url = remote["url"].as_str().context("Invalid destination")?;
                if !url.starts_with(&format!("{}/api/v1/uploads/", job.api)) {
                    bail!("API returned an unexpected upload destination");
                }
                progress(format!(
                    "Uploading {} ({:.1} MiB)",
                    source.kind,
                    source.compressed_bytes as f64 / 1048576.0
                ));
                let bytes = fs::read(
                    self.state
                        .join("spool")
                        .join(format!("{}.gz", source.sha256)),
                )?;
                if sha256(&bytes) != source.sha256 {
                    bail!("Local spool failed its integrity check");
                }
                checked(
                    self.client
                        .put(url)
                        .bearer_auth(token)
                        .header("Content-Type", "application/gzip")
                        .body(bytes)
                        .send()
                        .await?,
                )
                .await?;
            }
        }
        checked(
            self.client
                .post(format!("{}/api/v1/uploads/{id}/complete", job.api))
                .bearer_auth(token)
                .send()
                .await?,
        )
        .await?;
        progress("Processing report".into());
        for _ in 0..180 {
            let mut status: Value = checked(
                self.client
                    .get(format!("{}/api/v1/uploads/{id}", job.api))
                    .bearer_auth(token)
                    .send()
                    .await?,
            )
            .await?;
            if status["status"] == "ready" {
                self.db()?.execute(
                    "UPDATE jobs SET status = 'ready', error = NULL WHERE id = ?",
                    [&job.id],
                )?;
                progress("Report ready".into());
                // Preserve the legacy result while distinguishing a completed re-upload.
                status["newRuns"] = if upload["status"] == "ready" {
                    json!([])
                } else {
                    status["runs"].clone()
                };
                return Ok(status);
            }
            if status["status"] == "failed" {
                bail!(
                    "Processing failed: {}",
                    status["error"].as_str().unwrap_or("unknown error")
                );
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        bail!("Processing is still pending. Retry later to check the same upload.");
    }
    pub async fn device_code(&self, api: &str) -> Result<Value> {
        checked(
            self.client
                .post(format!("{}/api/auth/device/code", validate_api(api)?))
                .json(&json!({"client_id":"azs-uploader"}))
                .send()
                .await?,
        )
        .await
    }
    pub async fn authorize(
        &self,
        api: &str,
        code: &str,
        mut interval: u64,
        progress: impl Fn(String),
    ) -> Result<String> {
        let api = validate_api(api)?;
        for _ in 0..120 {
            tokio::time::sleep(Duration::from_secs(interval.max(1))).await;
            let response = self.client.post(format!("{api}/api/auth/device/token")).json(&json!({"grant_type":"urn:ietf:params:oauth:grant-type:device_code","device_code":code,"client_id":"azs-uploader"})).send().await?;
            let status = response.status();
            let body: Value = response.json().await?;
            if status.is_success() {
                let session = body["access_token"]
                    .as_str()
                    .context("Missing approval token")?;
                let registered = checked(
                    self.client
                        .post(format!("{api}/api/v1/devices"))
                        .bearer_auth(session)
                        .json(&json!({"name":"Azeroth Survivors Uploader"}))
                        .send()
                        .await?,
                )
                .await?;
                return Ok(registered["token"]
                    .as_str()
                    .context("Missing scoped device credential")?
                    .into());
            }
            match body["error"].as_str() {
                Some("authorization_pending") => progress("Waiting for browser approval".into()),
                Some("slow_down") => interval += 5,
                _ => bail!("Device authorization failed: {}", body["error"]),
            }
        }
        bail!("Device approval expired. Sign in again.")
    }
}
fn is_local_api(url: &reqwest::Url) -> bool {
    url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}
pub fn validate_api(api: &str) -> Result<String> {
    let url = reqwest::Url::parse(api)?;
    let local = is_local_api(&url);
    if url.scheme() != "https" && !(url.scheme() == "http" && local) {
        bail!("Use HTTPS for remote APIs");
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        bail!("API must be an origin without credentials or a path");
    }
    Ok(api.trim_end_matches('/').into())
}
async fn checked(response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    let body: Value = response.json().await.context("Invalid API response")?;
    if !status.is_success() {
        bail!(
            "API {}: {}",
            status.as_u16(),
            body["error"].as_str().unwrap_or("request failed")
        );
    }
    Ok(body)
}
