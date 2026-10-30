//! The headless responder: every thread has an owner conversation, and when
//! a message waits in a thread whose owner is live nowhere, the responder
//! runs that conversation headless and feeds it turns.
//!
//! - A thread whose owner is live in an interactive session: nothing to do,
//!   that session takes the message.
//! - A thread whose owner is live nowhere: resume it, `claude -p --resume
//!   <uuid>`, in its directory.
//! - A thread without an owner while no interactive session of the bot is
//!   online: a new conversation, `--session-id <new uuid>`, which owns the
//!   thread from before its start.
//!
//! The trusted user's messages are the prompt (see `headless`). Every other
//! message stays in the inbox, and the session reads it as data.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

use crate::headless;
use crate::inbox::{self, Inbox, Record};
use crate::keys::{Bot, Keys};
use crate::send::{self, Target};
use crate::sessions::{self, Entry};
use crate::threads::{self, Owner, Table};
use crate::zulip::Client;

/// Collect messages for this long before a turn, so that a burst of lines
/// becomes one turn.
const BATCH: Duration = Duration::from_secs(2);

/// End a headless process after this long without a turn. Its threads keep
/// their owner, so the next message resumes it.
const IDLE_END: Duration = Duration::from_secs(15 * 60);

/// At most this many headless processes per bot at a time.
const MAX_PER_BOT: usize = 3;

/// After a process that ended without finishing a turn, wait this long
/// before starting that conversation again.
const RETRY_GAP: Duration = Duration::from_secs(60);

/// The permission hook's timeout. The hook itself gives up after 14 minutes.
const PERMISSION_TIMEOUT: u64 = 15 * 60;

fn log(nick: &str, msg: impl std::fmt::Display) {
    eprintln!("goniec responder: @{nick}: {msg}");
}

/// What the stdout reader tells the responder about a process.
#[derive(Default)]
struct State {
    busy: bool,
    exited: bool,
    turns: u32,
    last_turn_end: Option<Instant>,
}

/// A headless process running one conversation.
struct Conversation {
    nick: String,
    child: Child,
    stdin: Option<ChildStdin>,
    state: Arc<Mutex<State>>,
    started: Instant,
    reply_file: PathBuf,
    /// The newest message the conversation was told about. Messages from
    /// others that it left unread do not start another turn.
    fed_up_to: u64,
}

impl Conversation {
    fn busy(&self) -> bool {
        self.state.lock().unwrap().busy
    }
}

#[derive(Default)]
pub struct Responder {
    /// Running headless processes, by session UUID.
    running: HashMap<String, Conversation>,
    /// Conversations whose process ended without finishing a turn.
    failed: HashMap<String, Instant>,
}

impl Responder {
    /// Look at every served bot once. Errors are logged per bot.
    pub async fn tick(&mut self, keys: &Keys, bound: &HashMap<String, Bot>) {
        self.reap().await;
        let live = sessions::live().unwrap_or_default();
        for (nick, bot) in bound.iter().filter(|(nick, _)| keys.responds_for(nick)) {
            if let Err(e) = self.tick_bot(keys, nick, bot, &live).await {
                log(nick, format!("{e:#}"));
            }
        }
        // Processes of bots that are no longer bound or served end with
        // their input.
        for conv in self.running.values_mut() {
            if !(bound.contains_key(&conv.nick) && keys.responds_for(&conv.nick)) {
                conv.stdin = None;
            }
        }
    }

    /// Forget processes that ended.
    async fn reap(&mut self) {
        let ended: Vec<String> = self
            .running
            .iter()
            .filter(|(_, c)| c.state.lock().unwrap().exited)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ended {
            let mut conv = self.running.remove(&id).expect("listed above");
            let _ = conv.child.wait().await;
            if conv.state.lock().unwrap().turns == 0 {
                self.failed.insert(id.clone(), Instant::now());
                log(
                    &conv.nick,
                    format!("conversation {id} ended before a turn finished"),
                );
            } else {
                log(&conv.nick, format!("conversation {id} ended"));
            }
        }
    }

    async fn tick_bot(&mut self, keys: &Keys, nick: &str, bot: &Bot, live: &[Entry]) -> Result<()> {
        let dir = bot.dir.as_deref().ok_or_else(|| anyhow!("no directory"))?;
        let inbox = Inbox::open(nick)?;
        let pending = inbox.peek()?;
        let interactive = |id: &str| live.iter().any(|e| e.session_id == id && !e.headless);
        // A dispatcher bot starts a new conversation for every new thread,
        // whether or not an interactive session of it is online.
        let online = !keys.dispatches(nick) && live.iter().any(|e| e.nick == nick && !e.headless);

        // Which conversations have to run, and in which directory.
        let mut table = threads::load(nick)?;
        let mut wanted: BTreeMap<String, (PathBuf, bool)> = BTreeMap::new();
        let mut unowned: BTreeMap<String, Vec<Record>> = BTreeMap::new();
        for r in &pending {
            match table.get(&threads::key(r)) {
                Some(owner) if !interactive(&owner.session_id) => {
                    wanted
                        .entry(owner.session_id.clone())
                        .or_insert((owner.dir.clone(), false));
                }
                Some(_) => {}
                None if !online => unowned.entry(threads::key(r)).or_default().push(r.clone()),
                None => {}
            }
        }
        // Each new thread gets a new conversation, which owns it from before
        // its start. A session may claim the thread in the meantime.
        for records in unowned.values() {
            let new_ones = wanted.values().filter(|(_, new)| *new).count();
            if self.count(nick) + new_ones >= MAX_PER_BOT {
                break;
            }
            let id = uuid::Uuid::new_v4().to_string();
            let owner = Owner {
                session_id: id.clone(),
                dir: dir.to_owned(),
            };
            threads::claim(nick, records, &owner)?;
            table = threads::load(nick)?;
            if table.get(&threads::key(&records[0])) == Some(&owner) {
                wanted.insert(id, (dir.to_owned(), true));
            }
        }

        for (id, (cdir, new)) in &wanted {
            if !self.running.contains_key(id) {
                if self.failed.get(id).is_some_and(|t| t.elapsed() < RETRY_GAP)
                    || self.count(nick) >= MAX_PER_BOT
                {
                    continue;
                }
                let conv = spawn(keys, nick, cdir, id, *new).await?;
                let how = if *new { "started" } else { "resumed" };
                let pid = conv.child.id().unwrap_or(0);
                log(nick, format!("{how} conversation {id} (pid {pid})"));
                self.running.insert(id.clone(), conv);
            }
            let conv = self.running.get_mut(id).expect("inserted above");
            let oldest = pending
                .iter()
                .filter(|r| owned_by(&table, r, id))
                .map(|r| age(r.timestamp))
                .max();
            if !conv.busy() && oldest.is_some_and(|a| a >= BATCH) {
                feed(conv, id, &inbox, &table, keys.trusted_user_id).await?;
            }
        }

        // End processes that are idle, or whose conversation the user took
        // over interactively. Their threads keep the owner.
        for (id, conv) in self.running.iter_mut().filter(|(_, c)| c.nick == nick) {
            let (busy, last_turn_end) = {
                let state = conv.state.lock().unwrap();
                (state.busy, state.last_turn_end)
            };
            if busy || conv.stdin.is_none() {
                continue;
            }
            let idle = last_turn_end.unwrap_or(conv.started).elapsed();
            let waiting = pending.iter().any(|r| owned_by(&table, r, id));
            let taken_over = interactive(id);
            if taken_over || (idle >= IDLE_END && !waiting) {
                let why = if taken_over {
                    "the user took it over"
                } else {
                    "idle"
                };
                log(nick, format!("ending conversation {id}: {why}"));
                conv.stdin = None;
            }
        }
        Ok(())
    }

    fn count(&self, nick: &str) -> usize {
        self.running.values().filter(|c| c.nick == nick).count()
    }
}

fn owned_by(table: &Table, record: &Record, session_id: &str) -> bool {
    table
        .get(&threads::key(record))
        .is_some_and(|o| o.session_id == session_id)
}

/// Time since a Zulip timestamp.
fn age(timestamp: i64) -> Duration {
    let now = chrono::Utc::now().timestamp();
    Duration::from_secs(now.saturating_sub(timestamp).max(0) as u64)
}

/// Start `claude -p` with stream-json input, in the conversation's own
/// directory: a new conversation with the given UUID, or a resumed one.
async fn spawn(
    keys: &Keys,
    nick: &str,
    dir: &Path,
    session_id: &str,
    new: bool,
) -> Result<Conversation> {
    let home = std::env::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    let claude = keys
        .claude_path
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/bin/claude"));
    let goniec = std::env::current_exe().context("cannot find the goniec binary")?;
    let state_dir = inbox::state_dir()?.join("headless");
    fs::create_dir_all(&state_dir)?;
    let reply_file = state_dir.join(format!("{session_id}.reply_to"));
    let stderr = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir.join(format!("{nick}.log")))?;

    let settings = serde_json::json!({
        "hooks": { "PermissionRequest": [{ "hooks": [{
            "type": "command",
            "command": format!("'{}' hook permission", goniec.display()),
            "timeout": PERMISSION_TIMEOUT,
        }] }] }
    });
    // The service's PATH may lack the user's own bin directories, and the
    // session runs `goniec send` from its Bash tool.
    let path = format!(
        "{}:{}:{}",
        home.join(".cargo/bin").display(),
        home.join(".local/bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let which = if new { "--session-id" } else { "--resume" };

    let mut child = Command::new(&claude)
        .args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--allowedTools",
            "Bash(goniec send *),Bash(goniec read *)",
            which,
            session_id,
            "--settings",
        ])
        .arg(settings.to_string())
        .current_dir(dir)
        .env("GONIEC_HEADLESS", "1")
        .env("GONIEC_DIR", dir)
        .env("GONIEC_REPLY_TO_FILE", &reply_file)
        .env("PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr))
        .spawn()
        .with_context(|| format!("cannot start {}", claude.display()))?;

    let stdin = child.stdin.take();
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let state = Arc::new(Mutex::new(State::default()));
    let bot = keys.bot(nick)?;
    let poster = Poster {
        nick: nick.to_owned(),
        client: Client::new(&keys.site, &bot.email, &bot.api_key)?,
        reply_file: reply_file.clone(),
    };
    tokio::spawn(read_output(poster, stdout, state.clone()));
    Ok(Conversation {
        nick: nick.to_owned(),
        child,
        stdin,
        state,
        started: Instant::now(),
        reply_file,
        fed_up_to: 0,
    })
}

/// Posts a conversation's answers to its thread, as the bot.
struct Poster {
    nick: String,
    client: Client,
    /// Names the thread of the current turn; `feed` writes it.
    reply_file: PathBuf,
}

/// Zulip refuses longer messages.
const MAX_MESSAGE_CHARS: usize = 9_000;

impl Poster {
    /// Post `text` to `target`, split at line ends where it is too long.
    async fn post(&self, target: &str, text: &str) -> Result<()> {
        for chunk in chunks(text, MAX_MESSAGE_CHARS) {
            match send::parse_target(target)? {
                Target::Channel { channel, topic } => {
                    self.client
                        .send_to_channel(&channel, &topic, &chunk)
                        .await?;
                }
                Target::UserId(id) => {
                    self.client.send_direct(id, &chunk).await?;
                }
                Target::User(name) => anyhow::bail!("unexpected reply target @{name}"),
            }
        }
        Ok(())
    }
}

/// Split `text` into pieces of at most `max` characters, at line ends where
/// possible.
fn chunks(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        if current.chars().count() + line.chars().count() > max && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        let mut line = line;
        while line.chars().count() > max {
            let cut = line.char_indices().nth(max).map_or(line.len(), |(i, _)| i);
            out.push(line[..cut].to_owned());
            line = &line[cut..];
        }
        current.push_str(line);
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

/// Follow the process's stream-json output. A `result` event ends a turn,
/// and its text is the answer, which goes to the turn's thread.
async fn read_output(
    poster: Poster,
    stdout: tokio::process::ChildStdout,
    state: Arc<Mutex<State>>,
) {
    let nick = poster.nick.clone();
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if event["type"] != "result" {
            continue;
        }
        let subtype = event["subtype"].as_str().unwrap_or("?").to_owned();
        let failed = subtype != "success" || event["is_error"].as_bool().unwrap_or(false);
        let text = if failed {
            format!("⚠ The turn ended with an error ({subtype}).")
        } else {
            event["result"].as_str().unwrap_or("").trim().to_owned()
        };
        // Read the thread before the turn counts as done: the next turn may
        // rewrite the file.
        let target = fs::read_to_string(&poster.reply_file).map(|t| t.trim().to_owned());
        {
            let mut s = state.lock().unwrap();
            s.busy = false;
            s.turns += 1;
            s.last_turn_end = Some(Instant::now());
        }
        log(&nick, format!("turn done ({subtype})"));
        match (target, text.is_empty()) {
            (_, true) => {}
            (Ok(target), false) => {
                if let Err(e) = poster.post(&target, &text).await {
                    log(&nick, format!("cannot post the answer: {e:#}"));
                }
            }
            (Err(e), false) => log(&nick, format!("no thread for the answer: {e}")),
        }
    }
    state.lock().unwrap().exited = true;
}

/// Send one turn: the trusted user's messages from this conversation's
/// threads as the prompt, and a count of the others, which the session
/// reads as data.
async fn feed(
    conv: &mut Conversation,
    id: &str,
    inbox: &Inbox,
    table: &Table,
    trusted: Option<u64>,
) -> Result<()> {
    let mine = |r: &Record| owned_by(table, r, id);
    let instructions = inbox.take_where(|r| mine(r) && headless::is_instruction(r, trusted))?;
    let others: Vec<Record> = inbox.peek()?.into_iter().filter(|r| mine(r)).collect();
    let news = others.iter().any(|r| r.id > conv.fed_up_to);
    if instructions.is_empty() && !news {
        return Ok(());
    }
    conv.fed_up_to = instructions
        .iter()
        .chain(&others)
        .map(|r| r.id)
        .max()
        .unwrap_or(conv.fed_up_to)
        .max(conv.fed_up_to);
    let reply_to = headless::reply_to(&instructions).or_else(|| others.last().map(threads::key));
    if let Some(to) = reply_to {
        fs::write(&conv.reply_file, to)?;
    }
    let prompt = headless::prompt(&instructions, others.len());
    let line = serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": prompt },
    });
    let stdin = conv
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow!("the conversation's input is closed"))?;
    stdin.write_all(format!("{line}\n").as_bytes()).await?;
    stdin.flush().await?;
    conv.state.lock().unwrap().busy = true;
    log(
        &conv.nick,
        format!(
            "turn: {} instruction(s), {} other message(s)",
            instructions.len(),
            others.len()
        ),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitting() {
        assert_eq!(chunks("short", 10), ["short"]);
        assert_eq!(chunks("aaaa\nbbbb\ncccc", 10), ["aaaa\nbbbb\n", "cccc"]);
        assert_eq!(chunks("abcdefghij12", 5), ["abcde", "fghij", "12"]);
        assert!(chunks("  \n", 10).is_empty());
    }
}
