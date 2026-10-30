//! The `.goniec` file, which binds a directory to its own bot.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const FILE: &str = ".goniec";

/// Nicknames that goniec itself uses.
const RESERVED: &[&str] = &["admin", "guest"];

#[derive(Serialize, Deserialize)]
pub struct Binding {
    pub nick: String,
    pub user_id: u64,
}

/// The session's directory: `$GONIEC_DIR`, else `$CLAUDE_PROJECT_DIR` (the
/// directory Claude Code started in, set for hooks), else the working
/// directory of the `claude` process in `$CLAUDE_PID` (set for the Bash
/// tool, Linux only), else the current directory.
pub fn session_dir() -> Result<PathBuf> {
    for var in ["GONIEC_DIR", "CLAUDE_PROJECT_DIR"] {
        if let Some(dir) = std::env::var_os(var).filter(|d| !d.is_empty()) {
            return Ok(dir.into());
        }
    }
    #[cfg(target_os = "linux")]
    if let Some(pid) = std::env::var("CLAUDE_PID")
        .ok()
        .filter(|p| p.parse::<u32>().is_ok())
        && let Ok(dir) = fs::read_link(format!("/proc/{pid}/cwd"))
    {
        return Ok(dir);
    }
    Ok(std::env::current_dir()?)
}

/// Read `dir/.goniec`, if it exists. goniec never looks in parent
/// directories: a binding belongs to the directory a session started in.
pub fn find(dir: &Path) -> Result<Option<Binding>> {
    let path = dir.join(FILE);
    if path.is_file() {
        Ok(Some(read(&path)?))
    } else {
        Ok(None)
    }
}

pub fn read(path: &Path) -> Result<Binding> {
    let text =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))
}

pub fn write(dir: &Path, binding: &Binding) -> Result<()> {
    let path = dir.join(FILE);
    fs::write(&path, toml::to_string(binding)?)
        .with_context(|| format!("cannot write {}", path.display()))
}

/// A nickname is lowercase ASCII letters, digits, `-` and `_`, at most 32
/// characters, and not one that goniec reserves.
pub fn check_nick(nick: &str) -> Result<()> {
    let valid = !nick.is_empty()
        && nick.len() <= 32
        && nick
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if !valid {
        bail!("bad nickname {nick:?}: use a-z, 0-9, - and _, at most 32 characters");
    }
    if RESERVED.contains(&nick) {
        bail!("@{nick} is reserved for goniec");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nick_rules() {
        assert!(check_nick("acme").is_ok());
        assert!(check_nick("some_university").is_ok());
        assert!(check_nick("acme-support").is_ok());
        assert!(check_nick("").is_err());
        assert!(check_nick("Acme").is_err());
        assert!(check_nick("a b").is_err());
        assert!(check_nick("guest").is_err());
        assert!(check_nick(&"x".repeat(33)).is_err());
    }

    #[test]
    fn find_reads_only_this_directory() {
        let root = std::env::temp_dir().join(format!("goniec-test-{}", std::process::id()));
        let sub = root.join("a/b");
        fs::create_dir_all(&sub).unwrap();
        let binding = Binding {
            nick: "acme".into(),
            user_id: 42,
        };
        write(&root, &binding).unwrap();

        let found = find(&root).unwrap().unwrap();
        assert_eq!(found.nick, "acme");
        assert_eq!(found.user_id, 42);
        assert!(find(&sub).unwrap().is_none());
        fs::remove_dir_all(&root).unwrap();
    }
}
