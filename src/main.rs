use std::io::Read as _;
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use goniec::inbox::{self, Inbox};
use goniec::keys::Keys;
use goniec::{binding, daemon, provision, rooms, send, threads};

/// goniec carries chat messages between the user and AI agent sessions.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Send a message to a channel topic or to a nickname
    Send {
        /// `#channel>topic`, `user:<id>`, `@nick` or a human's full name
        to: String,
        /// The message text; `-` or nothing reads it from stdin
        text: Option<String>,
        /// Upload a file and link it in the message (repeatable). Only files
        /// under the session directory or the session's own temp directory
        #[arg(long, value_name = "FILE")]
        attach: Vec<PathBuf>,
    },
    /// Print the messages in this session's inbox and mark them delivered,
    /// or, with a thread, that thread's latest messages from Zulip
    Read {
        /// `#channel>topic`, `user:<id>`, `@nick` or a human's full name
        thread: Option<String>,
        /// How many messages of the thread
        #[arg(short = 'n', long, default_value_t = 20, requires = "thread")]
        count: u32,
        /// Only messages before this message ID
        #[arg(long, requires = "thread")]
        before: Option<u64>,
    },
    /// List the streams this session's bot has joined
    Rooms,
    /// List the humans with their presence, and every bot
    Who,
    /// Give the current directory its own bot (default nickname: its name)
    Bind {
        /// The nickname, if not the directory name
        nick: Option<String>,
        /// Take over a bot that exists in Zulip but has no key on this host
        #[arg(long)]
        reuse: bool,
    },
    /// Show which bot this session speaks as, after checking its key with Zulip
    Whoami,
    /// Create @admin, @guest and #ops with the owner's key from rosa
    Provision,
    /// Listen to Zulip as every bot bound on this host and fill their inboxes
    Daemon,
    /// Add goniec's hooks to Claude Code's global settings, or remove them
    InstallHooks {
        /// Remove goniec's hooks instead
        #[arg(long)]
        remove: bool,
    },
    /// List the live conversations of bound sessions on this host
    Sessions,
    /// Claude Code hooks: `wait` (asyncRewake on SessionStart and Stop),
    /// `context` (PostToolBatch), `end` (SessionEnd) and `permission`
    /// (PermissionRequest in headless sessions). Exit 0 outside a bound
    /// session directory.
    Hook {
        /// `wait`, `context`, `end` or `permission`
        event: String,
    },
}

fn main() -> Result<()> {
    // Hooks bypass clap and the async runtime: they must start fast, and a
    // usage error must not exit with 2 (see `hook::run`).
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("hook") {
        std::process::exit(goniec::hook::run(args.get(2).map(String::as_str)));
    }
    let cli = Cli::parse();
    tokio::runtime::Runtime::new()?.block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Hook { .. } => unreachable!("handled before clap"),
        Command::Send { to, text, attach } => {
            let text = match text.as_deref() {
                // With files, a missing text is an empty one: an agent's
                // Bash tool has no stdin to wait for.
                None if !attach.is_empty() => String::new(),
                None | Some("-") => {
                    let mut text = String::new();
                    std::io::stdin().read_to_string(&mut text)?;
                    text
                }
                Some(text) => text.to_owned(),
            };
            let keys = Keys::load()?;
            let id = send::send(
                &keys,
                &binding::session_dir()?,
                &to,
                text.trim_end(),
                &attach,
            )
            .await?;
            println!("sent message {id}");
            if let Some(note) = goniec::headless::own_thread_note(&to) {
                println!("{note}");
            }
            Ok(())
        }
        Command::Read {
            thread: Some(thread),
            count,
            before,
        } => {
            let keys = Keys::load()?;
            let text =
                goniec::history::read(&keys, &binding::session_dir()?, &thread, before, count)
                    .await?;
            print!("{text}");
            Ok(())
        }
        Command::Read { thread: None, .. } => {
            let keys = Keys::load()?;
            let id = provision::identity(&keys, &binding::session_dir()?).await?;
            if id.dir.is_none() {
                bail!("read for @guest is not built yet; only a bound session has an inbox");
            }
            let inbox = Inbox::open(&id.nick)?;
            // Only the threads of this conversation, or threads without an
            // owner. Claude Code names the conversation in the Bash tool.
            let session = std::env::var("CLAUDE_CODE_SESSION_ID")
                .ok()
                .filter(|s| !s.is_empty());
            let table = threads::load(&id.nick)?;
            // In a headless session the trusted user's messages are the
            // responder's prompt, not data to read.
            let headless = goniec::headless::active();
            let records = inbox.take_where(|r| {
                threads::may_take(&table, r, session.as_deref(), !keys.dispatches(&id.nick))
                    && !(headless && goniec::headless::is_instruction(r, keys.trusted_user_id))
            })?;
            if let (Some(session), Some(dir)) = (&session, &id.dir) {
                let owner = threads::Owner {
                    session_id: session.clone(),
                    dir: dir.clone(),
                };
                threads::claim(&id.nick, &records, &owner)?;
            }
            let mut records = records;
            let mut errors = Vec::new();
            if let Some(session) = goniec::uploads::current_session(&binding::session_dir()?) {
                goniec::uploads::assign(&mut records, &session.files);
                let client =
                    goniec::zulip::Client::new(&keys.site, &id.bot.email, &id.bot.api_key)?;
                errors = goniec::uploads::save_all(&client, &records, &session.files).await;
            }
            if !records.is_empty() {
                print!("{}", inbox::format(&id.nick, &records));
            }
            for error in errors {
                println!("{error}");
            }
            Ok(())
        }
        Command::Rooms => {
            let keys = Keys::load()?;
            print!("{}", rooms::rooms(&keys, &binding::session_dir()?).await?);
            Ok(())
        }
        Command::Who => {
            let keys = Keys::load()?;
            print!("{}", rooms::who(&keys, &binding::session_dir()?).await?);
            Ok(())
        }
        // bind acts on a directory, so it takes the current one. The session
        // directory would be the agent's own, whatever it changed into.
        Command::Bind { nick, reuse } => {
            provision::bind(&std::env::current_dir()?, nick, reuse).await
        }
        Command::Whoami => {
            let keys = Keys::load()?;
            let id = provision::identity(&keys, &binding::session_dir()?).await?;
            match id.dir {
                Some(dir) => println!(
                    "@{} (user {}), bound to {}",
                    id.nick,
                    id.bot.user_id,
                    dir.display()
                ),
                None => println!("@{} (user {})", id.nick, id.bot.user_id),
            }
            Ok(())
        }
        Command::Provision => provision::provision().await,
        Command::InstallHooks { remove } => {
            let path = goniec::install::settings_path()?;
            let changed = goniec::install::install_hooks(!remove)?;
            let what = if remove {
                "removed from"
            } else {
                "installed in"
            };
            match changed {
                true => println!("goniec hooks {what} {}", path.display()),
                false => println!("{} already as wanted; not changed", path.display()),
            }
            Ok(())
        }
        Command::Sessions => {
            for e in goniec::sessions::live()? {
                let kind = if e.headless {
                    "headless"
                } else {
                    "interactive"
                };
                println!(
                    "@{}  {}  {kind}  pid {}  {}",
                    e.nick,
                    e.session_id,
                    e.pid,
                    e.dir.display()
                );
            }
            Ok(())
        }
        Command::Daemon => {
            let result = daemon::run().await;
            if let Err(e) = &result
                && e.is::<daemon::AlreadyRunning>()
            {
                eprintln!("Error: {e}");
                std::process::exit(daemon::EXIT_ALREADY_RUNNING);
            }
            result
        }
    }
}
