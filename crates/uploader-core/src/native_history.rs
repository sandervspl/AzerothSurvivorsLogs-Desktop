use crate::native_save::Reader;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    thread,
    time::Duration,
};
fn database(state: &Path) -> Result<Connection> {
    fs::create_dir_all(state)?;
    let db = Connection::open(state.join("native-talents.db"))?;
    db.busy_timeout(Duration::from_secs(2))?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS coverage (id TEXT PRIMARY KEY, source TEXT NOT NULL, start INTEGER NOT NULL, end INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS observations (session TEXT NOT NULL, time INTEGER NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(session,time));")?;
    Ok(db)
}
fn source_id(save: &Path, reader: &Reader) -> Result<String> {
    Ok(crate::sha256(
        format!(
            "{}:{}",
            save.canonicalize()?.display(),
            reader.executable_sha256
        )
        .as_bytes(),
    ))
}
pub struct Monitor {
    ready: Arc<AtomicBool>,
    compatibility: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
impl Monitor {
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }
    /// 0: server support unknown, 1: supported, 2: unsupported.
    pub fn compatibility(&self) -> u8 {
        self.compatibility.load(Ordering::Relaxed)
    }
    pub fn start(state: PathBuf, save: PathBuf, executable: PathBuf) -> Self {
        Self::start_with_api(state, save, executable, None)
    }
    pub fn start_with_api(
        state: PathBuf,
        save: PathBuf,
        executable: PathBuf,
        api: Option<String>,
    ) -> Self {
        let compatibility = Arc::new(AtomicU8::new(0));
        let support = compatibility.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let ready = Arc::new(AtomicBool::new(false));
        let ready_flag = ready.clone();
        let handle = thread::spawn(move || {
            let Ok(reader) = Reader::open(&executable) else {
                return;
            };
            if let Some(api) = api {
                let stop = flag.clone();
                let executable_hash = reader.executable_sha256.clone();
                // Network checks never block or interrupt local capture.
                thread::spawn(move || {
                    let Ok(runtime) = tokio::runtime::Runtime::new() else {
                        return;
                    };
                    runtime.block_on(async move {
                        let Ok(client) = reqwest::Client::builder()
                            .timeout(Duration::from_secs(5))
                            .redirect(reqwest::redirect::Policy::none())
                            .build()
                        else {
                            return;
                        };
                        while !stop.load(Ordering::Relaxed) {
                            let result = async {
                                let value: Value = client
                                    .get(format!(
                                        "{}/api/v1/talent-compatibility",
                                        api.trim_end_matches('/')
                                    ))
                                    .send()
                                    .await?
                                    .error_for_status()?
                                    .json()
                                    .await?;
                                Ok::<_, reqwest::Error>(value)
                            }
                            .await;
                            let status = match result {
                                Ok(value) => match value["executableSha256"].as_array() {
                                    Some(hashes) => {
                                        if hashes
                                            .iter()
                                            .any(|hash| hash.as_str() == Some(&executable_hash))
                                        {
                                            1
                                        } else {
                                            2
                                        }
                                    }
                                    None => 0,
                                },
                                Err(_) => 0,
                            };
                            support.store(status, Ordering::Relaxed);
                            for _ in 0..60 {
                                if stop.load(Ordering::Relaxed) {
                                    return;
                                }
                                tokio::time::sleep(Duration::from_secs(1)).await;
                            }
                        }
                    });
                });
            }
            let Ok(source) = source_id(&save, &reader) else {
                return;
            };
            let Ok(mut db) = database(&state) else {
                return;
            };
            let mut session: Option<String> = None;
            let mut previous = 0i64;
            let mut persisted = 0i64;
            let mut hash = String::new();
            while !flag.load(Ordering::Relaxed) {
                let result = (|| -> Result<()> {
                    let snapshot = reader.read(&save)?;
                    let now = snapshot["observedAt"]
                        .as_i64()
                        .context("Missing native observation time")?;
                    let fresh = session.is_none() || now - previous > 1500 || now < previous;
                    let id = if fresh {
                        format!("{}-{}-{}", std::process::id(), now, source)
                    } else {
                        session.clone().context("Missing monitor session")?
                    };
                    // Save nonces, achievements and counters can change on every write.
                    // Retain actual build revisions so those writes cannot evict run-start state.
                    let next = crate::sha256(
                        serde_json::to_string(&json!({
                            "classes": snapshot["classes"],
                            "equipment": snapshot["equipment"],
                            "clientBuild": snapshot["clientBuild"],
                        }))?
                        .as_bytes(),
                    );
                    if !fresh && hash == next && now - persisted < 1000 {
                        previous = now;
                        return Ok(());
                    }
                    let tx = db.transaction()?;
                    tx.execute("INSERT INTO coverage (id,source,start,end) VALUES (?,?,?,?) ON CONFLICT(id) DO UPDATE SET end=excluded.end", params![id,source,now,now])?;
                    if fresh || hash != next {
                        tx.execute(
                            "INSERT INTO observations (session,time,payload) VALUES (?,?,?)",
                            params![id, now, serde_json::to_string(&snapshot)?],
                        )?;
                    }
                    tx.commit()?;
                    ready_flag.store(true, Ordering::Relaxed);
                    session = Some(id);
                    hash = next;
                    previous = now;
                    persisted = now;
                    Ok(())
                })();
                if result.is_err() {
                    ready_flag.store(false, Ordering::Relaxed);
                    session = None;
                    hash.clear();
                }
                thread::sleep(Duration::from_millis(250));
            }
        });
        Self {
            compatibility,
            ready,
            stop,
            handle: Some(handle),
        }
    }
}
/// Immutable upload bundle: no history means no automatic per-run attribution.
pub fn bundle(state: &Path, save: &Path, executable: &Path) -> Result<Value> {
    let reader = Reader::open(executable)?;
    let latest = reader.read(save)?;
    let source = source_id(save, &reader)?;
    let mut db = database(state)?;
    let tx = db.transaction()?;
    let mut stmt = tx.prepare("SELECT o.payload FROM observations o JOIN coverage c ON o.session=c.id WHERE c.source=? ORDER BY o.time DESC LIMIT 128")?;
    let mut observations: Vec<Value> = stmt
        .query_map([&source], |row| row.get::<_, String>(0))?
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect::<Result<_>>()?;
    observations.sort_by_key(|snapshot| snapshot["observedAt"].as_i64().unwrap_or_default());
    observations.dedup_by(|a, b| {
        a["observedAt"] == b["observedAt"] && a["sourceSha256"] == b["sourceSha256"]
    });
    let mut stmt =
        tx.prepare("SELECT start,end FROM coverage WHERE source=? ORDER BY start DESC LIMIT 32")?;
    let coverage: Vec<Value> = stmt
        .query_map([&source], |row| {
            Ok(json!({"from":row.get::<_,i64>(0)?,"to":row.get::<_,i64>(1)?}))
        })?
        .collect::<std::result::Result<_, _>>()?;
    loop {
        let result = json!({"schemaVersion":1,"kind":"azs-native-save-history","latest":latest,"observations":observations,"coverage":coverage});
        if serde_json::to_vec(&result)?.len() <= 1024 * 1024 {
            return Ok(result);
        }
        if observations.is_empty() {
            bail!("Native talent snapshot is too large");
        }
        observations.remove(0);
    }
}
