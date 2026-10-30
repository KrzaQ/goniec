//! Files in chat messages: images and other uploads.
//!
//! A Zulip message links an upload as `[name](/user_uploads/…)`. When goniec
//! hands messages to a Claude Code session, it saves their files in that
//! session's own temp directory, `<tmp>/claude-<uid>/<project>/<session
//! id>/goniec/`, and shows a `file:` line with each path. Claude Code lets a
//! session read and write its own temp directory without asking (measured on
//! 2026-10-30, also in `-p` mode), so no allow rule is needed, and a session
//! gets only its own files. The temp directory is tmpfs on this host, so the
//! files go at reboot; Zulip keeps the originals, and `goniec read <thread>`
//! saves them again. A headless session also gets each image in its prompt,
//! so the model sees it at once.
//!
//! A file's path follows from the message: `<message id>-<n>-<name>`. So a
//! notice can name the files before they are saved, and the consumer saves
//! them right after it takes the messages.
//!
//! The other way, `goniec send --attach` uploads files and links them in the
//! message. It takes only files under the session directory or the
//! session's own temp directory: `Bash(goniec send *)` is allowed without
//! asking, so without that limit an agent that a chat message misled could
//! post any file the user can read, keys included. Not all of `/tmp`: it
//! holds every Claude session's scratchpad and task output.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use serde_json::{Value, json};

use crate::inbox::Record;
use crate::keys::Keys;
use crate::zulip::Client;

/// goniec does not download a larger file.
const MAX_BYTES: u64 = 25_000_000;

/// The largest image that goes into a headless prompt. The Claude API takes
/// at most 5 MB of base64 per image, which is 3.75 MB of file.
const MAX_INLINE_BYTES: u64 = 3_750_000;

/// The user ID, which Claude Code puts in its temp directory's name.
fn uid() -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let probe = if cfg!(target_os = "linux") {
            PathBuf::from("/proc/self")
        } else {
            std::env::home_dir().unwrap_or_default()
        };
        fs::metadata(probe).map_or(0, |m| m.uid())
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// Claude Code's name for a project directory: every character that is not
/// an ASCII letter or digit becomes `-`.
fn project_name(dir: &Path) -> String {
    dir.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// A Claude Code session's own temp directory. An existing directory for
/// the session ID wins, wherever it is, in case Claude Code names project
/// directories differently from `project_name`.
pub fn session_tmp(project_dir: &Path, session_id: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("claude-{}", uid()));
    let found = fs::read_dir(&base).ok().and_then(|entries| {
        entries
            .filter_map(|e| e.ok())
            .map(|e| e.path().join(session_id))
            .find(|p| p.is_dir())
    });
    found.unwrap_or_else(|| base.join(project_name(project_dir)).join(session_id))
}

/// Where goniec saves a session's files.
pub fn files_dir(project_dir: &Path, session_id: &str) -> PathBuf {
    session_tmp(project_dir, session_id).join("goniec")
}

/// The Claude Code session this process runs in.
pub struct Session {
    /// Where its files from the chat go.
    pub files: PathBuf,
    /// Its own temp directory.
    pub tmp: PathBuf,
    /// The ID comes from Claude Code's registry entry for the `claude`
    /// process above this one, not from the environment. Only then may
    /// `--attach` take files from `tmp`: a command can set any environment
    /// variable, for example another session's ID.
    pub verified: bool,
}

/// The calling session: from Claude Code's registry entry for the `claude`
/// ancestor process, else from `CLAUDE_CODE_SESSION_ID`, which its Bash
/// tool sets.
pub fn current_session(project_dir: &Path) -> Option<Session> {
    let registered = crate::process::claude_ancestor().and_then(|pid| {
        let path = std::env::home_dir()?.join(format!(".claude/sessions/{pid}.json"));
        let entry: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        entry["sessionId"].as_str().map(str::to_owned)
    });
    let verified = registered.is_some();
    let id = registered.or_else(|| std::env::var("CLAUDE_CODE_SESSION_ID").ok())?;
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    Some(Session {
        files: files_dir(project_dir, &id),
        tmp: session_tmp(project_dir, &id),
        verified,
    })
}

/// Set `files` of each record, and of its attached earlier messages, to
/// where their linked files go in `dir`.
pub fn assign(records: &mut [Record], dir: &Path) {
    for r in records {
        r.files = paths(dir, r);
        for e in &mut r.earlier {
            e.files = paths(dir, e);
        }
    }
}

/// Where the files a record links go, in `dir`, in link order.
pub fn paths(dir: &Path, record: &Record) -> Vec<PathBuf> {
    links(&record.content)
        .iter()
        .enumerate()
        .map(|(index, link)| path(dir, record.id, index, link))
        .collect()
}

fn path(dir: &Path, message_id: u64, index: usize, link: &str) -> PathBuf {
    // The index keeps two files with the same name apart.
    dir.join(format!("{message_id}-{index}-{}", local_name(link)))
}

/// Save the files that `records` and their attached earlier messages link,
/// into `dir`. Files already there stay. Returns a line for each file that
/// could not be saved.
pub async fn save_all(client: &Client, records: &[Record], dir: &Path) -> Vec<String> {
    let mut errors = Vec::new();
    for r in records
        .iter()
        .flat_map(|r| r.earlier.iter().chain(std::iter::once(r)))
    {
        for (index, link) in links(&r.content).iter().enumerate() {
            let path = path(dir, r.id, index, link);
            if path.is_file() {
                continue;
            }
            if let Err(e) = save(client, link, &path).await {
                errors.push(format!("[goniec] Could not save {link}: {e:#}"));
            }
        }
    }
    errors
}

/// `save_all` for code without an async runtime, as `nick`'s bot. Does
/// nothing when no record links a file.
pub fn save_all_blocking(keys: &Keys, nick: &str, records: &[Record], dir: &Path) -> Vec<String> {
    let any = records
        .iter()
        .flat_map(|r| r.earlier.iter().chain(std::iter::once(r)))
        .any(|r| !links(&r.content).is_empty());
    if !any {
        return Vec::new();
    }
    let run = || -> Result<Vec<String>> {
        let bot = keys.bot(nick)?;
        let client = Client::new(&keys.site, &bot.email, &bot.api_key)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(runtime.block_on(save_all(&client, records, dir)))
    };
    run().unwrap_or_else(|e| vec![format!("[goniec] Could not save the files: {e:#}")])
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

async fn save(client: &Client, link: &str, path: &Path) -> Result<()> {
    let bytes = client.download(link, MAX_BYTES).await?;
    let dir = path.parent().expect("a file path has a parent");
    fs::create_dir_all(dir)?;
    let tmp = path.with_extension("part");
    fs::write(&tmp, bytes).with_context(|| format!("cannot write {}", tmp.display()))?;
    fs::rename(&tmp, path)?;
    Ok(())
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
    fn paths_follow_the_message() {
        let mut r = crate::inbox::Record {
            id: 97,
            timestamp: 0,
            sender_id: 8,
            sender_name: "kq".into(),
            sender_is_bot: false,
            channel: None,
            topic: None,
            mentioned: false,
            content: "[a.png](/user_uploads/2/x/a.png) [a.png](/user_uploads/2/y/a.png)".into(),
            earlier: Vec::new(),
            note: None,
            files: Vec::new(),
        };
        assert_eq!(
            paths(Path::new("/s"), &r),
            [
                PathBuf::from("/s/97-0-a.png"),
                PathBuf::from("/s/97-1-a.png")
            ]
        );
        r.content = "no files".into();
        assert!(paths(Path::new("/s"), &r).is_empty());
    }

    #[test]
    fn project_names() {
        assert_eq!(
            project_name(Path::new(
                "/home/krzaq/code/kqsolutions/biblioteka_slaska/pretty_kiosk_gui"
            )),
            "-home-krzaq-code-kqsolutions-biblioteka-slaska-pretty-kiosk-gui"
        );
    }

    #[test]
    fn image_types() {
        assert_eq!(image_type(Path::new("a.PNG")), Some("image/png"));
        assert_eq!(image_type(Path::new("a.jpeg")), Some("image/jpeg"));
        assert_eq!(image_type(Path::new("a.pdf")), None);
    }
}
