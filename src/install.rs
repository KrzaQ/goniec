//! `goniec install-hooks`: add the Claude Code hooks to the user's global
//! settings, or remove them. goniec owns only its own entries; every other
//! key and every other hook in the file stays as it is.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value, json};

/// The watcher's timeout: the largest delay a JavaScript timer holds. See
/// `RENEW_AFTER` in `hook.rs`.
const WAIT_TIMEOUT: u64 = 2_147_483;

/// A hook command that does nothing where goniec is missing. `exec` passes
/// exit code 2 (wake) through; an `|| true` wrapper would swallow it.
fn guarded(event: &str) -> String {
    format!(
        "g=$(command -v goniec || echo \"$HOME/.cargo/bin/goniec\"); [ -x \"$g\" ] || exit 0; \
         exec \"$g\" hook {event}"
    )
}

/// A hook entry is goniec's own when its command names goniec and one of
/// its hook events. That covers older forms such as `goniec hook wait`.
fn is_ours(hook: &Value) -> bool {
    hook["command"].as_str().is_some_and(|c| {
        c.contains("goniec") && (c.contains("hook wait") || c.contains("hook context"))
    })
}

/// `$CLAUDE_CONFIG_DIR/settings.json`, else `~/.claude/settings.json`.
pub fn settings_path() -> Result<PathBuf> {
    let dir = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => std::env::home_dir()
            .ok_or_else(|| anyhow!("no home directory"))?
            .join(".claude"),
    };
    Ok(dir.join("settings.json"))
}

/// The hook groups goniec adds, per event.
fn our_groups() -> Vec<(&'static str, Value)> {
    vec![
        (
            "SessionStart",
            json!({ "hooks": [{ "type": "command", "command": guarded("wait"),
                                 "asyncRewake": true, "timeout": WAIT_TIMEOUT }] }),
        ),
        (
            "Stop",
            json!({ "hooks": [{ "type": "command", "command": guarded("wait"),
                                 "asyncRewake": true, "timeout": WAIT_TIMEOUT }] }),
        ),
        (
            "PostToolBatch",
            json!({ "hooks": [{ "type": "command", "command": guarded("context"),
                                 "timeout": 10 }] }),
        ),
    ]
}

/// Remove goniec's entries from every event, and events or groups left empty.
fn strip_ours(hooks: &mut Map<String, Value>) {
    for groups in hooks.values_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        for group in groups.iter_mut() {
            if let Some(list) = group["hooks"].as_array_mut() {
                list.retain(|hook| !is_ours(hook));
            }
        }
        groups.retain(|group| {
            group["hooks"]
                .as_array()
                .is_none_or(|list| !list.is_empty())
        });
    }
    hooks.retain(|_, groups| groups.as_array().is_none_or(|g| !g.is_empty()));
}

/// The settings with goniec's hooks added (`install`) or removed.
pub fn apply(mut settings: Value, install: bool) -> Result<Value> {
    let Some(top) = settings.as_object_mut() else {
        bail!("the settings file does not hold a JSON object");
    };
    let hooks = top.entry("hooks").or_insert_with(|| json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        bail!("`hooks` in the settings file is not an object");
    };
    strip_ours(hooks);
    if install {
        for (event, group) in our_groups() {
            let groups = hooks.entry(event).or_insert_with(|| json!([]));
            let Some(groups) = groups.as_array_mut() else {
                bail!("`hooks.{event}` in the settings file is not a list");
            };
            groups.push(group);
        }
    }
    if hooks.is_empty() {
        top.remove("hooks");
    }
    Ok(settings)
}

/// Install or remove the hooks. Returns false when the file already was as
/// wanted, and then does not write it.
pub fn install_hooks(install: bool) -> Result<bool> {
    let path = settings_path()?;
    let before: Value = match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("cannot parse {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let after = apply(before.clone(), install)?;
    if after == before {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    // Write through a symlink (settings.json may point into a checkout), and
    // keep the file's permissions: the rename would replace the link itself
    // and give the file the default mode.
    let target = fs::canonicalize(&path).unwrap_or(path);
    let tmp = target.with_extension("json.goniec-tmp");
    fs::write(&tmp, serde_json::to_string_pretty(&after)? + "\n")
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    if let Ok(meta) = fs::metadata(&target) {
        fs::set_permissions(&tmp, meta.permissions())?;
    }
    fs::rename(&tmp, &target).with_context(|| format!("cannot replace {}", target.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn other_hook() -> Value {
        json!({ "matcher": "Bash", "hooks": [{ "type": "command", "command": "log-it" }] })
    }

    #[test]
    fn install_keeps_everything_else() {
        let before = json!({ "zeta": 1, "alpha": 2, "hooks": { "PreToolUse": [other_hook()] } });
        let after = apply(before.clone(), true).unwrap();
        // Key order stays as it was.
        let keys: Vec<&String> = after.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["zeta", "alpha", "hooks"]);
        assert_eq!(after["hooks"]["PreToolUse"], json!([other_hook()]));
        assert_eq!(after["hooks"]["Stop"][0]["hooks"][0]["asyncRewake"], true);
        assert_eq!(
            after["hooks"]["PostToolBatch"][0]["hooks"][0]["timeout"],
            10
        );
    }

    #[test]
    fn other_hooks_on_the_same_event_stay() {
        let mine = json!({ "hooks": [{ "type": "command", "command": "my-stop-hook" }] });
        let before = json!({ "hooks": { "Stop": [mine.clone()] } });
        let installed = apply(before.clone(), true).unwrap();
        let stop = installed["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0], mine);
        // Twice gives no duplicate; removing gives back exactly the user's hook.
        assert_eq!(apply(installed.clone(), true).unwrap(), installed);
        assert_eq!(apply(installed, false).unwrap(), before);
    }

    #[test]
    fn install_twice_is_the_same() {
        let once = apply(json!({}), true).unwrap();
        assert_eq!(apply(once.clone(), true).unwrap(), once);
    }

    #[test]
    fn install_replaces_an_older_entry() {
        let old = json!({ "hooks": { "Stop": [{ "hooks": [{ "type": "command",
            "command": "$HOME/.cargo/bin/goniec hook wait", "asyncRewake": true, "timeout": 86400 }] }] } });
        let after = apply(old, true).unwrap();
        let stop = after["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert_eq!(stop[0]["hooks"][0]["timeout"], WAIT_TIMEOUT);
    }

    #[test]
    fn remove_leaves_other_hooks() {
        let installed = apply(json!({ "hooks": { "PreToolUse": [other_hook()] } }), true).unwrap();
        let removed = apply(installed, false).unwrap();
        assert_eq!(
            removed,
            json!({ "hooks": { "PreToolUse": [other_hook()] } })
        );
        assert_eq!(
            apply(apply(json!({ "a": 1 }), true).unwrap(), false).unwrap(),
            json!({ "a": 1 })
        );
    }
}
