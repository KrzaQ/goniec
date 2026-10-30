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

use crate::commands::{self, Command as ChatCommand, Order};
use crate::config;
use crate::headless;
use crate::inbox::{self, Inbox, Record};
use crate::keys::{Bot, Keys};
use crate::send::{self, Target};
use crate::sessions::{self, Entry};
use crate::threads::{self, Owner, Table};
use crate::uploads;
use crate::zulip::Client;

/// Collect messages for this long before a turn, so that a burst of lines
/// becomes one turn.
const BATCH: Duration = Duration::from_secs(2);

/// End a headless process after this long without a turn. Its threads keep
/// their owner, so the next message resumes it.
const IDLE_END: Duration = Duration::from_secs(15 * 60);

/// At most this many headless processes per bot at a time; a dispatcher
/// bot, which starts a conversation for every new thread, gets more. A
/// conversation over the limit evicts the one idle the longest.
const MAX_PER_BOT: usize = 3;
const MAX_PER_DISPATCHER: usize = 5;

fn limit(keys: &Keys, nick: &str) -> usize {
    if keys.dispatches(nick) {
        MAX_PER_DISPATCHER
    } else {
        MAX_PER_BOT
    }
}

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
    /// The directory it runs in.
    dir: PathBuf,
    /// The bot's client, to save the files of the user's messages.
    files_client: Client,
    /// `config::fingerprint` when the process started.
    config: u64,
    /// End the process after its current turn, for this reason: `!restart`,
    /// a new configuration, or a thread that went elsewhere.
    end_after_turn: Option<&'static str>,
    child: Child,
    stdin: Option<ChildStdin>,
    state: Arc<Mutex<State>>,
    started: Instant,
    reply_file: PathBuf,
    /// The newest message the conversation was told about. Messages from
    /// others that it left unread do not start another turn.
    fed_up_to: u64,
    /// The thread of the previous turn. A turn from another thread starts
    /// with a line that names it.
    last_thread: Option<String>,
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
        // At most a limit's worth of new conversations per tick; they make
        // room for themselves by evicting older ones.
        for records in unowned.values() {
            let new_ones = wanted.values().filter(|(_, new)| *new).count();
            if new_ones >= limit(keys, nick) {
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

        for (id, (cdir, _)) in &wanted {
            if !self.running.contains_key(id) {
                if self.failed.get(id).is_some_and(|t| t.elapsed() < RETRY_GAP) {
                    continue;
                }
                if self.count(nick) >= limit(keys, nick) {
                    self.make_room(nick, &pending, &table);
                    continue;
                }
                // A conversation that never finished a turn has no
                // transcript to resume.
                let new = config::transcript(id).is_none();
                let conv = spawn(keys, nick, cdir, id, new).await?;
                let how = if new { "started" } else { "resumed" };
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
            let open = conv.stdin.is_some() && conv.end_after_turn.is_none();
            if open && !conv.busy() && oldest.is_some_and(|a| a >= BATCH) {
                // MCP servers, plugins and the version load only at start: a
                // process with an old configuration ends, and the next tick
                // resumes the conversation with the new one.
                if config::fingerprint(&conv.dir, &config::claude_path(keys)?) != conv.config {
                    conv.end_after_turn = Some("its configuration changed");
                } else {
                    feed(conv, id, &inbox, &table, keys.trusted_user_id).await?;
                }
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
            if taken_over || conv.end_after_turn.is_some() || (idle >= IDLE_END && !waiting) {
                let why = if taken_over {
                    "the user took it over"
                } else {
                    conv.end_after_turn.unwrap_or("idle")
                };
                log(nick, format!("ending conversation {id}: {why}"));
                conv.stdin = None;
            }
        }
        Ok(())
    }

    /// Run the trusted user's command for one bot, and answer in the
    /// command's thread as that bot.
    pub async fn command(&mut self, keys: &Keys, order: Order) {
        let Order {
            nick,
            record,
            command,
        } = order;
        let answer = match self.run_command(keys, &nick, &record, command) {
            Ok(text) => text,
            Err(e) => format!("⚠ {e:#}"),
        };
        let thread = threads::key(&record);
        let posted = match keys.bot(&nick) {
            Ok(bot) => match Client::new(&keys.site, &bot.email, &bot.api_key) {
                Ok(client) => post(&client, &thread, &answer).await,
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        if let Err(e) = posted {
            log(&nick, format!("cannot answer a command: {e:#}"));
        }
    }

    fn run_command(
        &mut self,
        keys: &Keys,
        nick: &str,
        record: &Record,
        command: ChatCommand,
    ) -> Result<String> {
        let thread = threads::key(record);
        let owner = threads::load(nick)?.get(&thread).cloned();
        let live = sessions::live()?;
        let interactive = |id: &str| live.iter().find(|e| e.session_id == id && !e.headless);
        match command {
            ChatCommand::Status => Ok(self.status(keys, nick, &thread, owner.as_ref(), &live)),
            ChatCommand::Restart => {
                let Some(owner) = owner else {
                    return Ok("No conversation owns this thread yet.".to_owned());
                };
                if interactive(&owner.session_id).is_some() {
                    return Ok(format!(
                        "Conversation `{}` runs in an interactive session. `!restart` works only \
                         for a headless one.",
                        owner.session_id
                    ));
                }
                match self.running.get_mut(&owner.session_id) {
                    Some(conv) => {
                        conv.end_after_turn = Some("!restart");
                        Ok(format!(
                            "Conversation `{}` ends after its current turn. The next message \
                             resumes it with the current configuration.",
                            owner.session_id
                        ))
                    }
                    None => Ok(format!(
                        "Conversation `{}` is not running. The next message starts it with the \
                         current configuration.",
                        owner.session_id
                    )),
                }
            }
            ChatCommand::Headless(text) => {
                if !keys.responds_for(nick) {
                    return Ok(format!(
                        "@{nick} has no headless responder. Add it to `responder` in goniec.toml."
                    ));
                }
                let mut answer = match &owner {
                    Some(o) if interactive(&o.session_id).is_none() => format!(
                        "This thread already belongs to headless conversation `{}`.",
                        o.session_id
                    ),
                    _ => {
                        let dir = keys
                            .bot(nick)?
                            .dir
                            .clone()
                            .ok_or_else(|| anyhow!("@{nick} has no directory"))?;
                        let new = Owner {
                            session_id: uuid::Uuid::new_v4().to_string(),
                            dir,
                        };
                        threads::set(nick, &thread, &new)?;
                        let mut answer = format!(
                            "This thread now belongs to a new headless conversation `{}`.",
                            new.session_id
                        );
                        if let Some(old) = &owner {
                            answer.push_str(&format!(
                                " The interactive session `{}` no longer gets it.",
                                old.session_id
                            ));
                        }
                        answer
                    }
                };
                // The text after the command is the conversation's first
                // prompt: it goes through the inbox like any message.
                if !text.is_empty() {
                    let mut prompt = record.clone();
                    prompt.content = text;
                    Inbox::open(nick)?.put(&prompt)?;
                } else if !threads::load(nick)?
                    .get(&thread)
                    .is_some_and(|o| self.running.contains_key(&o.session_id))
                {
                    answer.push_str(" It starts with your next message.");
                }
                Ok(answer)
            }
            ChatCommand::Interactive => {
                let Some(session) = live
                    .iter()
                    .filter(|e| e.nick == nick && !e.headless)
                    .max_by_key(|e| e.start)
                else {
                    let stays = owner.map_or("without an owner".to_owned(), |o| {
                        format!("with conversation `{}`", o.session_id)
                    });
                    return Ok(format!(
                        "No interactive session of @{nick} is online. The thread stays {stays}."
                    ));
                };
                if owner
                    .as_ref()
                    .is_some_and(|o| o.session_id == session.session_id)
                {
                    return Ok(format!(
                        "This thread already belongs to the interactive session `{}`.",
                        session.session_id
                    ));
                }
                threads::set(
                    nick,
                    &thread,
                    &Owner {
                        session_id: session.session_id.clone(),
                        dir: session.dir.clone(),
                    },
                )?;
                if let Some(conv) = owner
                    .as_ref()
                    .and_then(|o| self.running.get_mut(&o.session_id))
                {
                    conv.end_after_turn = Some("!interactive");
                }
                // The session learns of the thread through the command
                // message itself, with a note from goniec.
                let mut note = "This thread is yours now (the user ran `!interactive`).".to_owned();
                match &owner {
                    Some(o) => {
                        note.push_str(&format!(
                            " Until now it belonged to conversation {}. What was said: \
                             goniec read {}",
                            o.session_id,
                            inbox::shell_quote(&thread)
                        ));
                        if let Some(path) = config::transcript(&o.session_id) {
                            note.push_str(&format!(". Its transcript: {}", path.display()));
                        }
                    }
                    None => note.push_str(&format!(
                        " What was said: goniec read {}",
                        inbox::shell_quote(&thread)
                    )),
                }
                let mut notice = record.clone();
                notice.note = Some(note);
                Inbox::open(nick)?.put(&notice)?;
                Ok(format!(
                    "This thread now belongs to the interactive session `{}` in `{}`.",
                    session.session_id,
                    session.dir.display()
                ))
            }
            ChatCommand::Unknown(word) => Ok(format!(
                "Unknown command `!{word}`. Commands: {}.",
                commands::HELP
            )),
        }
    }

    /// The answer to `!status`.
    fn status(
        &self,
        keys: &Keys,
        nick: &str,
        thread: &str,
        owner: Option<&Owner>,
        live: &[Entry],
    ) -> String {
        let online = live
            .iter()
            .filter(|e| e.nick == nick && !e.headless)
            .count();
        let mut out = format!("@{nick} · thread `{thread}`\n");
        match owner {
            None => {
                let next = if keys.dispatches(nick) || (online == 0 && keys.responds_for(nick)) {
                    "a new headless conversation"
                } else if online > 0 {
                    "the interactive session"
                } else {
                    "the inbox, until a session of the bot starts"
                };
                out.push_str(&format!(
                    "- owner: none yet; the next message goes to {next}\n"
                ));
            }
            Some(o) => {
                let state = if let Some(e) = live
                    .iter()
                    .find(|e| e.session_id == o.session_id && !e.headless)
                {
                    format!("interactive, pid {}", e.pid)
                } else if let Some(conv) = self.running.get(&o.session_id) {
                    let s = conv.state.lock().unwrap();
                    let pid = conv.child.id().unwrap_or(0);
                    if s.busy {
                        format!("headless, pid {pid}, in a turn")
                    } else {
                        let idle = s.last_turn_end.unwrap_or(conv.started).elapsed().as_secs() / 60;
                        format!("headless, pid {pid}, idle for {idle} min")
                    }
                } else if keys.responds_for(nick) {
                    "not running; the next message resumes it headless".to_owned()
                } else {
                    "not running; messages wait until it is resumed".to_owned()
                };
                out.push_str(&format!(
                    "- owner: conversation `{}` ({state})\n- directory: `{}`\n",
                    o.session_id,
                    o.dir.display()
                ));
            }
        }
        out.push_str(&format!(
            "- interactive sessions of @{nick} online: {online}"
        ));
        out
    }

    /// End the bot's process that has waited longest since its last turn,
    /// after that turn, so a conversation over the limit can start. Its
    /// threads keep their owner, so its next message resumes it. A process
    /// with messages waiting for it stays, and nothing is evicted while
    /// another eviction of the bot is under way.
    fn make_room(&mut self, nick: &str, pending: &[Record], table: &Table) {
        let mine = || self.running.iter().filter(|(_, c)| c.nick == nick);
        if mine().any(|(_, c)| c.end_after_turn.is_some() || c.stdin.is_none()) {
            return;
        }
        let victim = mine()
            .filter(|(id, _)| !pending.iter().any(|r| owned_by(table, r, id)))
            .min_by_key(|(_, c)| {
                let state = c.state.lock().unwrap();
                (state.busy, state.last_turn_end.unwrap_or(c.started))
            })
            .map(|(id, _)| id.clone());
        if let Some(id) = victim {
            log(nick, format!("evicting conversation {id} for a newer one"));
            if let Some(conv) = self.running.get_mut(&id) {
                conv.end_after_turn = Some("evicted for a newer conversation");
            }
        }
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
    let claude = config::claude_path(keys)?;
    let fingerprint = config::fingerprint(dir, &claude);
    let goniec = std::env::current_exe().context("cannot find the goniec binary")?;
    let state_dir = inbox::state_dir()?.join("headless");
    fs::create_dir_all(&state_dir)?;
    let reply_file = state_dir.join(format!("{session_id}.reply_to"));
    let stderr = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir.join(format!("{nick}.log")))?;

    // `permission` answers what reaches the permission flow (ask rules, and
    // the fallback after repeated blocks). `denied` asks the user about a
    // call the auto-mode classifier blocked, and `pretool` allows the retry
    // once the user said yes.
    let hook = |event: &str, timeout: u64| {
        serde_json::json!([{ "hooks": [{
            "type": "command",
            "command": format!("'{}' hook {event}", goniec.display()),
            "timeout": timeout,
        }] }])
    };
    // A deny rule keeps the session's Edit and Write tools out of the grants,
    // a second wall: `hook pretool` checks every grant with Zulip anyway.
    let settings = serde_json::json!({
        "permissions": {
            "deny": [format!("Edit(/{}/**)", crate::grants::dir()?.display())],
        },
        "hooks": {
            "PermissionRequest": hook("permission", PERMISSION_TIMEOUT),
            "PermissionDenied": hook("denied", PERMISSION_TIMEOUT),
            "PreToolUse": hook("pretool", 10),
        }
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
            // Auto mode: the classifier allows what is safe and what the
            // user's prompt asks for, and blocks the rest. The user wanted
            // to be asked only about calls the classifier will not allow.
            "--permission-mode",
            "auto",
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
        dir: dir.to_owned(),
        files_client: Client::new(&keys.site, &bot.email, &bot.api_key)?,
        config: fingerprint,
        end_after_turn: None,
        child,
        stdin,
        state,
        started: Instant::now(),
        reply_file,
        fed_up_to: 0,
        last_thread: None,
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
    async fn post(&self, target: &str, text: &str) -> Result<()> {
        post(&self.client, target, text).await
    }
}

/// Post `text` to `target`, split at line ends where it is too long.
async fn post(client: &Client, target: &str, text: &str) -> Result<()> {
    for chunk in chunks(text, MAX_MESSAGE_CHARS) {
        match send::parse_target(target)? {
            Target::Channel { channel, topic } => {
                client.send_to_channel(&channel, &topic, &chunk).await?;
            }
            Target::UserId(id) => {
                client.send_direct(id, &chunk).await?;
            }
            Target::User(name) => anyhow::bail!("unexpected reply target @{name}"),
        }
    }
    Ok(())
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
    // The files of the user's messages go to the conversation's own temp
    // directory, and their images into the prompt.
    let mut instructions = instructions;
    let files = uploads::files_dir(&conv.dir, id);
    uploads::assign(&mut instructions, &files);
    let errors = uploads::save_all(&conv.files_client, &instructions, &files).await;
    let mut prompt = headless::prompt(&instructions, others.len());
    for error in errors {
        prompt.push_str(&format!("\n{error}"));
    }
    if let Some(first) = instructions.last().or(others.last())
        && conv.last_thread.as_deref() != Some(threads::key(first).as_str())
    {
        prompt = format!("{}\n{prompt}", headless::thread_line(first));
        conv.last_thread = Some(threads::key(first));
    }
    let images = headless::images(&instructions);
    let content = if images.is_empty() {
        serde_json::Value::String(prompt)
    } else {
        let mut blocks = vec![serde_json::json!({ "type": "text", "text": prompt })];
        blocks.extend(images);
        serde_json::Value::Array(blocks)
    };
    let line = serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": content },
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
