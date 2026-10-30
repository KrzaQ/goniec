//! `goniec daemon`: one per host. It listens to Zulip as every bot bound on
//! this host and fills their inboxes.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use crate::approve;
use crate::binding;
use crate::commands::{self, Order};
use crate::headless;
use crate::history;
use crate::inbox::{self, Inbox};
use crate::keys::{self, Bot, Keys};
use crate::responder::Responder;
use crate::zulip::{self, Client, Message};

/// How often the daemon looks for changes in the key file, and for
/// delivered messages to mark read.
const TICK: Duration = Duration::from_secs(2);

const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// How often the daemon checks every binding again, even if the key file
/// did not change.
const RECHECK: Duration = Duration::from_secs(60);

/// Flags that mean a message mentions the bot.
const MENTION_FLAGS: &[&str] = &[
    "mentioned",
    "stream_wildcard_mentioned",
    "topic_wildcard_mentioned",
    "wildcard_mentioned",
];

/// The exit code when another daemon holds the lock. A service manager can
/// treat it as final (systemd: `RestartPreventExitStatus=3`), so it does not
/// restart a second daemon forever.
pub const EXIT_ALREADY_RUNNING: i32 = 3;

#[derive(Debug)]
pub struct AlreadyRunning;

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("another goniec daemon runs on this host")
    }
}

impl std::error::Error for AlreadyRunning {}

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
        return Err(AlreadyRunning.into());
    }

    let key_file = keys::path()?;
    let mut running: HashMap<String, Running> = HashMap::new();
    let mut seen_mtime: Option<SystemTime> = None;
    let mut last_reload = Instant::now();
    let mut keys: Option<Keys> = None;
    let mut responder = Responder::default();
    let (orders, mut received) = unbounded_channel::<Order>();
    // On SIGTERM the daemon drains: running headless turns finish and post
    // their answers, then it exits. The service manager must let it (with
    // systemd: KillMode=mixed, and a TimeoutStopSec longer than a turn).
    let mut stop = Stop::new()?;
    loop {
        let mtime = fs::metadata(&key_file).and_then(|m| m.modified()).ok();
        // A .goniec can appear or go without the key file changing, so the
        // bindings are also checked again every minute.
        if mtime != seen_mtime || last_reload.elapsed() >= RECHECK {
            seen_mtime = mtime;
            last_reload = Instant::now();
            match Keys::load() {
                Ok(loaded) => {
                    reconcile(&loaded, &mut running, &orders);
                    keys = Some(loaded);
                }
                Err(e) => eprintln!("goniec daemon: {e:#}"),
            }
        }
        if let Some(keys) = &keys {
            while let Ok(order) = received.try_recv() {
                responder.command(keys, order).await;
            }
            responder.tick(keys, &bound_bots(keys)).await;
        }
        tokio::select! {
            _ = stop.wait() => break,
            _ = tokio::time::sleep(TICK) => {}
        }
    }
    let mut last = usize::MAX;
    loop {
        let left = responder.drain().await;
        if left == 0 {
            eprintln!("goniec daemon: stopped");
            return Ok(());
        }
        if left != last {
            eprintln!("goniec daemon: stopping after {left} headless turn(s) finish");
            last = left;
        }
        tokio::time::sleep(TICK).await;
    }
}

/// The service manager's request to stop: SIGTERM on Unix, Ctrl-C
/// elsewhere.
#[cfg(unix)]
struct Stop(tokio::signal::unix::Signal);

#[cfg(unix)]
impl Stop {
    fn new() -> Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self(signal(SignalKind::terminate())?))
    }

    async fn wait(&mut self) {
        self.0.recv().await;
    }
}

#[cfg(not(unix))]
struct Stop;

#[cfg(not(unix))]
impl Stop {
    fn new() -> Result<Self> {
        Ok(Self)
    }

    async fn wait(&mut self) {
        let _ = tokio::signal::ctrl_c().await;
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
fn reconcile(keys: &Keys, running: &mut HashMap<String, Running>, orders: &UnboundedSender<Order>) {
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
        let listen = tokio::spawn(listen(
            keys.site.clone(),
            keys.trusted_user_id,
            nick.clone(),
            bot.clone(),
            orders.clone(),
        ));
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
async fn listen(
    site: String,
    trusted_user_id: Option<u64>,
    nick: String,
    bot: Bot,
    orders: UnboundedSender<Order>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let err = match Listener::new(&site, trusted_user_id, &nick, &bot, orders.clone()).await {
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
    /// The trusted user's typed answers to approval requests are for the
    /// permission hook, not for the agent.
    trusted_user_id: Option<u64>,
    client: Client,
    inbox: Inbox,
    queue_id: String,
    last_event_id: i64,
    /// Message IDs handled since the queue was registered. A message can come
    /// both from the unread fetch and from the queue.
    seen: HashSet<u64>,
    /// Whether each known user is a bot.
    is_bot: HashMap<u64, bool>,
    /// The trusted user's commands go to the responder.
    orders: UnboundedSender<Order>,
}

impl Listener {
    /// Register the queue, then fetch what is unread. In this order no
    /// message can arrive between the two unnoticed.
    async fn new(
        site: &str,
        trusted_user_id: Option<u64>,
        nick: &str,
        bot: &Bot,
        orders: UnboundedSender<Order>,
    ) -> Result<Self> {
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
            trusted_user_id,
            orders,
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
        if direct && Some(m.sender_id) == self.trusted_user_id && approve::is_reply(&m.content) {
            self.client.mark_read(&[m.id]).await?;
            return Ok(());
        }
        if !self.is_bot.contains_key(&m.sender_id) {
            self.refresh_users().await?;
        }
        let bare = !direct && history::is_bare_mention(&m.content);
        let mut record = history::record(m, &self.is_bot, mentioned);
        // The trusted user's command goes to the responder, never to the
        // agent. It is marked read at once, so a restart does not run it
        // again.
        if headless::is_instruction(&record, self.trusted_user_id)
            && let Some(command) = commands::parse(&record.content)
        {
            self.client.mark_read(&[record.id]).await?;
            log(
                &self.nick,
                format!("command !{} in message {}", command.name(), record.id),
            );
            let order = Order {
                nick: self.nick.clone(),
                record,
                command,
            };
            if self.orders.send(order).is_err() {
                log(&self.nick, "the responder is gone; command dropped");
            }
            return Ok(());
        }
        // A bare mention points at the messages before it, which this bot
        // never got: attach a few. Without them the agent still gets the
        // message, and its `thread:` line.
        if bare && let (Some(channel), Some(topic)) = (&record.channel, &record.topic) {
            let narrow = history::narrow(channel, topic);
            match history::fetch(
                &self.client,
                &narrow,
                Some(record.id),
                history::EARLIER,
                &self.is_bot,
            )
            .await
            {
                Ok(earlier) => record.earlier = earlier,
                Err(e) => log(
                    &self.nick,
                    format!("cannot fetch the messages before {}: {e:#}", record.id),
                ),
            }
        }
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
