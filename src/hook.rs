//! The Claude Code adapter: `goniec hook wait` and `goniec hook context`.
//!
//! Both do nothing outside a bound session directory, and neither needs the
//! network. A hook must never break a session, so errors go to `hook.log` in
//! the state directory and never to Claude Code.

use std::fs;
use std::io::{Read, Write};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::approve;
use crate::binding;
use crate::inbox::{self, Inbox};
use crate::keys::Keys;

/// Claude Code takes at most 10,000 characters of hook output.
const MAX_CHARS: usize = 9_000;

/// How often the watcher looks at the inbox.
const POLL: Duration = Duration::from_secs(1);

/// The hook config sets `"timeout": 2147483` (24.8 days) for the watcher.
/// That is the largest delay a JavaScript timer holds (2^31 - 1 ms); a larger
/// value may fire at once. Claude Code kills a watcher at its timeout, so the
/// watcher wakes the session itself about two hours before that, the turn
/// ends, and `Stop` starts a fresh watcher. `GONIEC_RENEW_SECS` overrides
/// this, for tests.
const RENEW_AFTER: Duration = Duration::from_secs(2_140_000);

/// The exit code that wakes Claude Code from an `asyncRewake` hook.
const EXIT_WAKE: i32 = 2;

#[derive(Deserialize, Default)]
struct Input {
    #[serde(default)]
    hook_event_name: String,
    /// For `PermissionRequest`: the tool and its input.
    #[serde(default)]
    tool_name: String,
    #[serde(default)]
    tool_input: serde_json::Value,
}

fn read_input() -> Input {
    let mut text = String::new();
    let _ = std::io::stdin().read_to_string(&mut text);
    serde_json::from_str(&text).unwrap_or_default()
}

fn log(what: &str, err: &anyhow::Error) {
    let Ok(dir) = inbox::state_dir() else { return };
    let _ = fs::create_dir_all(&dir);
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("hook.log"))
    {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let _ = writeln!(file, "{now} hook {what}: {err:#}");
    }
}

/// For `PermissionRequest` in a headless session that goniec started: ask the
/// trusted user in Zulip. Any failure gives no decision, and Claude Code then
/// denies the tool call, because a headless session cannot prompt.
pub fn permission() -> i32 {
    let input = read_input();
    let decision = match ask_trusted_user(&input) {
        Ok(Some(approve::Answer::Allow)) => serde_json::json!({ "behavior": "allow" }),
        Ok(Some(approve::Answer::Deny(why))) => {
            serde_json::json!({ "behavior": "deny", "message": format!("Not approved: {why}.") })
        }
        Ok(None) => return 0,
        Err(e) => {
            log("permission", &e);
            return 0;
        }
    };
    let output = serde_json::json!({
        "hookSpecificOutput": { "hookEventName": "PermissionRequest", "decision": decision }
    });
    println!("{output}");
    0
}

fn ask_trusted_user(input: &Input) -> Result<Option<approve::Answer>> {
    let dir = binding::session_dir()?;
    let Some(binding) = binding::find(&dir)? else {
        return Ok(None);
    };
    let keys = Keys::load()?;
    let bot = keys.bot(&binding.nick)?;
    if bot.user_id != binding.user_id || bot.dir.as_deref() != Some(dir.as_path()) {
        bail!(
            "{} does not match the key file",
            dir.join(binding::FILE).display()
        );
    }
    let Some(trusted) = keys.trusted_user_id else {
        bail!("no trusted_user_id in the key file, so nobody can approve");
    };
    let runtime = tokio::runtime::Runtime::new()?;
    let answer = runtime.block_on(approve::ask(
        &keys.site,
        &binding.nick,
        bot,
        trusted,
        &input.tool_name,
        &input.tool_input,
    ))?;
    Ok(Some(answer))
}

/// This session's nickname and inbox, if its directory is bound to a bot
/// whose key this host holds. The check is local; the daemon has already
/// checked the key with Zulip.
fn session_inbox() -> Result<Option<(String, Inbox)>> {
    let dir = binding::session_dir()?;
    let Some(binding) = binding::find(&dir)? else {
        return Ok(None);
    };
    let keys = Keys::load()?;
    let bot = keys.bot(&binding.nick)?;
    if bot.user_id != binding.user_id || bot.dir.as_deref() != Some(dir.as_path()) {
        bail!(
            "{} does not match the key file; run `goniec whoami` there",
            dir.join(binding::FILE).display()
        );
    }
    Ok(Some((binding.nick.clone(), Inbox::open(&binding.nick)?)))
}

/// Run `goniec hook <event>`. This runs before clap parses anything, because
/// clap exits with 2 on a usage error, and 2 from an `asyncRewake` hook wakes
/// the session. An unknown event therefore exits 0.
pub fn run(event: Option<&str>) -> i32 {
    match event {
        Some("wait") => wait(),
        Some("context") => context(),
        Some("permission") => permission(),
        other => {
            log("dispatch", &anyhow::anyhow!("unknown hook event {other:?}"));
            0
        }
    }
}

/// For `PostToolBatch`: add new messages to the context of a busy session.
pub fn context() -> i32 {
    let input = read_input();
    let text = match session_inbox().and_then(|found| match found {
        Some((nick, inbox)) => inbox.take_fitting(&nick, MAX_CHARS),
        None => Ok(None),
    }) {
        Ok(Some(text)) => text,
        Ok(None) => return 0,
        Err(e) => {
            log("context", &e);
            return 0;
        }
    };
    let event = match input.hook_event_name.as_str() {
        "" => "PostToolBatch",
        name => name,
    };
    let output = serde_json::json!({
        "hookSpecificOutput": { "hookEventName": event, "additionalContext": text }
    });
    println!("{output}");
    0
}

/// For `asyncRewake` on `SessionStart` and `Stop`: wait until the inbox has
/// messages, print them to stderr and exit 2, which wakes the session.
pub fn wait() -> i32 {
    read_input();
    match wait_for_messages() {
        Ok(Some(text)) => {
            eprint!("{text}");
            EXIT_WAKE
        }
        Ok(None) => 0,
        Err(e) => {
            log("wait", &e);
            0
        }
    }
}

fn wait_for_messages() -> Result<Option<String>> {
    let Some((nick, inbox)) = session_inbox()? else {
        return Ok(None);
    };
    // One watcher per nickname. A second one, from the next Stop while the
    // first still waits, ends at once. The lock ends with the process.
    let lock_path = inbox::state_dir()?
        .join("inbox")
        .join(&nick)
        .join("watcher.lock");
    let lock = fs::File::create(&lock_path)?;
    if lock.try_lock().is_err() {
        return Ok(None);
    }
    let owner = Owner::find();
    let renew_after = std::env::var("GONIEC_RENEW_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(RENEW_AFTER, Duration::from_secs);
    let started = Instant::now();
    loop {
        // While a turn runs, PostToolBatch delivers inline. A wake from here
        // would wait for the turn to end and then cost a turn of its own.
        if inbox.pending()? > 0
            && !owner.in_turn()
            && let Some(text) = inbox.take_fitting(&nick, MAX_CHARS)?
        {
            return Ok(Some(text));
        }
        if !owner.alive() {
            return Ok(None);
        }
        if started.elapsed() >= renew_after {
            return Ok(Some(format!(
                "[goniec] Renewing the chat watcher for @{nick}. There are no messages. \
                 No action is needed: end the turn.\n"
            )));
        }
        sleep(POLL);
    }
}

/// The Claude Code process this hook runs for. A watcher whose session has
/// ended must not keep the lock for its nickname.
#[cfg(target_os = "linux")]
struct Owner {
    pid: u32,
    start: u64,
}

#[cfg(target_os = "linux")]
impl Owner {
    /// `$CLAUDE_PID`, else the nearest ancestor named `claude`, else the
    /// parent process.
    fn find() -> Self {
        if let Some(pid) = std::env::var("CLAUDE_PID")
            .ok()
            .and_then(|p| p.parse().ok())
            && let Some(stat) = Stat::read(pid)
        {
            return Self {
                pid,
                start: stat.start,
            };
        }
        let parent = std::os::unix::process::parent_id();
        let mut pid = parent;
        for _ in 0..16 {
            let Some(stat) = Stat::read(pid) else { break };
            if stat.comm == "claude" {
                return Self {
                    pid,
                    start: stat.start,
                };
            }
            if stat.ppid <= 1 {
                break;
            }
            pid = stat.ppid;
        }
        let start = Stat::read(parent).map_or(0, |s| s.start);
        Self { pid: parent, start }
    }

    /// The process still runs, and it is the same process, not a new one
    /// with a reused PID.
    fn alive(&self) -> bool {
        Stat::read(self.pid).is_some_and(|s| s.start == self.start)
    }

    /// Claude Code says this session is in a turn (`busy`) or waits for a
    /// permission answer (`waiting`). It keeps one file per process in
    /// `~/.claude/sessions/<pid>.json`, with a `status` field. The file is
    /// internal and undocumented: the documented `claude agents --json` lists
    /// background sessions only, not ones started with `claude` in a
    /// terminal. If the file is missing or its format changes, this says no,
    /// so the watcher delivers as if the session were idle.
    fn in_turn(&self) -> bool {
        let dir = match std::env::var_os("CLAUDE_CONFIG_DIR") {
            Some(dir) => std::path::PathBuf::from(dir),
            None => match std::env::home_dir() {
                Some(home) => home.join(".claude"),
                None => return false,
            },
        };
        let Ok(text) = fs::read_to_string(dir.join("sessions").join(format!("{}.json", self.pid)))
        else {
            return false;
        };
        let status = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["status"].as_str().map(str::to_owned));
        matches!(status.as_deref(), Some("busy" | "waiting"))
    }
}

#[cfg(target_os = "linux")]
struct Stat {
    comm: String,
    ppid: u32,
    start: u64,
}

#[cfg(target_os = "linux")]
impl Stat {
    /// Parse `/proc/<pid>/stat`. The name is in parentheses and may contain
    /// spaces, so the other fields are counted from the last `)`.
    fn read(pid: u32) -> Option<Self> {
        let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let open = text.find('(')?;
        let close = text.rfind(')')?;
        let fields: Vec<&str> = text[close + 1..].split_whitespace().collect();
        Some(Self {
            comm: text[open + 1..close].to_owned(),
            // Fields 4 and 22 of stat(5); `fields[0]` is field 3.
            ppid: fields.get(1)?.parse().ok()?,
            start: fields.get(19)?.parse().ok()?,
        })
    }
}

/// Elsewhere goniec cannot see the owner yet, so the watcher waits until
/// Claude Code ends it.
#[cfg(not(target_os = "linux"))]
struct Owner;

#[cfg(not(target_os = "linux"))]
impl Owner {
    fn find() -> Self {
        Owner
    }

    fn alive(&self) -> bool {
        true
    }

    fn in_turn(&self) -> bool {
        false
    }
}
