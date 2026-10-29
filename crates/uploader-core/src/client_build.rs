use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{fs, path::Path};

fn word(bytes: &[u8], at: usize, size: usize) -> Option<u64> {
    let data = bytes.get(at..at.checked_add(size)?)?;
    Some(data.iter().enumerate().fold(0, |value, (index, byte)| {
        value | (u64::from(*byte) << (index * 8))
    }))
}

/// The title formatter's preceding pointer/length pair names the displayed release.
/// Resolve the PE virtual address as data; never execute the image.
fn displayed_version(bytes: &[u8]) -> Option<(String, usize)> {
    let pe = usize::try_from(word(bytes, 0x3c, 4)?).ok()?;
    if bytes.get(pe..pe.checked_add(4)?)? != b"PE\0\0" {
        return None;
    }
    let optional = pe.checked_add(24)?;
    if word(bytes, optional, 2)? != 0x20b {
        return None;
    }
    let base = word(bytes, optional + 24, 8)?;
    let section_count = usize::try_from(word(bytes, pe + 6, 2)?).ok()?;
    if section_count > 96 {
        return None;
    }
    let sections = optional.checked_add(usize::try_from(word(bytes, pe + 20, 2)?).ok()?)?;
    let mut found = vec![];
    for at in memchr::memmem::find_iter(bytes, b"\x01v\xc0\x02 (\xc0\x01)\0") {
        let pointer = word(bytes, at.checked_sub(16)?, 8)?;
        let length = usize::try_from(word(bytes, at.checked_sub(8)?, 8)?).ok()?;
        if length == 0 || length > 100 {
            return None;
        }
        let rva = pointer.checked_sub(base)?;
        let mut offsets = vec![];
        for index in 0..section_count {
            let section = sections.checked_add(index * 40)?;
            let start = word(bytes, section + 12, 4)?;
            let size = word(bytes, section + 16, 4)?;
            if rva >= start && rva.checked_add(length as u64)? <= start.checked_add(size)? {
                let offset = word(bytes, section + 20, 4)?.checked_add(rva - start)?;
                offsets.push(usize::try_from(offset).ok()?);
            }
        }
        if offsets.len() != 1 {
            return None;
        }
        let offset = offsets[0];
        let version = std::str::from_utf8(bytes.get(offset..offset.checked_add(length)?)?).ok()?;
        semver::Version::parse(version).ok()?;
        found.push((version.to_string(), offset));
    }
    if found.len() == 1 { found.pop() } else { None }
}

/// Read the adjacent Rust build-id string slices, not dependency versions or PE timestamps.
/// Unknown/ambiguous compiler layouts deliberately have no descriptor.
pub fn extract(bytes: &[u8]) -> Option<Value> {
    if !bytes.starts_with(b"MZ") {
        return None;
    }
    let mut matches = vec![];
    let release = displayed_version(bytes);
    for profile in [b"release\0".as_slice(), b"debug\0".as_slice()] {
        for end in memchr::memmem::find_iter(bytes, profile) {
            if end < 256 {
                continue;
            }
            let Ok(date) = std::str::from_utf8(&bytes[end - 10..end]) else {
                continue;
            };
            if chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_err() {
                continue;
            }
            let short = &bytes[end - 18..end - 10];
            let commit = &bytes[end - 58..end - 18];
            if !commit.iter().all(u8::is_ascii_hexdigit) || short != &commit[..8] {
                continue;
            }
            let commit_at = end - 58;
            let Some(start) = bytes[commit_at - 128..commit_at]
                .iter()
                .rposition(|byte| *byte == b'v')
                .map(|at| at + commit_at - 128)
            else {
                continue;
            };
            let Ok(build) = std::str::from_utf8(&bytes[start..commit_at]) else {
                continue;
            };
            let Some((tag, suffix)) = build[1..].split_once('-') else {
                continue;
            };
            let Some((distance, hash)) = suffix.split_once("-g") else {
                continue;
            };
            if semver::Version::parse(tag).is_err()
                || distance.parse::<u64>().is_err()
                || hash.as_bytes() != short
                || start < tag.len()
                || &bytes[start - tag.len()..start] != tag.as_bytes()
            {
                continue;
            }
            matches.push(json!({
                "gameVersion": release.as_ref().map(|(version, _)| version),
                "packageVersion": tag, "gameBuild": build,
                "releaseVersionOffset": release.as_ref().map(|(_, offset)| offset),
                "commit": std::str::from_utf8(commit).ok()?, "buildDate": date,
                "profile": std::str::from_utf8(&profile[..profile.len()-1]).ok()?,
                "metadataOffset": start - tag.len(),
            }));
        }
    }
    if matches.len() == 1 {
        matches.pop()
    } else {
        None
    }
}

pub fn inspect(executable: &Path) -> Result<Value> {
    let before = fs::metadata(executable)?;
    ensure!(before.len() <= 512 * 1024 * 1024, "Executable is too large");
    let bytes = fs::read(executable).context("Cannot read game executable")?;
    let after = fs::metadata(executable)?;
    ensure!(
        before.len() == after.len()
            && after.len() == bytes.len() as u64
            && before.modified()? == after.modified()?,
        "Executable changed while reading; retry"
    );
    Ok(json!({
        "source": "installed-executable", "observedAt": chrono::Utc::now().timestamp_millis(),
        "executableSha256": crate::sha256(&bytes), "clientBuild": extract(&bytes),
    }))
}
