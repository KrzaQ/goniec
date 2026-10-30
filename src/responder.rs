//! The headless responder: when a message waits for a bound bot whose
//! session is offline, start a headless Claude Code session in the bot's
//! directory and feed it turns.
//!
//! The trusted user's messages become the prompt (see `headless`). Every
//! other message stays in the inbox, and the session reads it as data.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

use crate::headless;
use crate::inbox::{self, Inbox, Record, reply_target};
use crate::keys::{Bot, Keys};

/// Wait this long after the oldest message before starting a session, in
/// case the user is restarting the interactive one.
const GRACE: Duration = Duration::from_secs(30);

/// Collect messages for this long before a turn.
const BATCH: Duration = Duration::from_secs(10);

/// End a session after this long without a turn.
const IDLE_END: Duration = Duration::from_secs(15 * 60);

/// Start at most one session per bot in this time.
const SPAWN_GAP: Duration = Duration::from_secs(5 * 60);

/// The permission hook's timeout. The hook itself gives up after 14 minutes.
const PERMISSION_TIMEOUT: u64 = 15 * 60;

fn log(nick: &str, msg: impl std::fmt::Display) {
    eprintln!("goniec responder: @{nick}: {msg}");
}

/// What the stdout reader tells the responder about a session.
#[derive(Default)]
struct State {
    busy: bool,
    exited: bool,
    last_turn_end: Option<Instant>,
}

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    state: Arc<Mutex<State>>,
    started: Instant,
    reply_file: PathBuf,
    /// The newest message the session was told about. Messages from others
    /// that it left unread do not start another turn.
    fed_up_to: u64,
}

#[derive(Default)]
pub struct Responder {
    sessions: HashMap<String, Session>,
    last_spawn: HashMap<String, Instant>,
}

impl Responder {
    /// Look at every bound bot once. Errors are logged per bot.
    pub async fn tick(&mut self, keys: &Keys, bound: &HashMap<String, Bot>) {
        let ours: HashSet<u32> = self
            .sessions
            .values()
            .filter_map(|s| s.child.id())
            .collect();
        for (nick, bot) in bound.iter().filter(|(nick, _)| keys.responds_for(nick)) {
            if let Err(e) = self.tick_bot(keys, nick, bot, &ours).await {
                log(nick, format!("{e:#}"));
            }
        }
        // Sessions of bots that are no longer bound, or no longer served,
        // end with their input.
        self.sessions.retain(|nick, s| {
            let keep = bound.contains_key(nick) && keys.responds_for(nick);
            if !keep {
                s.stdin = None;
            }
            keep
        });
    }

    async fn tick_bot(
        &mut self,
        keys: &Keys,
        nick: &str,
        bot: &Bot,
        ours: &HashSet<u32>,
    ) -> Result<()> {
        let dir = bot.dir.as_deref().ok_or_else(|| anyhow!("no directory"))?;
        let inbox = Inbox::open(nick)?;
        let pending = inbox.peek()?;
        let oldest_age = pending.first().map(|r| age(r.timestamp));

        if let Some(session) = self.sessions.get_mut(nick) {
            let (busy, exited, last_turn_end) = {
                let state = session.state.lock().unwrap();
                (state.busy, state.exited, state.last_turn_end)
            };
            if exited {
                log(nick, "session ended");
                let _ = session.child.wait().await;
                self.sessions.remove(nick);
                return Ok(());
            }
            if busy {
                return Ok(());
            }
            let idle_since = last_turn_end.unwrap_or(session.started);
            let interactive = interactive_session(dir, ours);
            if (pending.is_empty() && idle_since.elapsed() >= IDLE_END) || interactive {
                log(
                    nick,
                    if interactive {
                        "an interactive session took over; ending"
                    } else {
                        "idle; ending"
                    },
                );
                session.stdin = None;
                return Ok(());
            }
            if oldest_age.is_some_and(|a| a >= BATCH) {
                feed(nick, session, &inbox, keys.trusted_user_id).await?;
            }
            return Ok(());
        }

        if oldest_age.is_none_or(|a| a < GRACE)
            || self
                .last_spawn
                .get(nick)
                .is_some_and(|t| t.elapsed() < SPAWN_GAP)
            || interactive_session(dir, ours)
        {
            return Ok(());
        }
        self.last_spawn.insert(nick.to_owned(), Instant::now());
        let mut session = spawn(keys, nick, dir).await?;
        log(
            nick,
            format!(
                "started a headless session (pid {})",
                session.child.id().unwrap_or(0)
            ),
        );
        feed(nick, &mut session, &inbox, keys.trusted_user_id).await?;
        self.sessions.insert(nick.to_owned(), session);
        Ok(())
    }
}

/// Seconds since a Zulip timestamp, as a duration.
fn age(timestamp: i64) -> Duration {
    let now = chrono::Utc::now().timestamp();
    Duration::from_secs(now.saturating_sub(timestamp).max(0) as u64)
}

/// A live `claude` process, other than our own headless ones, works in
/// `dir`. Claude Code keeps one file per process in `~/.claude/sessions/`,
/// with the process's `pid` and `cwd`. The file is internal; if it cannot be
/// read, this says no.
fn interactive_session(dir: &Path, ours: &HashSet<u32>) -> bool {
    let Some(sessions) = claude_config_dir().map(|d| d.join("sessions")) else {
        return false;
    };
    let Ok(entries) = fs::read_dir(sessions) else {
        return false;
    };
    entries.filter_map(|e| e.ok()).any(|entry| {
        let Ok(text) = fs::read_to_string(entry.path()) else {
            return false;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            return false;
        };
        let pid = v["pid"].as_u64().unwrap_or(0) as u32;
        v["cwd"].as_str().map(Path::new) == Some(dir)
            && !ours.contains(&pid)
            && Path::new(&format!("/proc/{pid}")).exists()
    })
}

fn claude_config_dir() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => Some(dir.into()),
        None => std::env::home_dir().map(|h| h.join(".claude")),
    }
}

/// Start `claude -p` with stream-json input in the bot's directory.
async fn spawn(keys: &Keys, nick: &str, dir: &Path) -> Result<Session> {
    let home = std::env::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    let claude = keys
        .claude_path
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/bin/claude"));
    let goniec = std::env::current_exe().context("cannot find the goniec binary")?;
    let state_dir = inbox::state_dir()?.join("headless");
    fs::create_dir_all(&state_dir)?;
    let reply_file = state_dir.join(format!("{nick}.reply_to"));
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

    let mut child = Command::new(&claude)
        .args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--allowedTools",
            "Bash(goniec send *),Bash(goniec read)",
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
    tokio::spawn(read_output(nick.to_owned(), stdout, state.clone()));
    Ok(Session {
        child,
        stdin,
        state,
        started: Instant::now(),
        reply_file,
        fed_up_to: 0,
    })
}

/// Follow the session's stream-json output: a `result` event ends a turn.
async fn read_output(nick: String, stdout: tokio::process::ChildStdout, state: Arc<Mutex<State>>) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if event["type"] == "result" {
            let subtype = event["subtype"].as_str().unwrap_or("?");
            log(&nick, format!("turn done ({subtype})"));
            let mut s = state.lock().unwrap();
            s.busy = false;
            s.last_turn_end = Some(Instant::now());
        }
    }
    state.lock().unwrap().exited = true;
}

/// Send one turn: the trusted user's messages as the prompt, and a count of
/// the others, which the session reads as data.
async fn feed(
    nick: &str,
    session: &mut Session,
    inbox: &Inbox,
    trusted: Option<u64>,
) -> Result<()> {
    let instructions = inbox.take_where(|r| headless::is_instruction(r, trusted))?;
    let others: Vec<Record> = inbox.peek()?;
    let news = others.iter().any(|r| r.id > session.fed_up_to);
    if instructions.is_empty() && !news {
        return Ok(());
    }
    session.fed_up_to = instructions
        .iter()
        .chain(&others)
        .map(|r| r.id)
        .max()
        .unwrap_or(session.fed_up_to)
        .max(session.fed_up_to);
    let reply_to = headless::reply_to(&instructions).or_else(|| others.last().map(reply_target));
    if let Some(to) = reply_to {
        fs::write(&session.reply_file, to)?;
    }
    let prompt = headless::prompt(nick, &instructions, others.len());
    let line = serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": prompt },
    });
    let stdin = session
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow!("the session's input is closed"))?;
    stdin.write_all(format!("{line}\n").as_bytes()).await?;
    stdin.flush().await?;
    session.state.lock().unwrap().busy = true;
    log(
        nick,
        format!(
            "turn: {} instruction(s), {} other message(s)",
            instructions.len(),
            others.len()
        ),
    );
    Ok(())
}
