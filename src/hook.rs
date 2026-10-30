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
use crate::headless;
use crate::inbox::{self, Inbox, Record};
use crate::keys::Keys;
use crate::process::Owner;
use crate::sessions;
use crate::threads;

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
    /// The conversation's session UUID.
    #[serde(default)]
    session_id: String,
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

/// The conversation's session ID, if the hook input names one.
fn session_id(id: &str) -> Option<&str> {
    (!id.is_empty()).then_some(id)
}

/// A message waits that this conversation may take: its thread belongs to
/// the conversation, or has no owner yet.
fn has_mine(nick: &str, inbox: &Inbox, session: &str) -> Result<bool> {
    let table = threads::load(nick)?;
    Ok(inbox
        .peek()?
        .iter()
        .any(|r| threads::may_take(&table, r, session_id(session))))
}

/// Take the messages this conversation may take and `extra` accepts, as one
/// text, and make the conversation the owner of their threads.
fn take_mine(
    nick: &str,
    inbox: &Inbox,
    session: &str,
    extra: impl Fn(&Record) -> bool,
) -> Result<Option<String>> {
    let table = threads::load(nick)?;
    let mine = |r: &Record| threads::may_take(&table, r, session_id(session)) && extra(r);
    let Some((text, taken)) = inbox.take_fitting_where(nick, MAX_CHARS, mine)? else {
        return Ok(None);
    };
    if let Some(id) = session_id(session) {
        let owner = threads::Owner {
            session_id: id.to_owned(),
            dir: binding::session_dir()?,
        };
        threads::claim(nick, &taken, &owner)?;
    }
    Ok(Some(text))
}

/// Record that this conversation is live in this session. Failures are only
/// logged: delivery must not depend on the registry.
fn register_session(input: &Input, nick: &str) {
    if input.session_id.is_empty() {
        return;
    }
    let owner = Owner::find();
    let entry = binding::session_dir().map(|dir| sessions::Entry {
        session_id: input.session_id.clone(),
        nick: nick.to_owned(),
        pid: owner.pid,
        start: owner.start,
        dir,
        headless: headless::active(),
    });
    if let Err(e) = entry.and_then(|entry| sessions::register(&entry)) {
        log("register", &e);
    }
}

/// For `SessionEnd`: the conversation is no longer live in this session.
pub fn end() -> i32 {
    let input = read_input();
    if let Err(e) = sessions::unregister(&input.session_id, Owner::find().pid) {
        log("end", &e);
    }
    0
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
        &approve::Place::from_env(),
        &input.tool_name,
        &input.tool_input,
    ))?;
    Ok(Some(answer))
}

/// This session's nickname and inbox, if its directory is bound to a bot
/// whose key this host holds. The check is local; the daemon has already
/// checked the key with Zulip.
fn session_inbox() -> Result<Option<(String, Inbox, Option<u64>)>> {
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
    Ok(Some((
        binding.nick.clone(),
        Inbox::open(&binding.nick)?,
        keys.trusted_user_id,
    )))
}

/// Run `goniec hook <event>`. This runs before clap parses anything, because
/// clap exits with 2 on a usage error, and 2 from an `asyncRewake` hook wakes
/// the session. An unknown event therefore exits 0.
pub fn run(event: Option<&str>) -> i32 {
    match event {
        Some("wait") => wait(),
        Some("context") => context(),
        Some("permission") => permission(),
        Some("end") => end(),
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
        Some((nick, inbox, trusted)) => {
            register_session(&input, &nick);
            // In a headless session the trusted user's messages are the
            // responder's prompt, so the hook leaves them alone.
            let headless = headless::active();
            take_mine(&nick, &inbox, &input.session_id, |r| {
                !headless || !headless::is_instruction(r, trusted)
            })
        }
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
    let input = read_input();
    match wait_for_messages(&input) {
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

fn wait_for_messages(input: &Input) -> Result<Option<String>> {
    let Some((nick, inbox, _)) = session_inbox()? else {
        return Ok(None);
    };
    register_session(input, &nick);
    // A headless session gets its turns from the responder, not from a wake.
    if headless::active() {
        return Ok(None);
    }
    // One watcher per conversation. A second one, from the next Stop while
    // the first still waits, ends at once. The lock ends with the process.
    let lock_name = match input.session_id.as_str() {
        "" => "watcher.lock".to_owned(),
        id => format!("watcher-{id}.lock"),
    };
    let lock_path = inbox::state_dir()?
        .join("inbox")
        .join(&nick)
        .join(lock_name);
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
        if has_mine(&nick, &inbox, &input.session_id)?
            && !owner.in_turn()
            && let Some(text) = take_mine(&nick, &inbox, &input.session_id, |_| true)?
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
