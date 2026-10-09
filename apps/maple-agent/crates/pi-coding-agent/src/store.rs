use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::session::{SessionEntry, SessionHeader};

/// Where a session's entries are kept.
///
/// The session tree lives in memory; a store only receives writes. It is written for the
/// first time once the session has a conversation, so opening a session and leaving
/// without chatting stores nothing.
pub trait SessionStore: Send {
    /// Replace everything stored with `header` and `entries`.
    fn write_all(&mut self, header: &SessionHeader, entries: &[SessionEntry]) -> io::Result<()>;
    /// Append one entry after an earlier `write_all`.
    fn append(&mut self, entry: &SessionEntry) -> io::Result<()>;
    /// The file the session is kept in, for a store that keeps one.
    fn file(&self) -> Option<&Path> {
        None
    }
}

/// A store that keeps nothing, for ephemeral sessions and tests.
pub struct MemoryStore;

impl SessionStore for MemoryStore {
    fn write_all(&mut self, _header: &SessionHeader, _entries: &[SessionEntry]) -> io::Result<()> {
        Ok(())
    }

    fn append(&mut self, _entry: &SessionEntry) -> io::Result<()> {
        Ok(())
    }
}

/// One JSON object per line: a `{"type":"session"}` header, then the entries.
pub struct JsonlStore {
    path: PathBuf,
}

impl JsonlStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read a session file. `Ok(None)` when the file is missing or empty; an error when
    /// it has content but no session header. Lines that do not parse, such as one cut off
    /// by a crash, are skipped.
    pub fn load(path: &Path) -> io::Result<Option<(SessionHeader, Vec<SessionEntry>)>> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut header = None;
        let mut entries = Vec::new();
        let mut saw_content = false;
        for line in BufReader::new(file).split(b'\n') {
            let line = line?;
            if line.trim_ascii().is_empty() {
                continue;
            }
            saw_content = true;
            let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) == Some("session") {
                if header.is_none() {
                    header = serde_json::from_value(value).ok();
                }
            } else if let Ok(entry) = serde_json::from_value::<SessionEntry>(value) {
                entries.push(entry);
            }
        }
        match header {
            Some(header) => Ok(Some((header, entries))),
            None if !saw_content => Ok(None),
            None => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not a session file", path.display()),
            )),
        }
    }

    fn header_line(header: &SessionHeader) -> io::Result<String> {
        let mut value = serde_json::to_value(header)?;
        if let Value::Object(map) = &mut value {
            map.insert("type".into(), Value::String("session".into()));
        }
        Ok(value.to_string())
    }
}

/// One JSON line, written with a single call so it lands whole or not at all.
fn entry_line(entry: &SessionEntry) -> io::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(entry)?;
    line.push(b'\n');
    Ok(line)
}

impl SessionStore for JsonlStore {
    fn file(&self) -> Option<&Path> {
        Some(&self.path)
    }

    fn write_all(&mut self, header: &SessionHeader, entries: &[SessionEntry]) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Write a sibling file and rename it, so a failed write leaves the old file intact.
        let staging = self.path.with_extension("jsonl.tmp");
        {
            let mut file = File::create(&staging)?;
            writeln!(file, "{}", Self::header_line(header)?)?;
            for entry in entries {
                file.write_all(&entry_line(entry)?)?;
            }
            file.sync_all()?;
        }
        fs::rename(&staging, &self.path)
    }

    fn append(&mut self, entry: &SessionEntry) -> io::Result<()> {
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&self.path)?;
        // A line cut off by a crash would swallow this entry; end it first.
        let mut line = Vec::new();
        if file.metadata()?.len() > 0 {
            let mut last = [0u8];
            file.seek(SeekFrom::End(-1))?;
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                line.push(b'\n');
            }
        }
        line.extend(entry_line(entry)?);
        file.write_all(&line)
    }
}
