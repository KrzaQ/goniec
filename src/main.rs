use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use goniec::keys::Keys;
use goniec::provision;

/// goniec carries chat messages between the user and AI agent sessions.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Send a message to a stream topic or to a nickname
    Send {
        /// `#stream>topic` or a nickname
        to: String,
        /// The message text
        text: String,
    },
    /// Print the messages in this session's inbox and mark them delivered
    Read,
    /// List the streams this session's bot has joined
    Rooms,
    /// List the nicknames that are online
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Send { .. } => bail!("send is not implemented yet"),
        Command::Read => bail!("read is not implemented yet"),
        Command::Rooms => bail!("rooms is not implemented yet"),
        Command::Who => bail!("who is not implemented yet"),
        Command::Bind { nick, reuse } => {
            provision::bind(&std::env::current_dir()?, nick, reuse).await
        }
        Command::Whoami => {
            let keys = Keys::load()?;
            let id = provision::identity(&keys, &std::env::current_dir()?).await?;
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
    }
}
