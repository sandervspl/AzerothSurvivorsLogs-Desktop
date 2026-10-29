use crate::{Job, distribution};
use anyhow::Result;
use rusqlite::{Connection, TransactionBehavior};
use std::{
    collections::HashMap,
    fs,
    path::Path,
    time::{Duration, SystemTime},
};

const RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

pub(crate) fn initialize(db: &mut Connection) -> Result<()> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let has_finished = {
        let mut statement = tx.prepare("PRAGMA table_info(jobs)")?;
        let names = statement.query_map([], |row| row.get::<_, String>(1))?;
        names
            .collect::<std::result::Result<Vec<_>, _>>()?
            .iter()
            .any(|name| name == "finished_at")
    };
    if !has_finished {
        tx.execute_batch("ALTER TABLE jobs ADD COLUMN finished_at INTEGER;")?;
    }
    // Older queue rows have no completion time. Give them a full policy window
    // from first observation instead of inferring a destructive earlier deadline.
    tx.execute("UPDATE jobs SET finished_at = ? WHERE status IN ('ready','invalid') AND finished_at IS NULL", [chrono::Utc::now().timestamp_millis()])?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn prune(state: &Path, db: &Connection) -> Result<()> {
    let spool = state.join("spool");
    let metadata = fs::symlink_metadata(&spool)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Ok(());
        }
    }
    let Ok(_lock) = distribution::lock(&spool.join("storage.lock")) else {
        return Ok(());
    };
    let now = SystemTime::now();
    let cutoff = chrono::Utc::now().timestamp_millis() - RETENTION.as_millis() as i64;
    let mut protect = HashMap::<String, bool>::new();
    let mut statement = db.prepare("SELECT payload,status,finished_at FROM jobs")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<i64>>(2)?,
        ))
    })?;
    for row in rows {
        let (payload, status, finished) = row?;
        let job: Job = serde_json::from_str(&payload)?;
        let keep = !matches!(status.as_str(), "ready" | "invalid")
            || finished.is_none_or(|time| time > cutoff);
        for source in job.manifest.sources {
            protect
                .entry(source.sha256)
                .and_modify(|value| *value |= keep)
                .or_insert(keep);
        }
    }
    for entry in fs::read_dir(&spool)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(hash) = name.strip_suffix(".gz") else {
            continue;
        };
        if hash.len() != 64
            || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            || protect.get(hash) == Some(&true)
        {
            continue;
        }
        if now
            .duration_since(entry.metadata()?.modified()?)
            .unwrap_or_default()
            >= RETENTION
        {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}
