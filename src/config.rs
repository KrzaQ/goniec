//! What a headless process loads only when it starts.
//!
//! Claude Code reads settings again while it runs, but it starts MCP servers
//! and plugins only once, and a running process keeps its own version. The
//! responder takes a fingerprint of these when it starts a process, and
//! before each turn it compares it with the current one. When they differ,
//! the process ends after its turn and the next message resumes the
//! conversation with the new configuration.

use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use serde_json::Value;

use crate::keys::Keys;

/// The `claude` binary that headless sessions run.
pub fn claude_path(keys: &Keys) -> Result<PathBuf> {
    let home = std::env::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    Ok(keys
        .claude_path
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/bin/claude")))
}

/// A hash of the MCP servers, plugins and the `claude` binary that a session
/// in `dir` would load. The hash, not the text, stays in memory, because MCP
/// server entries can hold tokens.
pub fn fingerprint(dir: &Path, claude: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();
    let home = std::env::home_dir().unwrap_or_default();

    // A version upgrade points the `claude` link at a new file.
    let binary = fs::canonicalize(claude).unwrap_or_else(|_| claude.to_owned());
    binary.hash(&mut hasher);
    fs::metadata(&binary)
        .and_then(|m| m.modified())
        .ok()
        .hash(&mut hasher);

    // `~/.claude.json` changes all the time (counters, tips), so only the
    // MCP parts count: user servers and this project's local servers.
    if let Some(state) = json(&home.join(".claude.json")) {
        state["mcpServers"].to_string().hash(&mut hasher);
        let project = &state["projects"][dir.to_string_lossy().as_ref()];
        for key in MCP_KEYS {
            project[key].to_string().hash(&mut hasher);
        }
    }
    fs::read(dir.join(".mcp.json")).ok().hash(&mut hasher);

    for settings in [
        home.join(".claude/settings.json"),
        dir.join(".claude/settings.json"),
        dir.join(".claude/settings.local.json"),
    ] {
        let value = json(&settings).unwrap_or(Value::Null);
        value["enabledPlugins"].to_string().hash(&mut hasher);
        for key in MCP_KEYS.iter().skip(1) {
            value[key].to_string().hash(&mut hasher);
        }
    }
    fs::read(home.join(".claude/plugins/installed_plugins.json"))
        .ok()
        .hash(&mut hasher);
    hasher.finish()
}

/// The keys that choose MCP servers, in `~/.claude.json` projects and in
/// settings files.
const MCP_KEYS: [&str; 4] = [
    "mcpServers",
    "enabledMcpjsonServers",
    "disabledMcpjsonServers",
    "enableAllProjectMcpServers",
];

fn json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// The transcript of a conversation, if Claude Code has written one. A
/// conversation without one has not had a turn yet, so it starts with
/// `--session-id`, not `--resume`.
pub fn transcript(session_id: &str) -> Option<PathBuf> {
    let projects = std::env::home_dir()?.join(".claude/projects");
    let name = format!("{session_id}.jsonl");
    fs::read_dir(projects)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path().join(&name))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_follows_the_project_mcp_file() {
        let dir = std::env::temp_dir().join(format!("goniec-config-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let claude = Path::new("/nonexistent/claude");
        let before = fingerprint(&dir, claude);
        assert_eq!(fingerprint(&dir, claude), before);
        fs::write(dir.join(".mcp.json"), r#"{"mcpServers":{}}"#).unwrap();
        assert_ne!(fingerprint(&dir, claude), before);
        fs::remove_dir_all(&dir).unwrap();
    }
}
