use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha1::Sha1;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

type HmacSha1 = Hmac<Sha1>;
fn mac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut value = HmacSha1::new_from_slice(key).expect("HMAC accepts all key lengths");
    value.update(data);
    value.finalize().into_bytes().to_vec()
}
fn read_stable(path: &Path, limit: u64) -> Result<(Vec<u8>, u64)> {
    let before = fs::metadata(path).with_context(|| format!("Cannot read {}", path.display()))?;
    if before.len() == 0 || before.len() > limit {
        bail!("Native talent source has an invalid size");
    }
    let bytes = fs::read(path)?;
    let after = fs::metadata(path)?;
    if before.len() != bytes.len() as u64
        || before.len() != after.len()
        || before.modified()? != after.modified()?
    {
        bail!("Native talent source changed while reading; retry after saving");
    }
    Ok((
        bytes,
        before.modified()?.duration_since(UNIX_EPOCH)?.as_millis() as u64,
    ))
}
fn ranks(text: &str) -> Result<BTreeMap<String, u8>> {
    let mut result = BTreeMap::new();
    if text.is_empty() {
        return Ok(result);
    }
    for pair in text.split(',') {
        let (key, rank) = pair
            .split_once(':')
            .context("Malformed native talent rank")?;
        if key.is_empty()
            || key.len() > 100
            || !key
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        {
            bail!("Invalid native talent key");
        }
        let rank: u8 = rank.parse().context("Invalid native talent rank")?;
        if result.insert(key.into(), rank).is_some() {
            bail!("Duplicate native talent key");
        }
    }
    Ok(result)
}
fn class(text: &str) -> Result<u8> {
    let id: u8 = text.parse()?;
    if ![1, 2, 3, 4, 5, 7, 8, 9, 11].contains(&id) {
        bail!("Unknown native talent class");
    }
    Ok(id)
}
/// Read-only: only talent/spec rows leave the desktop, never the key or full save.
pub struct Reader {
    key: Vec<u8>,
    pub executable_sha256: String,
    client_build: Option<Value>,
    executable_path: PathBuf,
    executable_modified: u64,
    executable_size: u64,
}
impl Reader {
    pub fn open(executable: &Path) -> Result<Self> {
        let (exe, executable_modified) = read_stable(executable, 512 * 1024 * 1024)?;
        let prefix: Vec<u8> = b"azeroth-survivors/sa".iter().map(|b| b ^ 0x36).collect();
        let at = memchr::memmem::find(&exe, &prefix)
            .context("Save reader does not support this executable")?;
        if at + 64 > exe.len() || memchr::memmem::find(&exe[at + prefix.len()..], &prefix).is_some()
        {
            bail!("Ambiguous native save key layout");
        }
        let block: Vec<u8> = exe[at..at + 64].iter().map(|b| b ^ 0x36).collect();
        let end = (prefix.len()..64)
            .find(|i| block[*i..(i / 16 + 1) * 16].iter().all(|b| *b == 0))
            .unwrap_or(64);
        let key = block[..end].to_vec();
        Ok(Self {
            key,
            executable_sha256: crate::sha256(&exe),
            client_build: crate::client_build::extract(&exe),
            executable_path: executable.into(),
            executable_modified,
            executable_size: exe.len() as u64,
        })
    }
    pub fn read(&self, save: &Path) -> Result<Value> {
        let metadata = fs::metadata(&self.executable_path)?;
        if metadata.len() != self.executable_size
            || metadata.modified()?.duration_since(UNIX_EPOCH)?.as_millis() as u64
                != self.executable_modified
        {
            bail!("Game executable changed; restart monitoring to capture the new build");
        }
        let (sealed, modified) = read_stable(save, 1024 * 1024)?;
        let key = &self.key;
        let body = std::str::from_utf8(&sealed)?
            .trim()
            .strip_prefix("RSE1:")
            .context("Native save is not RSE1")?;
        let raw = STANDARD
            .decode(body)
            .context("Native save has invalid base64")?;
        if raw.len() < 36 {
            bail!("Native save is truncated");
        }
        let (message, tag) = raw.split_at(raw.len() - 20);
        let mut verifier = HmacSha1::new_from_slice(key)?;
        verifier.update(b"tag");
        verifier.update(message);
        verifier
            .verify_slice(tag)
            .context("Native save seal does not match the executable")?;
        let (header, encrypted) = message.split_at(16);
        let mut plain = Vec::with_capacity(encrypted.len());
        for (counter, chunk) in encrypted.chunks(20).enumerate() {
            let mut input = b"ks".to_vec();
            input.extend(header);
            input.extend((counter as u64).to_le_bytes());
            let stream = mac(key, &input);
            plain.extend(chunk.iter().zip(stream).map(|(a, b)| a ^ b));
        }
        let mut classes = BTreeMap::new();
        let mut specs = BTreeMap::new();
        for line in std::str::from_utf8(&plain)?.lines() {
            let fields: Vec<_> = line.split('|').collect();
            match fields[0] {
                "talent" => {
                    if fields.len() != 4 {
                        bail!("Malformed native talent row");
                    }
                    let id = class(fields[1])?;
                    let _: u64 = fields[2].parse().context("Invalid talent XP")?;
                    if classes.insert(id, ranks(fields[3])?).is_some() {
                        bail!("Duplicate native talent class");
                    }
                }
                "spec" => {
                    if fields.len() != 6 {
                        bail!("Malformed native spec row");
                    }
                    let id = class(fields[1])?;
                    let active: u8 = fields[2].parse()?;
                    if active > 2 {
                        bail!("Invalid active native spec");
                    }
                    let builds = vec![ranks(fields[3])?, ranks(fields[4])?, ranks(fields[5])?];
                    if specs.insert(id, (active, builds)).is_some() {
                        bail!("Duplicate native specs");
                    }
                }
                _ => {}
            }
        }
        let mut allocations = vec![];
        for (id, ranks) in classes {
            let (active, builds) = if let Some((active, builds)) = specs.remove(&id) {
                if builds[active as usize] != ranks {
                    bail!("Native active spec disagrees with the saved talent ranks");
                }
                (Some(active), builds)
            } else {
                (None, vec![])
            };
            allocations
                .push(json!({"classId":id,"activeSpec":active,"ranks":ranks,"specs":builds}));
        }
        if !specs.is_empty() {
            bail!("Native spec has no corresponding talent row");
        }
        Ok(
            json!({"schemaVersion":1,"kind":"azs-native-save","observedAt":chrono::Utc::now().timestamp_millis(),"sourceModifiedAt":modified,"sourceSha256":crate::sha256(&sealed),"executableSha256":self.executable_sha256,"clientBuild":self.client_build,"classes":allocations}),
        )
    }
}
pub fn snapshot(save: &Path, executable: &Path) -> Result<Value> {
    Reader::open(executable)?.read(save)
}

pub fn discover(recording: Option<&Path>) -> Option<(PathBuf, PathBuf)> {
    let save = std::env::var_os("WOW_ROGUE_SAVE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("APPDATA")
                .map(|p| PathBuf::from(p).join("wenilla-rogue/characters.txt"))
        })?;
    let exe = recording
        .and_then(|recording| {
            recording
                .ancestors()
                .map(|p| p.join("AzerothSurvivors.exe"))
                .find(|p| p.is_file())
        })
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(|p| PathBuf::from(p).join("Documents/AzerothSurvivors/AzerothSurvivors.exe"))
                .filter(|p| p.is_file())
        })?;
    save.is_file().then_some((save, exe))
}
