//! The `.goniec` file, which binds a directory to its own bot.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const FILE: &str = ".goniec";

/// Nicknames that goniec itself uses.
const RESERVED: &[&str] = &["admin", "guest", "claude"];

#[derive(Serialize, Deserialize)]
pub struct Binding {
    pub nick: String,
    pub user_id: u64,
}

/// Find the nearest `.goniec` in `dir` or one of its parents, so that a
/// session that changed into a subdirectory keeps its identity.
pub fn find(dir: &Path) -> Result<Option<(PathBuf, Binding)>> {
    for dir in dir.ancestors() {
        let path = dir.join(FILE);
        if path.is_file() {
            return Ok(Some((dir.to_owned(), read(&path)?)));
        }
    }
    Ok(None)
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
    fn find_walks_up() {
        let root = std::env::temp_dir().join(format!("goniec-test-{}", std::process::id()));
        let sub = root.join("a/b");
        fs::create_dir_all(&sub).unwrap();
        let binding = Binding {
            nick: "acme".into(),
            user_id: 42,
        };
        write(&root, &binding).unwrap();

        let (dir, found) = find(&sub).unwrap().unwrap();
        assert_eq!(dir, root);
        assert_eq!(found.nick, "acme");
        assert_eq!(found.user_id, 42);
        fs::remove_dir_all(&root).unwrap();
    }
}
