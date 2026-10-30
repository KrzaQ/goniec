//! Commands from the trusted user: `@**bot** !status` and the like.
//!
//! A message is a command when its text, without the mentions and the
//! surrounding whitespace, starts with `!`. Every bot the message reaches
//! runs it for itself, so `@**a** @**b** !status` answers twice. Only the
//! trusted user's messages count. goniec runs a command itself and answers
//! in the thread; the model never sees it. From anyone else, a message that
//! starts with `!` is ordinary data.

use crate::history;
use crate::inbox::Record;

#[derive(Debug, PartialEq)]
pub enum Command {
    /// Who owns the thread, and in what state.
    Status,
    /// End the thread's headless process after its turn, so the next
    /// message resumes it with the current configuration.
    Restart,
    /// A new headless conversation takes the thread. The text after the
    /// command, if any, is its first prompt.
    Headless(String),
    /// The bot's interactive session takes the thread.
    Interactive,
    Unknown(String),
}

impl Command {
    /// The command word, for the log. `!headless` text is a prompt and
    /// stays out of it.
    pub fn name(&self) -> &str {
        match self {
            Command::Status => "status",
            Command::Restart => "restart",
            Command::Headless(_) => "headless",
            Command::Interactive => "interactive",
            Command::Unknown(word) => word,
        }
    }
}

/// The commands, for the answer to an unknown one.
pub const HELP: &str = "`!status`, `!restart`, `!headless [text]`, `!interactive`";

/// A command, sent by a bot's listener to the responder.
pub struct Order {
    pub nick: String,
    pub record: Record,
    pub command: Command,
}

/// The command in a message's text, if it is one.
pub fn parse(content: &str) -> Option<Command> {
    let (text, _) = history::strip_mentions(content);
    let text = text.trim().strip_prefix('!')?;
    let (word, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    Some(match word.to_lowercase().as_str() {
        "status" => Command::Status,
        "restart" => Command::Restart,
        "headless" => Command::Headless(rest.trim().to_owned()),
        "interactive" => Command::Interactive,
        _ => Command::Unknown(word.to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing() {
        assert_eq!(
            parse("@**series** @**series-pl** !status"),
            Some(Command::Status)
        );
        assert_eq!(parse("  !Restart  "), Some(Command::Restart));
        assert_eq!(
            parse("@**series** !headless what is\nfree?"),
            Some(Command::Headless("what is\nfree?".into()))
        );
        assert_eq!(parse("!headless"), Some(Command::Headless(String::new())));
        assert_eq!(parse("!interactive"), Some(Command::Interactive));
        assert_eq!(parse("!quit"), Some(Command::Unknown("quit".into())));
        assert_eq!(parse("@**series** check this!"), None);
        assert_eq!(parse("hello !status"), None);
        assert_eq!(parse("@**series**"), None);
    }
}
