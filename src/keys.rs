//! The key file: the Zulip site, the owner's email and every bot key held on
//! this host. The owner's key is not here; rosa holds it.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct Keys {
    pub site: String,
    pub owner_email: String,
    /// The user's own Zulip account. Only this user can approve a headless
    /// session's permission request, and in a headless session only this
    /// user's messages count as instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_user_id: Option<u64>,
    /// The bound bots that get a headless session when their own session is
    /// offline, by nickname. `"*"` means every bound bot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub responder: Vec<String>,
    /// The `claude` binary for headless sessions. Default:
    /// `~/.local/bin/claude`, because a service's PATH may not have it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_path: Option<String>,
    #[serde(default)]
    pub bots: BTreeMap<String, Bot>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Bot {
    pub user_id: u64,
    pub email: String,
    pub api_key: String,
    /// The directory bound to this bot on this host. Shared bots have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
}

// Never print a key, not even in a debug message.
impl fmt::Debug for Bot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bot")
            .field("user_id", &self.user_id)
            .field("email", &self.email)
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

/// `$GONIEC_KEYS`, else `~/.config/secrets/goniec.toml`.
pub fn path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("GONIEC_KEYS") {
        return Ok(path.into());
    }
    let home = std::env::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    Ok(home.join(".config/secrets/goniec.toml"))
}

impl Keys {
    pub fn load() -> Result<Self> {
        let path = path()?;
        let text = fs::read_to_string(&path)
            .with_context(|| format!("cannot read the key file {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))
    }

    /// Write the file atomically, readable by its owner only.
    pub fn save(&self) -> Result<()> {
        let path = path()?;
        let text = toml::to_string(self)?;
        let tmp = path.with_extension("toml.tmp");
        write_private(&tmp, &text)?;
        fs::rename(&tmp, &path).with_context(|| format!("cannot replace {}", path.display()))
    }

    /// The responder may start headless sessions for this bot.
    pub fn responds_for(&self, nick: &str) -> bool {
        self.responder.iter().any(|n| n == "*" || n == nick)
    }

    pub fn bot(&self, nick: &str) -> Result<&Bot> {
        self.bots
            .get(nick)
            .ok_or_else(|| anyhow!("no key for @{nick} on this host; run `goniec provision`"))
    }
}

fn write_private(path: &Path, text: &str) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    // On Windows the file gets the default access rules of the user's
    // profile directory, which is normally private to the user.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(path)
        .with_context(|| format!("cannot write {}", path.display()))?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    Ok(())
}
