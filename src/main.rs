use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

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
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Send { .. } => bail!("send is not implemented yet"),
        Command::Read => bail!("read is not implemented yet"),
        Command::Rooms => bail!("rooms is not implemented yet"),
        Command::Who => bail!("who is not implemented yet"),
        Command::Bind { .. } => bail!("bind is not implemented yet"),
    }
}
