use std::io::Read as _;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use goniec::inbox::{self, Inbox};
use goniec::keys::Keys;
use goniec::{binding, daemon, provision, rooms, send};

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
    },
    /// Print the messages in this session's inbox and mark them delivered
    Read,
    /// List the streams this session's bot has joined
    Rooms,
    /// List the humans with their presence, and every bot
    Who,
    /// Give this directory its own bot (default nickname: the directory name)
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
    /// Claude Code hooks: `wait` (asyncRewake on SessionStart and Stop) and
    /// `context` (PostToolBatch). Exits 0 outside a bound session directory.
    Hook {
        /// `wait` or `context`
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
        Command::Send { to, text } => {
            let text = match text.as_deref() {
                None | Some("-") => {
                    let mut text = String::new();
                    std::io::stdin().read_to_string(&mut text)?;
                    text
                }
                Some(text) => text.to_owned(),
            };
            let keys = Keys::load()?;
            let id = send::send(&keys, &binding::session_dir()?, &to, text.trim_end()).await?;
            println!("sent message {id}");
            Ok(())
        }
        Command::Read => {
            let keys = Keys::load()?;
            let id = provision::identity(&keys, &binding::session_dir()?).await?;
            if id.dir.is_none() {
                bail!("read for @guest is not built yet; only a bound session has an inbox");
            }
            let records = Inbox::open(&id.nick)?.take()?;
            if !records.is_empty() {
                print!("{}", inbox::format(&id.nick, &records));
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
        Command::Bind { nick, reuse } => {
            provision::bind(&binding::session_dir()?, nick, reuse).await
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
