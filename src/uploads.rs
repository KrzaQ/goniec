//! Files in chat messages: images and other uploads.
//!
//! A Zulip message links an upload as `[name](/user_uploads/…)`. goniec
//! saves each linked file on this host, under `<state>/uploads/<message
//! id>/`, and shows the agent a `file:` line with its path. An interactive
//! session reads the file with its Read tool; a headless session also gets
//! each image in its prompt, so the model sees it at once.
//!
//! The other way, `goniec send --attach` uploads files and links them in the
//! message. It takes only files under the session directory or goniec's own
//! `<state>/outgoing/`: `Bash(goniec send *)` is allowed without asking, so
//! without that limit an agent that a chat message misled could post any
//! file the user can read, keys included. Not the temp directory: it holds
//! every Claude session's scratchpad and task output.
//!
//! The daemon removes saved and outgoing files after `KEEP`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use serde_json::{Value, json};

use crate::inbox::{self, Record};
use crate::zulip::Client;

/// goniec does not download a larger file.
const MAX_BYTES: u64 = 25_000_000;

/// The largest image that goes into a headless prompt. The Claude API takes
/// at most 5 MB of base64 per image, which is 3.75 MB of file.
const MAX_INLINE_BYTES: u64 = 3_750_000;

/// Where uploads are saved.
pub fn dir() -> Result<PathBuf> {
    Ok(inbox::state_dir()?.join("uploads"))
}

/// Where an agent puts a file to attach that is not in its session
/// directory.
pub fn outgoing_dir() -> Result<PathBuf> {
    Ok(inbox::state_dir()?.join("outgoing"))
}

/// Saved and outgoing files older than this are removed.
pub const KEEP: Duration = Duration::from_secs(30 * 24 * 3600);

/// Remove the entries of the uploads and outgoing directories that were
/// last changed more than `keep` ago. Returns how many it removed.
pub fn prune(keep: Duration) -> Result<usize> {
    Ok(prune_in(&[dir()?, outgoing_dir()?], keep))
}

fn prune_in(dirs: &[PathBuf], keep: Duration) -> usize {
    let mut removed = 0;
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > keep);
            if !old {
                continue;
            }
            let path = entry.path();
            let result = if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            if result.is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// The upload paths a message links, in order, each once.
pub fn links(content: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut rest = content;
    while let Some(at) = rest.find("(/user_uploads/") {
        rest = &rest[at + 1..];
        let end = rest.find([')', ' ', '\n']).unwrap_or(rest.len());
        let path = rest[..end].to_owned();
        if !found.contains(&path) {
            found.push(path);
        }
        rest = &rest[end..];
    }
    found
}

/// A safe local file name for an upload path: its last part, with only
/// plain characters, never hidden and never empty.
fn local_name(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or("");
    let name: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = name.trim_start_matches('.');
    if name.is_empty() {
        "file".to_owned()
    } else {
        name.to_owned()
    }
}

/// Save the files that `record` links, and list them in `record.files`.
/// Returns what could not be saved, one line per file.
pub async fn fetch(client: &Client, record: &mut Record) -> Vec<String> {
    let mut errors = Vec::new();
    for (index, link) in links(&record.content).iter().enumerate() {
        match save(client, record.id, index, link).await {
            Ok(path) => record.files.push(path),
            Err(e) => errors.push(format!("{link}: {e:#}")),
        }
    }
    errors
}

async fn save(client: &Client, message_id: u64, index: usize, link: &str) -> Result<PathBuf> {
    let dir = dir()?.join(message_id.to_string());
    // The index keeps two files with the same name apart.
    let path = dir.join(format!("{index}-{}", local_name(link)));
    if path.is_file() {
        return Ok(path);
    }
    let bytes = client.download(link, MAX_BYTES).await?;
    fs::create_dir_all(&dir)?;
    let tmp = path.with_extension("part");
    fs::write(&tmp, bytes).with_context(|| format!("cannot write {}", tmp.display()))?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

/// The media type of an image the Claude API takes, by file extension.
fn image_type(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

/// An image content block for a prompt, if the file is an image small
/// enough to send.
pub fn image_block(path: &Path) -> Option<Value> {
    let media_type = image_type(path)?;
    if fs::metadata(path).ok()?.len() > MAX_INLINE_BYTES {
        return None;
    }
    let data = base64::engine::general_purpose::STANDARD.encode(fs::read(path).ok()?);
    Some(json!({
        "type": "image",
        "source": { "type": "base64", "media_type": media_type, "data": data },
    }))
}

/// Check a file for `goniec send --attach`: a regular file under one of
/// `roots`, after following links. Returns its real path.
pub fn check_attachment(file: &Path, roots: &[PathBuf]) -> Result<PathBuf> {
    let real = fs::canonicalize(file).with_context(|| format!("cannot find {}", file.display()))?;
    if !real.is_file() {
        bail!("{} is not a file", file.display());
    }
    let inside = roots
        .iter()
        .filter_map(|root| fs::canonicalize(root).ok())
        .any(|root| real.starts_with(root));
    if !inside {
        let roots: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
        bail!(
            "{} is outside {}: goniec attaches only files there. Copy the file there first.",
            file.display(),
            roots.join(" and ")
        );
    }
    Ok(real)
}

/// Upload files and return the Markdown links for a message.
pub async fn attach(client: &Client, files: &[PathBuf]) -> Result<Vec<String>> {
    let mut links = Vec::new();
    for file in files {
        let name = file
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("bad file name {}", file.display()))?
            .to_owned();
        let bytes = fs::read(file).with_context(|| format!("cannot read {}", file.display()))?;
        let url = client.upload(&name, bytes).await?;
        links.push(format!("[{name}]({url})"));
    }
    Ok(links)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_links() {
        let content = "look [shot.png](/user_uploads/2/ab/xyz/shot.png) and \
                       [a b.pdf](/user_uploads/2/cd/q/a%20b.pdf)\n\
                       again [shot.png](/user_uploads/2/ab/xyz/shot.png)";
        assert_eq!(
            links(content),
            [
                "/user_uploads/2/ab/xyz/shot.png",
                "/user_uploads/2/cd/q/a%20b.pdf"
            ]
        );
        assert!(links("no files here").is_empty());
    }

    #[test]
    fn names() {
        assert_eq!(local_name("/user_uploads/2/ab/xyz/shot.png"), "shot.png");
        assert_eq!(local_name("/user_uploads/2/cd/q/a%20b.pdf"), "a_20b.pdf");
        assert_eq!(local_name("/user_uploads/2/x/.."), "file");
        assert_eq!(local_name("/user_uploads/2/x/.bashrc"), "bashrc");
    }

    #[test]
    fn attachments_stay_inside_their_roots() {
        let root = std::env::temp_dir().join(format!("goniec-attach-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let inside = root.join("shot.png");
        fs::write(&inside, b"png").unwrap();
        let roots = [root.clone()];
        assert!(check_attachment(&inside, &roots).is_ok());
        assert!(check_attachment(Path::new("/etc/hostname"), &roots).is_err());
        assert!(check_attachment(&root.join("../goniec-attach-missing"), &roots).is_err());
        #[cfg(unix)]
        {
            let link = root.join("hostname");
            std::os::unix::fs::symlink("/etc/hostname", &link).unwrap();
            assert!(check_attachment(&link, &roots).is_err());
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn prune_removes_only_old_entries() {
        let state = std::env::temp_dir().join(format!("goniec-prune-{}", std::process::id()));
        let _ = fs::remove_dir_all(&state);
        let dirs = [state.join("uploads"), state.join("outgoing")];
        fs::create_dir_all(dirs[0].join("97")).unwrap();
        fs::create_dir_all(&dirs[1]).unwrap();
        fs::write(dirs[1].join("chart.png"), b"png").unwrap();
        assert_eq!(prune_in(&dirs, KEEP), 0);
        assert_eq!(prune_in(&dirs, Duration::ZERO), 2);
        assert!(!dirs[0].join("97").exists());
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn image_types() {
        assert_eq!(image_type(Path::new("a.PNG")), Some("image/png"));
        assert_eq!(image_type(Path::new("a.jpeg")), Some("image/jpeg"));
        assert_eq!(image_type(Path::new("a.pdf")), None);
    }
}
