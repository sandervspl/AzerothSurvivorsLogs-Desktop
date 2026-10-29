use anyhow::{Context, Result, bail, ensure};
use chrono::{Datelike, NaiveDateTime};
use serde_json::{Map, Value};

// A bounded data reader, never a Lua interpreter. Accept only SavedVariables
// literals; calls, expressions and trailing statements fail closed.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    nodes: usize,
}
impl Reader<'_> {
    fn skip(&mut self) {
        loop {
            while self
                .bytes
                .get(self.pos)
                .is_some_and(u8::is_ascii_whitespace)
            {
                self.pos += 1;
            }
            if self.bytes[self.pos..].starts_with(b"--") {
                while self.bytes.get(self.pos).is_some_and(|b| *b != b'\n') {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }
    fn take(&mut self, byte: u8) -> Result<()> {
        self.skip();
        ensure!(
            self.bytes.get(self.pos) == Some(&byte),
            "Invalid recorder syntax"
        );
        self.pos += 1;
        Ok(())
    }
    fn string(&mut self) -> Result<String> {
        let quote = self.bytes[self.pos];
        self.pos += 1;
        let mut out = Vec::new();
        while let Some(&byte) = self.bytes.get(self.pos) {
            self.pos += 1;
            if byte == quote {
                return Ok(String::from_utf8(out)?);
            }
            if byte == b'\\' {
                let byte = *self
                    .bytes
                    .get(self.pos)
                    .context("Truncated recorder escape")?;
                self.pos += 1;
                if byte.is_ascii_digit() {
                    let mut number = u16::from(byte - b'0');
                    for _ in 0..2 {
                        if let Some(&next) = self.bytes.get(self.pos).filter(|b| b.is_ascii_digit())
                        {
                            number = number * 10 + u16::from(next - b'0');
                            self.pos += 1;
                        } else {
                            break;
                        }
                    }
                    out.push(u8::try_from(number)?);
                } else {
                    out.push(match byte {
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        b'\\' | b'"' | b'\'' | b'\n' => byte,
                        _ => bail!("Unsupported recorder escape"),
                    });
                }
            } else {
                out.push(byte);
            }
            ensure!(out.len() <= 65536, "Recorder string exceeds 64 KiB");
        }
        bail!("Truncated recorder string")
    }
    fn word(&mut self) -> String {
        let start = self.pos;
        while self
            .bytes
            .get(self.pos)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
        {
            self.pos += 1;
        }
        String::from_utf8_lossy(&self.bytes[start..self.pos]).into_owned()
    }
    fn value(&mut self, depth: usize) -> Result<Value> {
        self.skip();
        self.nodes += 1;
        ensure!(
            depth <= 12 && self.nodes <= 300000,
            "Recorder structure limit exceeded"
        );
        match self.bytes.get(self.pos).copied() {
            Some(b'"' | b'\'') => Ok(Value::String(self.string()?)),
            Some(b'{') => {
                self.pos += 1;
                let mut object = Map::new();
                let mut index = 1;
                loop {
                    self.skip();
                    if self.bytes.get(self.pos) == Some(&b'}') {
                        self.pos += 1;
                        return Ok(Value::Object(object));
                    }
                    let saved = self.pos;
                    let key = if self.bytes.get(self.pos) == Some(&b'[') {
                        self.pos += 1;
                        let key = self.value(depth + 1)?;
                        self.take(b']')?;
                        self.take(b'=')?;
                        match key {
                            Value::String(s) => s,
                            Value::Number(n) => {
                                let number = n.as_f64().context("Invalid numeric key")?;
                                ensure!(
                                    number >= 1.0 && number.fract() == 0.0,
                                    "Invalid numeric key"
                                );
                                format!("{number:.0}")
                            }
                            _ => bail!("Invalid recorder key"),
                        }
                    } else {
                        let word = self.word();
                        self.skip();
                        if !word.is_empty() && self.bytes.get(self.pos) == Some(&b'=') {
                            self.pos += 1;
                            word
                        } else {
                            self.pos = saved;
                            let key = index.to_string();
                            index += 1;
                            key
                        }
                    };
                    ensure!(!object.contains_key(&key), "Duplicate recorder field");
                    object.insert(key, self.value(depth + 1)?);
                    self.skip();
                    match self.bytes.get(self.pos) {
                        Some(b',' | b';') => self.pos += 1,
                        Some(b'}') => {}
                        _ => bail!("Invalid recorder separator"),
                    }
                }
            }
            Some(b'-' | b'0'..=b'9' | b'.') => {
                let start = self.pos;
                while self
                    .bytes
                    .get(self.pos)
                    .is_some_and(|b| b.is_ascii_digit() || b"-.eE+".contains(b))
                {
                    self.pos += 1;
                }
                let number: f64 = std::str::from_utf8(&self.bytes[start..self.pos])?.parse()?;
                Ok(Value::Number(
                    serde_json::Number::from_f64(number).context("Invalid recorder number")?,
                ))
            }
            _ => match self.word().as_str() {
                "true" => Ok(Value::Bool(true)),
                "false" => Ok(Value::Bool(false)),
                "nil" => Ok(Value::Null),
                _ => bail!("Executable or unsupported Lua in recorder"),
            },
        }
    }
}

fn ordered(value: &Value) -> Result<Vec<&Value>> {
    let mut entries = value
        .as_object()
        .context("Invalid recorder list")?
        .iter()
        .map(|(key, value)| Ok((key.parse::<usize>()?, value)))
        .collect::<Result<Vec<_>>>()?;
    entries.sort_by_key(|(key, _)| *key);
    ensure!(
        entries
            .iter()
            .enumerate()
            .all(|(index, (key, _))| *key == index + 1),
        "Non-contiguous recorder list"
    );
    Ok(entries.into_iter().map(|(_, value)| value).collect())
}
fn lua(value: &Value) -> String {
    match value {
        Value::Object(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(key, value)| format!(
                    "[{}]={}",
                    lua(&Value::String(key.clone())),
                    lua(value)
                ))
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::String(value) => {
            let mut out = String::from("\"");
            for c in value.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '"' => out.push_str("\\\""),
                    c if c.is_ascii_control() => out.push_str(&format!("\\{:03}", c as u32)),
                    c => out.push(c),
                }
            }
            out.push('"');
            out
        }
        Value::Null => "nil".into(),
        _ => value.to_string(),
    }
}

pub struct Filtered {
    pub combat: Vec<u8>,
    pub recorder: Option<Vec<u8>>,
    pub ignored: bool,
    pub year: i32,
}
pub fn filter(combat: &[u8], recorder: Option<&[u8]>, year: i32, offset: i32) -> Result<Filtered> {
    let mut intervals = Vec::new();
    let recorder = if let Some(bytes) = recorder {
        let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
        let mut reader = Reader {
            bytes,
            pos: 0,
            nodes: 0,
        };
        reader.skip();
        ensure!(
            reader.word() == "AZSRecorderDB",
            "Wrong recorder file; select benilla-config/saved/AzerothSurvivorsRecorder.lua"
        );
        reader.take(b'=')?;
        let mut db = reader.value(0)?;
        reader.skip();
        ensure!(
            reader.pos == bytes.len(),
            "Executable content after recorder"
        );
        ensure!(
            db["schemaVersion"].as_f64() == Some(1.0),
            "Unsupported recorder schema"
        );
        let runs = ordered(&db["runs"])?;
        ensure!(runs.len() <= 8, "Too many recorder segments");
        let mut kept = Map::new();
        let mut bounds = Vec::new();
        for run in runs {
            let events = ordered(&run["events"])?;
            ensure!(events.len() <= 25000, "Too many recorder events");
            if events.is_empty() {
                continue;
            }
            let events = events
                .iter()
                .map(|value| {
                    Ok(serde_json::from_str::<Value>(
                        value.as_str().context("Invalid recorder event")?,
                    )?)
                })
                .collect::<Result<Vec<_>>>()?;
            ensure!(
                events[0]["kind"] == "run_start",
                "Missing recorder baseline"
            );
            let mut wall = 0.0;
            let mut sequence = 0;
            for event in &events {
                let time = event["wallTime"]
                    .as_f64()
                    .context("Invalid recorder timestamp")?;
                let seq = event["seq"].as_u64().context("Invalid recorder sequence")?;
                ensure!(
                    time > 0.0 && time >= wall && seq > sequence && event["runId"] == run["id"],
                    "Invalid recorder identity or ordering"
                );
                if let Some(time) = event.get("runTime") {
                    ensure!(
                        time.as_f64().is_some_and(|time| time >= 0.0),
                        "Invalid run clock"
                    );
                }
                wall = time;
                sequence = seq;
            }
            let start = events[0]["wallTime"]
                .as_f64()
                .context("Missing recorder start")?;
            let end = events.last().unwrap()["wallTime"]
                .as_f64()
                .context("Missing recorder end")?
                + 999.0;
            let duration = events
                .iter()
                .filter_map(|event| event["runTime"].as_f64())
                .fold(0.0, f64::max);
            bounds.push((start, end, duration >= 300.0, run.clone()));
        }
        bounds.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (index, (start, end, qualifies, run)) in bounds.iter().enumerate() {
            if *qualifies {
                intervals.push((
                    *start,
                    end.min(
                        bounds
                            .get(index + 1)
                            .map_or(f64::INFINITY, |next| next.0 - 1.0),
                    ),
                ));
                kept.insert((kept.len() + 1).to_string(), run.clone());
            }
        }
        db["runs"] = Value::Object(kept);
        Some(format!("AZSRecorderDB={}\n", lua(&db)).into_bytes())
    } else {
        None
    };
    let text = std::str::from_utf8(combat).context("Combat log is not UTF-8")?;
    let mut current_year = year;
    let mut previous = None;
    let mut lines = Vec::new();
    for line in text.lines() {
        let Some((stamp, _)) = line.split_once("  ") else {
            continue;
        };
        let Ok(mut date) = NaiveDateTime::parse_from_str(
            &format!("{current_year}/{stamp}"),
            "%Y/%m/%d %H:%M:%S%.f",
        ) else {
            continue;
        };
        if previous.is_some_and(|last| date.and_utc().timestamp_millis() < last - 86400000) {
            current_year += 1;
            date = date
                .with_year(current_year)
                .context("Invalid year rollover")?;
        }
        previous = Some(date.and_utc().timestamp_millis());
        let time = date.and_utc().timestamp_millis() as f64 - f64::from(offset) * 60000.0;
        lines.push((time, line, current_year));
    }
    let qualifies = if recorder.is_some() {
        !intervals.is_empty()
    } else {
        lines
            .first()
            .zip(lines.last())
            .is_some_and(|(first, last)| last.0 - first.0 >= 300000.0)
    };
    let mut out = Vec::new();
    let mut first_year = None;
    if qualifies {
        for (time, line, line_year) in lines {
            if recorder.is_none()
                || intervals
                    .iter()
                    .any(|(start, end)| time >= *start && time <= *end)
            {
                first_year.get_or_insert(line_year);
                out.extend_from_slice(line.as_bytes());
                out.push(b'\n');
            }
        }
    }
    if qualifies && out.is_empty() {
        bail!(
            "No combat events match the recorder. Select the matching combat log and check the recording year and time zone; no files were uploaded."
        );
    }
    Ok(Filtered {
        combat: out,
        recorder,
        ignored: !qualifies,
        year: first_year.unwrap_or(year),
    })
}
