//! One-time approvals for calls the auto-mode classifier blocked.
//!
//! A headless session runs in auto mode. When the classifier blocks a call,
//! `goniec hook denied` asks the trusted user in the chat. On a yes it
//! records a grant for exactly that call and tells the model to retry. On
//! the retry, `goniec hook pretool` finds the grant, uses it up and allows
//! the call: a `PreToolUse` allow skips the classifier (measured on
//! 2026-10-30; without it the retry was blocked again).
//!
//! A grant is not proof. The session runs as the same Unix user and could
//! write this file itself, so a grant only names the Zulip question, and
//! `hook pretool` checks with Zulip that the trusted user approved that
//! question (`approve::verify`) before it allows anything.
//!
//! One file per conversation: `<state>/grants/<session id>.json`.

use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::inbox;

/// A grant the model does not use within this time lapses.
const LIFETIME: Duration = Duration::from_secs(15 * 60);

#[derive(Serialize, Deserialize)]
struct Grant {
    call: u64,
    expires: u64,
    #[serde(flatten)]
    question: Question,
}

/// The Zulip question a grant comes from, for `approve::verify`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub message_id: u64,
    pub code: String,
    /// `#channel>topic`, or `direct`.
    pub place: String,
}

/// The call a grant is for: the tool and its input, without the
/// `description` the model may reword on the retry. A hash, so that the
/// file holds no file contents or commands.
fn call_key(tool_name: &str, input: &Value) -> u64 {
    let mut input = input.clone();
    if let Some(map) = input.as_object_mut() {
        map.remove("description");
    }
    let mut hasher = DefaultHasher::new();
    tool_name.hash(&mut hasher);
    input.to_string().hash(&mut hasher);
    hasher.finish()
}

/// Where grants live.
pub fn dir() -> Result<PathBuf> {
    Ok(inbox::state_dir()?.join("grants"))
}

fn path(session_id: &str) -> Result<PathBuf> {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        bail!("bad session ID {session_id:?}");
    }
    Ok(dir()?.join(format!("{session_id}.json")))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn load(path: &PathBuf) -> Vec<Grant> {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<Grant>>(&b).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|g| g.expires > now())
        .collect()
}

fn store(path: &PathBuf, grants: &[Grant]) -> Result<()> {
    if grants.is_empty() {
        let _ = fs::remove_file(path);
        return Ok(());
    }
    fs::create_dir_all(path.parent().expect("a grant path has a parent"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(grants)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Record that the user approved this call once, in `question`.
pub fn grant(session_id: &str, tool_name: &str, input: &Value, question: Question) -> Result<()> {
    grant_at(&path(session_id)?, tool_name, input, question)
}

fn grant_at(path: &PathBuf, tool_name: &str, input: &Value, question: Question) -> Result<()> {
    let mut grants = load(path);
    grants.push(Grant {
        call: call_key(tool_name, input),
        expires: now() + LIFETIME.as_secs(),
        question,
    });
    store(path, &grants)
}

/// How long a question counts as an approval.
pub const MAX_AGE: Duration = LIFETIME;

/// Use up a grant for this call, if there is one, and return its question,
/// which the caller must still check with Zulip.
pub fn take(session_id: &str, tool_name: &str, input: &Value) -> Result<Option<Question>> {
    take_at(&path(session_id)?, tool_name, input)
}

fn take_at(path: &PathBuf, tool_name: &str, input: &Value) -> Result<Option<Question>> {
    // Most calls find no file and cost nothing more.
    if !path.exists() {
        return Ok(None);
    }
    let mut grants = load(path);
    let call = call_key(tool_name, input);
    let Some(at) = grants.iter().position(|g| g.call == call) else {
        store(path, &grants)?;
        return Ok(None);
    };
    let grant = grants.remove(at);
    store(path, &grants)?;
    Ok(Some(grant.question))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_ignore_the_description() {
        let a = json!({ "command": "uname -r", "description": "kernel" });
        let b = json!({ "command": "uname -r", "description": "Show the kernel" });
        assert_eq!(call_key("Bash", &a), call_key("Bash", &b));
        assert_ne!(
            call_key("Bash", &a),
            call_key("Bash", &json!({ "command": "uname -a" }))
        );
        assert_ne!(call_key("Bash", &a), call_key("Read", &a));
    }

    #[test]
    fn a_grant_allows_its_call_once() {
        let state = std::env::temp_dir().join(format!("goniec-grants-once-{}", std::process::id()));
        let path = state.join("s.json");
        let call = json!({ "command": "uname -r", "description": "kernel" });
        let question = Question {
            message_id: 177,
            code: "ae65".into(),
            place: "#ops>goniec test".into(),
        };
        assert_eq!(take_at(&path, "Bash", &call).unwrap(), None);
        grant_at(&path, "Bash", &call, question.clone()).unwrap();
        assert_eq!(
            take_at(&path, "Bash", &json!({ "command": "uname -a" })).unwrap(),
            None
        );
        let retry = json!({ "command": "uname -r", "description": "again" });
        assert_eq!(take_at(&path, "Bash", &retry).unwrap(), Some(question));
        assert_eq!(take_at(&path, "Bash", &retry).unwrap(), None, "used up");
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn expired_grants_lapse() {
        let state = std::env::temp_dir().join(format!("goniec-grants-{}", std::process::id()));
        let path = state.join("s.json");
        let call = call_key("Bash", &json!({ "command": "uname -r" }));
        let question = || Question {
            message_id: 1,
            code: "0000".into(),
            place: "direct".into(),
        };
        store(
            &path,
            &[Grant {
                call,
                expires: now() + 60,
                question: question(),
            }],
        )
        .unwrap();
        assert_eq!(load(&path).len(), 1);
        store(
            &path,
            &[Grant {
                call,
                expires: now() - 1,
                question: question(),
            }],
        )
        .unwrap();
        assert!(load(&path).is_empty(), "an expired grant is gone");
        store(&path, &[]).unwrap();
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&state);
    }
}
