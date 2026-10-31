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
use crate::pi;
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

/// A pi bot's model fills most of the one GPU, so it runs one at a time.
const MAX_PER_PI_BOT: usize = 1;

fn limit(keys: &Keys, nick: &str) -> usize {
    if keys.on_pi(nick) {
        MAX_PER_PI_BOT
    } else if keys.dispatches(nick) {
        MAX_PER_DISPATCHER
    } else {
        MAX_PER_BOT
    }
}

/// After a process that ended without finishing a turn, wait this long
/// before starting that conversation again.
const RETRY_GAP: Duration = Duration::from_secs(60);

/// Added to a headless session's system prompt. After the user approves a
/// blocked call, Claude Code tells the model only that it "may retry", and
/// drops a `PermissionDenied` hook's `additionalContext`. Without this
/// note haiku read the value another way instead of retrying (measured on
/// 2026-10-30); with it, it retried in 3 runs of 3. The user's message stays
/// the prompt, verbatim.
const RETRY_NOTE: &str = "This session runs headless; the user approves tool calls in a chat. \
    When the auto-mode classifier denies a tool call and Claude Code then says you may retry \
    it, the user has approved exactly that call in the chat: run the same call again, \
    unchanged, before you try anything else. Without that message, the denial stands.";

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
    /// On pi, the conversation's size in tokens after the last run.
    context_tokens: Option<u64>,
    /// The user stopped the last turn. Its partial answer stays in the
    /// conversation, and without a word the model went on with that task
    /// instead of the next message (seen live on 2026-10-31).
    stopped: bool,
}

/// A headless process running one conversation.
struct Conversation {
    nick: String,
    /// The directory it runs in.
    dir: PathBuf,
    /// The bot's client, to save the files of the user's messages.
    files_client: Client,
    /// `config::fingerprint` when the process started. None on pi: goniec
    /// writes pi's whole config itself.
    config: Option<u64>,
    /// The model of a pi process; None means a Claude Code process.
    pi_model: Option<String>,
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
    /// The last known size of pi conversations, in tokens, also after their
    /// process ended.
    context_tokens: HashMap<String, u64>,
}

impl Responder {
    /// The daemon is stopping: start no turn, and end every process as soon
    /// as its current turn is done, so that the turn's answer is still
    /// posted. A killed turn would be lost, because its messages already
    /// count as delivered. Returns how many processes still run.
    pub async fn drain(&mut self) -> usize {
        self.reap().await;
        for conv in self.running.values_mut() {
            if !conv.busy() {
                conv.stdin = None;
            }
        }
        self.running.len()
    }

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
            if let Some(tokens) = conv.state.lock().unwrap().context_tokens {
                self.context_tokens.insert(id.clone(), tokens);
            }
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
                model: None,
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
                // transcript to resume. pi takes the same flag for both.
                let new = config::transcript(id).is_none();
                let model = table
                    .values()
                    .find(|o| o.session_id == *id)
                    .and_then(|o| o.model.clone());
                let conv = spawn(keys, nick, cdir, id, new, model).await?;
                let how = match (keys.on_pi(nick), new) {
                    (true, _) => "running",
                    (false, true) => "started",
                    (false, false) => "resumed",
                };
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
                let changed = match conv.config {
                    Some(old) => config::fingerprint(&conv.dir, &config::claude_path(keys)?) != old,
                    None => false,
                };
                if changed {
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
        let result = match command {
            ChatCommand::Stop => self.stop(&nick, &record).await,
            ChatCommand::Compact(focus) => self.compact(&nick, &record, &focus).await,
            command => self.run_command(keys, &nick, &record, command),
        };
        let answer = match result {
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

    /// `!stop`: interrupt the thread's running turn. pi has an RPC command
    /// for it; on Claude Code goniec has none yet.
    async fn stop(&mut self, nick: &str, record: &Record) -> Result<String> {
        let thread = threads::key(record);
        let table = threads::load(nick)?;
        let conv = table
            .get(&thread)
            .and_then(|o| self.running.get_mut(&o.session_id));
        let Some(conv) = conv else {
            return Ok("Nothing runs in this thread.".to_owned());
        };
        if conv.pi_model.is_none() {
            return Ok("`!stop` works only on pi bots so far.".to_owned());
        }
        if !conv.busy() {
            return Ok("Nothing runs in this thread.".to_owned());
        }
        let stdin = conv
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("the conversation's input is closed"))?;
        let line = serde_json::json!({ "type": "abort" });
        stdin.write_all(format!("{line}\n").as_bytes()).await?;
        stdin.flush().await?;
        Ok("Stopping the current turn.".to_owned())
    }

    /// `!compact`: summarize the conversation now, on a pi bot. pi's answer
    /// ends this like a turn; `read_output` posts the result.
    async fn compact(&mut self, nick: &str, record: &Record, focus: &str) -> Result<String> {
        let thread = threads::key(record);
        let table = threads::load(nick)?;
        let conv = table
            .get(&thread)
            .and_then(|o| self.running.get_mut(&o.session_id));
        let Some(conv) = conv else {
            return Ok(
                "The conversation is not running. Send a message first, then `!compact`."
                    .to_owned(),
            );
        };
        if conv.pi_model.is_none() {
            return Ok("`!compact` works only on pi bots.".to_owned());
        }
        if conv.busy() {
            return Ok("A turn is running. Send `!compact` after it.".to_owned());
        }
        let line = if focus.is_empty() {
            serde_json::json!({ "type": "compact" })
        } else {
            serde_json::json!({ "type": "compact", "customInstructions": focus })
        };
        fs::write(&conv.reply_file, &thread)?;
        let stdin = conv
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("the conversation's input is closed"))?;
        stdin.write_all(format!("{line}\n").as_bytes()).await?;
        stdin.flush().await?;
        conv.state.lock().unwrap().busy = true;
        Ok("Compacting. This can take a few minutes on a long conversation.".to_owned())
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
                            model: None,
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
                        model: None,
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
            ChatCommand::New => {
                let Some(owner) = owner else {
                    return Ok("This thread has no conversation yet.".to_owned());
                };
                if interactive(&owner.session_id).is_some() {
                    return Ok(format!(
                        "Conversation `{}` runs in an interactive session, and it would lose \
                         this thread. `!headless` moves the thread instead.",
                        owner.session_id
                    ));
                }
                if let Some(conv) = self.running.get_mut(&owner.session_id) {
                    conv.end_after_turn = Some("!new");
                }
                // The thread's model is the thread's, so it stays.
                match &owner.model {
                    Some(_) => threads::set(
                        nick,
                        &thread,
                        &Owner {
                            session_id: uuid::Uuid::new_v4().to_string(),
                            ..owner.clone()
                        },
                    )?,
                    None => threads::remove(nick, &thread)?,
                }
                Ok(format!(
                    "This thread left conversation `{}`. Your next message starts a new one.",
                    owner.session_id
                ))
            }
            ChatCommand::Model(words) => {
                if !keys.on_pi(nick) {
                    return Ok(format!(
                        "@{nick} runs on Claude Code. `!model` is for pi bots."
                    ));
                }
                if words.is_empty() {
                    let current = owner
                        .and_then(|o| o.model)
                        .unwrap_or_else(|| pi::default_model(keys));
                    return Ok(format!("This thread runs `{current}`. {}", pi::usage()));
                }
                let model = pi::parse_model(keys, &words)?;
                let owner = match owner {
                    Some(owner) => owner,
                    None => Owner {
                        session_id: uuid::Uuid::new_v4().to_string(),
                        dir: keys
                            .bot(nick)?
                            .dir
                            .clone()
                            .ok_or_else(|| anyhow!("@{nick} has no directory"))?,
                        model: None,
                    },
                };
                // A process keeps its model, and a variant is a reload
                // anyway: the next message starts the conversation again.
                if let Some(conv) = self.running.get_mut(&owner.session_id) {
                    conv.end_after_turn = Some("!model");
                }
                threads::set(
                    nick,
                    &thread,
                    &Owner {
                        model: Some(model.clone()),
                        ..owner
                    },
                )?;
                Ok(format!("This thread now runs `{model}`."))
            }
            ChatCommand::Stop | ChatCommand::Compact(_) => {
                unreachable!("handled in `command`")
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
        if keys.on_pi(nick) {
            let model = owner
                .and_then(|o| o.model.clone())
                .unwrap_or_else(|| pi::default_model(keys));
            let window = pi::window(&model);
            let used = owner.and_then(|o| {
                let running = self.running.get(&o.session_id);
                running
                    .and_then(|c| c.state.lock().unwrap().context_tokens)
                    .or_else(|| self.context_tokens.get(&o.session_id).copied())
            });
            out.push_str(&format!("- model: `{model}`\n"));
            match used {
                Some(used) => out.push_str(&format!(
                    "- context: {used} of {window} tokens ({}%)\n",
                    used * 100 / window.max(1)
                )),
                None => out.push_str(&format!("- context window: {window} tokens\n")),
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
    model: Option<String>,
) -> Result<Conversation> {
    let home = std::env::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    let goniec = std::env::current_exe().context("cannot find the goniec binary")?;
    let state_dir = inbox::state_dir()?.join("headless");
    fs::create_dir_all(&state_dir)?;
    let reply_file = state_dir.join(format!("{session_id}.reply_to"));
    let stderr = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir.join(format!("{nick}.log")))?;
    // The service's PATH may lack the user's own bin directories, and the
    // session runs `goniec send` from its Bash tool.
    let path = format!(
        "{}:{}:{}",
        home.join(".cargo/bin").display(),
        home.join(".local/bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let on_pi = keys.on_pi(nick);
    let pi_model = on_pi.then(|| model.unwrap_or_else(|| pi::default_model(keys)));
    let (mut command, fingerprint) = match &pi_model {
        Some(model) => (pi_command(keys, session_id, model, &goniec)?, None),
        None => {
            let claude = config::claude_path(keys)?;
            let fingerprint = config::fingerprint(dir, &claude);
            (
                claude_command(&claude, session_id, new, &goniec)?,
                Some(fingerprint),
            )
        }
    };
    let mut child = command
        .current_dir(dir)
        .env("GONIEC_HEADLESS", "1")
        .env("GONIEC_DIR", dir)
        .env("GONIEC_REPLY_TO_FILE", &reply_file)
        .env("PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr))
        .spawn()
        .with_context(|| format!("cannot start the session of @{nick}"))?;

    let stdin = child.stdin.take();
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let state = Arc::new(Mutex::new(State::default()));
    let bot = keys.bot(nick)?;
    let poster = Poster {
        nick: nick.to_owned(),
        client: Client::new(&keys.site, &bot.email, &bot.api_key)?,
        reply_file: reply_file.clone(),
    };
    tokio::spawn(read_output(poster, stdout, state.clone(), on_pi));
    Ok(Conversation {
        nick: nick.to_owned(),
        dir: dir.to_owned(),
        files_client: Client::new(&keys.site, &bot.email, &bot.api_key)?,
        config: fingerprint,
        pi_model,
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

/// `claude -p` for one conversation: a new one with this UUID, or a resumed
/// one.
fn claude_command(claude: &Path, session_id: &str, new: bool, goniec: &Path) -> Result<Command> {
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
    let which = if new { "--session-id" } else { "--resume" };
    let mut command = Command::new(claude);
    command
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
            "--append-system-prompt",
            RETRY_NOTE,
            "--allowedTools",
            "Bash(goniec send *),Bash(goniec read *)",
            which,
            session_id,
            "--settings",
        ])
        .arg(settings.to_string());
    Ok(command)
}

/// `pi --mode rpc` for one conversation. `--session-id` creates the session
/// or continues it. Only goniec's gate loads: no user or project extension,
/// no MCP server (their tools have not been tested behind the gate), no
/// skill.
fn pi_command(keys: &Keys, session_id: &str, model: &str, goniec: &Path) -> Result<Command> {
    let setup = pi::prepare(keys)?;
    let mut command = Command::new(pi::binary(keys)?);
    command
        .args(["--mode", "rpc", "--session-id", session_id, "--model"])
        .arg(format!("{}/{model}", pi::PROVIDER))
        .args(["--no-extensions", "--extension"])
        .arg(&setup.gate)
        .args([
            "--no-approve",
            "--no-mcp",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--offline",
        ])
        .env("PI_CODING_AGENT_DIR", &setup.agent_dir)
        .env("GONIEC_BIN", goniec)
        .env("GONIEC_SESSION_ID", session_id)
        .env("GONIEC_MODEL", model);
    Ok(command)
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

/// The end of a turn: mark the process idle, and post the answer to the
/// turn's thread.
async fn finish_turn(poster: &Poster, state: &Mutex<State>, how: &str, text: &str) {
    // Read the thread before the turn counts as done: the next turn may
    // rewrite the file.
    let target = fs::read_to_string(&poster.reply_file).map(|t| t.trim().to_owned());
    {
        let mut s = state.lock().unwrap();
        s.busy = false;
        s.turns += 1;
        s.last_turn_end = Some(Instant::now());
    }
    let nick = &poster.nick;
    log(nick, format!("turn done ({how})"));
    match (target, text.is_empty()) {
        (_, true) => {}
        (Ok(target), false) => {
            if let Err(e) = poster.post(&target, text).await {
                log(nick, format!("cannot post the answer: {e:#}"));
            }
        }
        (Err(e), false) => log(nick, format!("no thread for the answer: {e}")),
    }
}

/// Follow the process's JSON output. On Claude Code a `result` event ends a
/// turn, and its text is the answer. On pi `agent_end` carries the answer,
/// but retries or compaction can follow, so `agent_settled` ends the turn.
async fn read_output(
    poster: Poster,
    stdout: tokio::process::ChildStdout,
    state: Arc<Mutex<State>>,
    on_pi: bool,
) {
    let mut lines = BufReader::new(stdout).lines();
    let mut answer = (String::new(), None::<String>);
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let kind = event["type"].as_str().unwrap_or("");
        match (on_pi, kind) {
            (false, "result") => {
                let subtype = event["subtype"].as_str().unwrap_or("?");
                let failed = subtype != "success" || event["is_error"].as_bool().unwrap_or(false);
                let text = if failed {
                    format!("⚠ The turn ended with an error ({subtype}).")
                } else {
                    event["result"].as_str().unwrap_or("").trim().to_owned()
                };
                finish_turn(&poster, &state, subtype, &text).await;
            }
            (true, "agent_end") => {
                answer = pi::answer(&event);
                if let Some(tokens) = pi::context_tokens(&event) {
                    state.lock().unwrap().context_tokens = Some(tokens);
                }
            }
            (true, "agent_settled") => {
                let (text, error) = std::mem::take(&mut answer);
                let aborted = event["aborted"].as_bool().unwrap_or(false);
                state.lock().unwrap().stopped = aborted;
                let (how, text) = match error {
                    _ if aborted => ("aborted", "⏹ Stopped.".to_owned()),
                    Some(error) => ("error", format!("⚠ The turn ended with an error: {error}")),
                    None => ("success", text),
                };
                finish_turn(&poster, &state, how, &text).await;
            }
            (true, "response") if event["command"] == "compact" => {
                let text = if event["success"] == true {
                    let after = event["data"]["estimatedTokensAfter"].as_u64();
                    state.lock().unwrap().context_tokens = after;
                    format!(
                        "Compacted: {} tokens, now about {}.",
                        event["data"]["tokensBefore"],
                        after.unwrap_or(0)
                    )
                } else {
                    let error = event["error"].as_str().unwrap_or("pi refused");
                    format!("⚠ Compaction failed: {error}")
                };
                finish_turn(&poster, &state, "compact", &text).await;
            }
            // A prompt pi refused, or handled without a run, ends no run.
            (true, "response") if event["command"] == "prompt" => {
                if event["success"] == false {
                    let error = event["error"].as_str().unwrap_or("pi refused the prompt");
                    let text = format!("⚠ {error}");
                    finish_turn(&poster, &state, "refused", &text).await;
                } else if event["data"]["disposition"] == "handled" {
                    finish_turn(&poster, &state, "handled", "").await;
                }
            }
            _ => {}
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
    if std::mem::take(&mut conv.state.lock().unwrap().stopped) {
        prompt = format!("{}\n{prompt}", headless::STOPPED_LINE);
    }
    let images = headless::images(&instructions);
    let line = if conv.pi_model.is_some() {
        pi::prompt_line(&prompt, &images)
    } else {
        let content = if images.is_empty() {
            serde_json::Value::String(prompt)
        } else {
            let mut blocks = vec![serde_json::json!({ "type": "text", "text": prompt })];
            blocks.extend(images);
            serde_json::Value::Array(blocks)
        };
        serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": content },
        })
    };
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
