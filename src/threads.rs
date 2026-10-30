//! Threads and their owners.
//!
//! A thread is one bot's (channel, topic) pair, or its direct-message
//! conversation with one sender. Its key is the reply target:
//! `#channel>topic` or `user:<id>`. Each thread has an owner: one Claude Code
//! conversation, named by its session UUID, with the directory it runs in.
//! Messages of a thread go only to its owner, wherever that conversation is
//! live; if it is live nowhere, the responder resumes it headless.
//!
//! One table per bot: `<state>/threads/<nick>.json`. A lock file guards the
//! read-modify-write, because several sessions and the daemon claim threads.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::inbox::{self, Record, reply_target};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Owner {
    pub session_id: String,
    pub dir: PathBuf,
}

pub type Table = BTreeMap<String, Owner>;

/// The thread a message belongs to.
pub fn key(record: &Record) -> String {
    reply_target(record)
}

fn path(nick: &str) -> Result<PathBuf> {
    Ok(inbox::state_dir()?
        .join("threads")
        .join(format!("{nick}.json")))
}

/// The bot's thread table. A missing or unreadable table is empty.
pub fn load(nick: &str) -> Result<Table> {
    let path = path(nick)?;
    match fs::read_to_string(&path) {
        Ok(text) => Ok(serde_json::from_str(&text).unwrap_or_default()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Table::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// The conversation `session_id` may take this message: it owns the thread,
/// or the thread has no owner yet and `unowned` allows that. Without a
/// session ID, only threads without an owner. A dispatcher bot's
/// interactive sessions pass `unowned = false`: its new threads are for the
/// responder.
pub fn may_take(table: &Table, record: &Record, session_id: Option<&str>, unowned: bool) -> bool {
    match table.get(&key(record)) {
        Some(owner) => Some(owner.session_id.as_str()) == session_id,
        None => unowned,
    }
}

/// Make `owner` the owner of the threads of `records` that have no owner
/// yet. A thread that has one keeps it.
pub fn claim(nick: &str, records: &[Record], owner: &Owner) -> Result<()> {
    let keys: Vec<String> = records.iter().map(key).collect();
    update(nick, |table| {
        for key in keys {
            table.entry(key).or_insert_with(|| owner.clone());
        }
    })
}

/// Change the table under the lock, and write it back atomically.
fn update(nick: &str, change: impl FnOnce(&mut Table)) -> Result<()> {
    let path = path(nick)?;
    let dir = path.parent().expect("a table path has a parent");
    fs::create_dir_all(dir)?;
    let lock = fs::File::create(dir.join(format!("{nick}.lock")))?;
    lock.lock().context("cannot lock the thread table")?;
    let mut table = load(nick)?;
    let before = table.clone();
    change(&mut table);
    if table != before {
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&table)?)?;
        fs::rename(&tmp, &path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: u64, topic: Option<&str>, sender_id: u64) -> Record {
        Record {
            id,
            timestamp: 1_792_900_000,
            sender_id,
            sender_name: "alice".into(),
            sender_is_bot: false,
            channel: topic.map(|_| "ops".into()),
            topic: topic.map(str::to_owned),
            mentioned: true,
            content: "hi".into(),
            earlier: Vec::new(),
        }
    }

    #[test]
    fn keys() {
        assert_eq!(key(&record(1, Some("deploy"), 8)), "#ops>deploy");
        assert_eq!(key(&record(2, None, 8)), "user:8");
    }

    #[test]
    fn ownership() {
        let mut table = Table::new();
        let r = record(1, Some("deploy"), 8);
        assert!(may_take(&table, &r, Some("a"), true));
        assert!(may_take(&table, &r, None, true));
        assert!(!may_take(&table, &r, Some("a"), false));
        table.insert(
            key(&r),
            Owner {
                session_id: "a".into(),
                dir: "/p".into(),
            },
        );
        assert!(may_take(&table, &r, Some("a"), true));
        assert!(may_take(&table, &r, Some("a"), false));
        assert!(!may_take(&table, &r, Some("b"), true));
        assert!(!may_take(&table, &r, None, true));
        // Another topic is still free.
        assert!(may_take(
            &table,
            &record(2, Some("other"), 8),
            Some("b"),
            true
        ));
    }
}
