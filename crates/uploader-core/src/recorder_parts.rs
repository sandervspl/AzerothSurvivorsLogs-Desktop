//! Incremental data-only SavedVariables partitioning. The game file is read-only.
use anyhow::{Context, Result, ensure};
use std::{
    collections::HashSet,
    fs::File,
    io::{BufRead, BufReader, Write},
    path::Path,
};
use tempfile::NamedTempFile;

struct Tokens<R> {
    input: R,
    next: Option<Vec<u8>>,
}

impl<R: BufRead> Tokens<R> {
    fn peek_byte(&mut self) -> Result<Option<u8>> {
        Ok(self.input.fill_buf()?.first().copied())
    }
    fn byte(&mut self) -> Result<Option<u8>> {
        let byte = self.peek_byte()?;
        if byte.is_some() {
            self.input.consume(1);
        }
        Ok(byte)
    }
    fn token(&mut self) -> Result<Vec<u8>> {
        if let Some(token) = self.next.take() {
            return Ok(token);
        }
        loop {
            while self.peek_byte()?.is_some_and(|b| b.is_ascii_whitespace()) {
                self.byte()?;
            }
            let Some(byte) = self.byte()? else {
                return Ok(vec![]);
            };
            if byte == b'-' && self.peek_byte()? == Some(b'-') {
                self.byte()?;
                // Skip comments without allocating their potentially large text.
                loop {
                    let buffer = self.input.fill_buf()?;
                    if buffer.is_empty() {
                        break;
                    }
                    let end = buffer.iter().position(|b| *b == b'\n');
                    let count = end.map_or(buffer.len(), |n| n + 1);
                    self.input.consume(count);
                    if end.is_some() {
                        break;
                    }
                }
                continue;
            }
            let mut token = vec![byte];
            if byte == b'"' || byte == b'\'' {
                loop {
                    let next = self.byte()?.context("Truncated recorder string")?;
                    token.push(next);
                    if next == byte {
                        break;
                    }
                    if next == b'\\' {
                        token.push(self.byte()?.context("Truncated recorder escape")?);
                    }
                    ensure!(token.len() <= 512 * 1024, "Recorder string exceeds limit");
                }
            } else if byte.is_ascii_alphanumeric() || b"_-.".contains(&byte) {
                while self
                    .peek_byte()?
                    .is_some_and(|b| b.is_ascii_alphanumeric() || b"_-.+".contains(&b))
                {
                    token.push(self.byte()?.unwrap());
                    ensure!(token.len() <= 1024, "Invalid recorder token");
                }
            }
            return Ok(token);
        }
    }
    fn peek(&mut self) -> Result<Vec<u8>> {
        let token = self.token()?;
        self.next = Some(token.clone());
        Ok(token)
    }
    fn take(&mut self, expected: &[u8]) -> Result<()> {
        ensure!(self.token()? == expected, "Invalid recorder syntax");
        Ok(())
    }
    fn value(&mut self) -> Result<Vec<u8>> {
        let mut out = self.token()?;
        ensure!(!out.is_empty(), "Truncated recorder value");
        let mut depth = usize::from(out == b"{");
        while depth > 0 {
            let token = self.token()?;
            ensure!(!token.is_empty(), "Truncated recorder table");
            if token == b"{" {
                depth += 1;
            }
            if token == b"}" {
                depth -= 1;
            }
            ensure!(depth <= 12, "Recorder structure limit exceeded");
            out.push(b' ');
            out.extend(token);
            ensure!(
                out.len() <= 128 * 1024 * 1024,
                "Recorder run exceeds 128 MiB"
            );
        }
        super::run_filter::literal(&out)?;
        Ok(out)
    }
    fn key(&mut self, index: usize) -> Result<String> {
        let token = self.peek()?;
        if token == b"[" {
            self.take(b"[")?;
            let value = super::run_filter::literal(&self.value()?)?;
            self.take(b"]")?;
            self.take(b"=")?;
            return match value {
                serde_json::Value::String(key) => Ok(key),
                serde_json::Value::Number(n) => {
                    let n = n.as_f64().context("Invalid recorder key")?;
                    ensure!(n >= 1.0 && n.fract() == 0.0, "Invalid recorder key");
                    Ok(format!("{n:.0}"))
                }
                _ => anyhow::bail!("Invalid recorder key"),
            };
        }
        if token
            .first()
            .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
        {
            let key = String::from_utf8(self.token()?)?;
            self.take(b"=")?;
            return Ok(key);
        }
        Ok(index.to_string())
    }
    fn separator(&mut self) -> Result<()> {
        let token = self.peek()?;
        if token == b"," || token == b";" {
            self.token()?;
        } else {
            ensure!(token == b"}", "Invalid recorder separator");
        }
        Ok(())
    }
}

pub(crate) struct Part {
    pub file: NamedTempFile,
    pub start: f64,
    pub end: f64,
    index: usize,
}

pub(crate) fn split(path: &Path, directory: &Path) -> Result<Vec<Part>> {
    let file = File::open(path)?;
    let before = file.metadata()?;
    let mut input = BufReader::new(file);
    if input.fill_buf()?.starts_with(&[0xef, 0xbb, 0xbf]) {
        input.consume(3);
    }
    let mut tokens = Tokens { input, next: None };
    tokens.take(b"AZSRecorderDB")?;
    tokens.take(b"=")?;
    tokens.take(b"{")?;
    let mut fields = HashSet::new();
    let mut parts = Vec::new();
    let mut run_ids = HashSet::new();
    let mut run_keys = HashSet::new();
    let mut implicit_index = 1;
    let mut schema = None;
    while tokens.peek()? != b"}" {
        let key = tokens.key(fields.len() + 1)?;
        ensure!(fields.insert(key.clone()), "Duplicate recorder field");
        if key == "runs" {
            tokens.take(b"{")?;
            while tokens.peek()? != b"}" {
                ensure!(parts.len() < 8, "Too many recorder segments");
                let implicit = tokens.peek()? == b"{";
                let index = tokens.key(implicit_index)?.parse::<usize>()?;
                if implicit {
                    implicit_index += 1;
                }
                ensure!(
                    index > 0 && run_keys.insert(index),
                    "Duplicate or invalid recorder run key"
                );
                let value = tokens.value()?;
                let run = super::run_filter::literal(&value)?;
                let id = run["id"].as_str().context("Invalid recorder run ID")?;
                ensure!(run_ids.insert(id.to_owned()), "Duplicate recorder run ID");
                let events = run["events"]
                    .as_object()
                    .context("Invalid recorder events")?;
                let time = |index: usize| -> Result<f64> {
                    let event: serde_json::Value = serde_json::from_str(
                        events
                            .get(&index.to_string())
                            .and_then(|value| value.as_str())
                            .context("Invalid recorder event")?,
                    )?;
                    event["wallTime"]
                        .as_f64()
                        .context("Invalid recorder timestamp")
                };
                let (start, end) = if events.is_empty() {
                    (0.0, 0.0)
                } else {
                    (time(1)?, time(events.len())? + 999.0)
                };
                drop(run);
                let mut part = NamedTempFile::new_in(directory)?;
                part.write_all(b"AZSRecorderDB={schemaVersion=1,runs={")?;
                part.write_all(&value)?;
                part.write_all(b"}}\n")?;
                part.as_file().sync_all()?;
                parts.push(Part {
                    file: part,
                    start,
                    end,
                    index,
                });
                tokens.separator()?;
            }
            tokens.take(b"}")?;
        } else {
            let value = tokens.value()?;
            if key == "schemaVersion" {
                schema = super::run_filter::literal(&value)?.as_f64();
            }
        }
        tokens.separator()?;
    }
    tokens.take(b"}")?;
    ensure!(
        tokens.token()?.is_empty(),
        "Executable content after recorder"
    );
    ensure!(
        schema == Some(1.0) && fields.contains("runs"),
        "Unsupported recorder schema"
    );
    let after = tokens.input.get_ref().metadata()?;
    ensure!(
        before.len() == after.len() && before.modified()? == after.modified()?,
        "Source changed while copying; save and retry"
    );
    ensure!(
        (1..=parts.len()).all(|index| run_keys.contains(&index)),
        "Invalid recorder run ordering"
    );
    parts.sort_by_key(|part| part.index);
    parts.sort_by(|a, b| a.start.total_cmp(&b.start));
    for index in 0..parts.len().saturating_sub(1) {
        parts[index].end = parts[index].end.min(parts[index + 1].start - 1.0);
    }
    Ok(parts)
}
