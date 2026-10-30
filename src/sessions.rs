//! Which conversations are live, as the sessions report it themselves.
//!
//! A bound session registers at `SessionStart`, and again at each `Stop` and
//! `PostToolBatch` (sessions that started before the hooks existed register
//! at their next turn). `SessionEnd` removes the entry. A crash fires no
//! hook, so `live` drops every entry whose process is gone.
//!
//! One file per conversation: `<state>/sessions/<uuid>.json`.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::inbox;
use crate::process;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// The conversation's session UUID.
    pub session_id: String,
    pub nick: String,
    /// The `claude` process, and its start time.
    pub pid: u32,
    pub start: u64,
    /// The session directory.
    pub dir: PathBuf,
    /// A headless session that goniec started.
    pub headless: bool,
}

fn dir() -> Result<PathBuf> {
    Ok(inbox::state_dir()?.join("sessions"))
}

/// A session UUID is a file name here, so it must not name a path.
fn valid_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 64
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Register a live conversation. Writes only when the entry changed, so a
/// hook that runs on every tool batch costs almost nothing.
pub fn register(entry: &Entry) -> Result<()> {
    if !valid_id(&entry.session_id) {
        anyhow::bail!("bad session ID {:?}", entry.session_id);
    }
    let dir = dir()?;
    let path = dir.join(format!("{}.json", entry.session_id));
    if let Ok(text) = fs::read_to_string(&path)
        && serde_json::from_str::<Entry>(&text).is_ok_and(|old| &old == entry)
    {
        return Ok(());
    }
    fs::create_dir_all(&dir)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(entry)?)
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Remove a conversation's entry, for example at `SessionEnd`.
pub fn unregister(session_id: &str) -> Result<()> {
    if !valid_id(session_id) {
        return Ok(());
    }
    match fs::remove_file(dir()?.join(format!("{session_id}.json"))) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Every live conversation. Entries whose process is gone are removed.
pub fn live() -> Result<Vec<Entry>> {
    let dir = dir()?;
    let Ok(files) = fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::new();
    for file in files.filter_map(|f| f.ok()) {
        let path = file.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let entry = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Entry>(&text).ok());
        match entry {
            Some(entry) if process::alive(entry.pid, entry.start) => entries.push(entry),
            _ => {
                let _ = fs::remove_file(&path);
            }
        }
    }
    entries.sort_by(|a, b| (&a.nick, &a.session_id).cmp(&(&b.nick, &b.session_id)));
    Ok(entries)
}

/// The live entry of one conversation, if it is live.
pub fn find(session_id: &str) -> Result<Option<Entry>> {
    Ok(live()?.into_iter().find(|e| e.session_id == session_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        assert!(valid_id("c45a9307-91cc-41a6-b526-1f5c5d31a85a"));
        assert!(!valid_id("../../etc/passwd"));
        assert!(!valid_id(""));
        assert!(!valid_id("a/b"));
    }

    #[test]
    fn register_and_live() {
        let state = std::env::temp_dir().join(format!("goniec-sessions-{}", std::process::id()));
        let _ = fs::remove_dir_all(&state);
        // SAFETY: tests in this module set GONIEC_STATE before any other
        // thread reads it, and no other test uses sessions.
        unsafe { std::env::set_var("GONIEC_STATE", &state) };
        let alive = Entry {
            session_id: "aaaa-1".into(),
            nick: "acme".into(),
            pid: std::process::id(),
            start: process_start(),
            dir: "/tmp".into(),
            headless: false,
        };
        let dead = Entry {
            session_id: "bbbb-2".into(),
            pid: u32::MAX - 1,
            ..alive.clone()
        };
        register(&alive).unwrap();
        register(&dead).unwrap();
        assert_eq!(live().unwrap(), vec![alive.clone()]);
        // The dead entry's file is gone.
        assert!(!dir().unwrap().join("bbbb-2.json").exists());
        assert_eq!(find("aaaa-1").unwrap(), Some(alive.clone()));
        unregister("aaaa-1").unwrap();
        assert!(live().unwrap().is_empty());
        fs::remove_dir_all(&state).unwrap();
    }

    /// This test process's own start time.
    fn process_start() -> u64 {
        #[cfg(target_os = "linux")]
        {
            process::Stat::read(std::process::id()).unwrap().start
        }
        #[cfg(not(target_os = "linux"))]
        {
            0
        }
    }
}
