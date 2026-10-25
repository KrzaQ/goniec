//! `goniec daemon`: one per host. It listens to Zulip as every bot bound on
//! this host and fills their inboxes.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use tokio::task::JoinHandle;

use crate::binding;
use crate::inbox::{self, Inbox, Record};
use crate::keys::{self, Bot, Keys};
use crate::zulip::{self, Client, Message};

/// How often the daemon looks for changes in the key file, and for
/// delivered messages to mark read.
const TICK: Duration = Duration::from_secs(2);

const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Flags that mean a message mentions the bot.
const MENTION_FLAGS: &[&str] = &[
    "mentioned",
    "stream_wildcard_mentioned",
    "topic_wildcard_mentioned",
    "wildcard_mentioned",
];

fn log(nick: &str, msg: impl std::fmt::Display) {
    eprintln!("goniec daemon: @{nick}: {msg}");
}

pub async fn run() -> Result<()> {
    let state = inbox::state_dir()?;
    fs::create_dir_all(&state)?;
    let lock_path = state.join("daemon.lock");
    let lock = fs::File::create(&lock_path)
        .with_context(|| format!("cannot open {}", lock_path.display()))?;
    if lock.try_lock().is_err() {
        bail!("another goniec daemon runs on this host");
    }

    let key_file = keys::path()?;
    let mut running: HashMap<String, Running> = HashMap::new();
    let mut seen_mtime: Option<SystemTime> = None;
    loop {
        let mtime = fs::metadata(&key_file).and_then(|m| m.modified()).ok();
        if mtime != seen_mtime {
            seen_mtime = mtime;
            match Keys::load() {
                Ok(keys) => reconcile(&keys, &mut running),
                Err(e) => eprintln!("goniec daemon: {e:#}"),
            }
        }
        tokio::time::sleep(TICK).await;
    }
}

/// The two tasks that serve one bot.
struct Running {
    bot: Bot,
    tasks: [JoinHandle<()>; 2],
}

impl Drop for Running {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// The bots that are bound to a directory whose `.goniec` matches.
fn bound_bots(keys: &Keys) -> HashMap<String, Bot> {
    keys.bots
        .iter()
        .filter(|(nick, bot)| {
            let Some(dir) = &bot.dir else { return false };
            match binding::read(&dir.join(binding::FILE)) {
                Ok(b) => &b.nick == *nick && b.user_id == bot.user_id,
                Err(_) => false,
            }
        })
        .map(|(nick, bot)| (nick.clone(), bot.clone()))
        .collect()
}

/// Start tasks for new bots and stop the tasks of bots that are gone or
/// have a new key.
fn reconcile(keys: &Keys, running: &mut HashMap<String, Running>) {
    let wanted = bound_bots(keys);
    running.retain(|nick, r| {
        let keep = wanted
            .get(nick)
            .is_some_and(|bot| bot.user_id == r.bot.user_id && bot.api_key == r.bot.api_key);
        if !keep {
            log(nick, "stopped");
        }
        keep
    });
    for (nick, bot) in wanted {
        if running.contains_key(&nick) {
            continue;
        }
        let inbox = match Inbox::open(&nick) {
            Ok(inbox) => inbox,
            Err(e) => {
                log(&nick, format!("{e:#}"));
                continue;
            }
        };
        log(&nick, "started");
        let listen = tokio::spawn(listen(keys.site.clone(), nick.clone(), bot.clone()));
        let mark = tokio::spawn(mark_read(
            keys.site.clone(),
            nick.clone(),
            bot.clone(),
            inbox,
        ));
        running.insert(
            nick,
            Running {
                bot,
                tasks: [listen, mark],
            },
        );
    }
}

/// Listen as one bot, forever. Errors restart the listener after a delay.
async fn listen(site: String, nick: String, bot: Bot) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let err = match Listener::new(&site, &nick, &bot).await {
            Ok(mut listener) => {
                backoff = Duration::from_secs(1);
                listener.run().await
            }
            Err(e) => e,
        };
        if zulip::is_bad_queue(&err) {
            log(
                &nick,
                "Zulip dropped the event queue; registering a new one",
            );
            continue;
        }
        log(
            &nick,
            format!("{err:#}; retrying in {} s", backoff.as_secs()),
        );
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

struct Listener {
    nick: String,
    user_id: u64,
    client: Client,
    inbox: Inbox,
    queue_id: String,
    last_event_id: i64,
    /// Message IDs handled since the queue was registered. A message can come
    /// both from the unread fetch and from the queue.
    seen: HashSet<u64>,
    /// Whether each known user is a bot.
    is_bot: HashMap<u64, bool>,
}

impl Listener {
    /// Register the queue, then fetch what is unread. In this order no
    /// message can arrive between the two unnoticed.
    async fn new(site: &str, nick: &str, bot: &Bot) -> Result<Self> {
        let client = Client::new(site, &bot.email, &bot.api_key)?;
        let me = client.me().await?;
        if me.user_id != bot.user_id {
            bail!(
                "the key belongs to user {}, not to {}",
                me.user_id,
                bot.user_id
            );
        }
        let queue = client.register().await?;
        let mut listener = Self {
            nick: nick.to_owned(),
            user_id: bot.user_id,
            client,
            inbox: Inbox::open(nick)?,
            queue_id: queue.queue_id,
            last_event_id: queue.last_event_id,
            seen: HashSet::new(),
            is_bot: HashMap::new(),
        };
        listener.refresh_users().await?;
        for narrow in [("is", "dm"), ("is", "mentioned")] {
            for message in listener.client.unread_messages(&[narrow]).await? {
                let flags = message.flags.clone();
                listener.handle(message, &flags).await?;
            }
        }
        Ok(listener)
    }

    /// Wait for events until something fails.
    async fn run(&mut self) -> anyhow::Error {
        loop {
            let events = match self.client.events(&self.queue_id, self.last_event_id).await {
                Ok(events) => events,
                Err(e) => return e,
            };
            for event in events {
                self.last_event_id = self.last_event_id.max(event.id);
                if let Some(message) = event.message
                    && let Err(e) = self.handle(message, &event.flags).await
                {
                    return e;
                }
            }
        }
    }

    async fn refresh_users(&mut self) -> Result<()> {
        self.is_bot = self
            .client
            .users()
            .await?
            .into_iter()
            .map(|u| (u.user_id, u.is_bot))
            .collect();
        Ok(())
    }

    /// Put a message in the inbox if it is for this bot: a direct message,
    /// or a channel message that mentions it.
    async fn handle(&mut self, m: Message, flags: &[String]) -> Result<()> {
        if m.sender_id == self.user_id || !self.seen.insert(m.id) {
            return Ok(());
        }
        let direct = m.kind == "private";
        let mentioned = flags.iter().any(|f| MENTION_FLAGS.contains(&f.as_str()));
        if !direct && !mentioned {
            return Ok(());
        }
        if !self.is_bot.contains_key(&m.sender_id) {
            self.refresh_users().await?;
        }
        let record = Record {
            id: m.id,
            timestamp: m.timestamp,
            sender_id: m.sender_id,
            sender_name: m.sender_full_name,
            // An unknown sender is treated as a bot, so it never counts as the user.
            sender_is_bot: self.is_bot.get(&m.sender_id).copied().unwrap_or(true),
            channel: if direct {
                None
            } else {
                m.display_recipient.as_str().map(str::to_owned)
            },
            topic: if direct { None } else { Some(m.subject) },
            mentioned,
            content: m.content,
        };
        if self.inbox.put(&record)? {
            log(&self.nick, format!("message {} in the inbox", record.id));
        }
        Ok(())
    }
}

/// Mark delivered messages read in Zulip, then remove them from the inbox.
async fn mark_read(site: String, nick: String, bot: Bot, inbox: Inbox) {
    let client = match Client::new(&site, &bot.email, &bot.api_key) {
        Ok(client) => client,
        Err(e) => return log(&nick, format!("{e:#}")),
    };
    loop {
        tokio::time::sleep(TICK).await;
        let ids = match inbox.delivered() {
            Ok(ids) if !ids.is_empty() => ids,
            Ok(_) => continue,
            Err(e) => {
                log(&nick, format!("{e:#}"));
                continue;
            }
        };
        let result = match client.mark_read(&ids).await {
            Ok(()) => inbox.forget(&ids),
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => log(&nick, format!("marked {} read", ids.len())),
            Err(e) => log(&nick, format!("cannot mark read: {e:#}")),
        }
    }
}
