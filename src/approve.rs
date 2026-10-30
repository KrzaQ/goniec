//! Ask the trusted user in Zulip whether a headless session may use a tool.
//!
//! The session's bot sends the trusted user a direct message, adds ✅ and ❌
//! to it, and waits. Only a reaction by the trusted user on that message, or
//! a typed `yes <code>` / `no <code>` from the trusted user, counts. Zulip
//! takes the reacting or sending user from the authenticated account, so no
//! other user or bot can answer on the trusted user's behalf.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;

use crate::keys::Bot;
use crate::zulip::{Client, Queue};

/// Emoji code points of the two answers, and their Zulip names.
const ALLOW_CODE: &str = "2705";
const DENY_CODE: &str = "274c";
const ALLOW_NAME: &str = "check";
const DENY_NAME: &str = "cross_mark";

/// How long to wait for an answer. The hook's own timeout in the headless
/// session's settings must be longer. `GONIEC_APPROVAL_SECS` overrides it,
/// for tests.
const WAIT: Duration = Duration::from_secs(14 * 60);

/// How much of a tool's input the question shows.
const MAX_INPUT_CHARS: usize = 1500;

/// Where the question goes: the topic where the user is talking to the
/// session, or a direct message to the trusted user.
#[derive(Clone, Debug, PartialEq)]
pub enum Place {
    Topic { channel: String, topic: String },
    Direct,
}

impl Place {
    /// From `GONIEC_REPLY_TO`, which goniec sets when it starts a headless
    /// session: `#channel>topic` for a topic, anything else for a direct
    /// message to the trusted user.
    pub fn from_env() -> Self {
        match std::env::var("GONIEC_REPLY_TO")
            .ok()
            .and_then(|to| crate::send::parse_target(&to).ok())
        {
            Some(crate::send::Target::Channel { channel, topic }) => {
                Place::Topic { channel, topic }
            }
            _ => Place::Direct,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Answer {
    Allow,
    /// Denied, with the reason Claude Code passes on to the model.
    Deny(String),
}

/// A short random code that ties an answer to one request.
pub fn new_code() -> String {
    // RandomState gets fresh random keys in each process.
    let mut hasher = RandomState::new().build_hasher();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    hasher.write_u128(now.map_or(0, |d| d.as_nanos()));
    format!("{:04x}", hasher.finish() & 0xffff)
}

/// How long `wait` is, in words.
fn duration_text(wait: Duration) -> String {
    match wait.as_secs() {
        s if s < 60 => format!("{s} seconds"),
        s if s < 120 => "1 minute".to_owned(),
        s => format!("{} minutes", s.div_ceil(60)),
    }
}

/// The text is only an answer to some approval request: `yes`/`no` and a
/// 4-character code. The daemon does not deliver such a message to the
/// agent.
pub fn is_reply(text: &str) -> bool {
    let words: Vec<&str> = text.split_whitespace().collect();
    match words.as_slice() {
        [_, code] => {
            code.len() == 4
                && code.chars().all(|c| c.is_ascii_hexdigit())
                && parse_reply(text, &code.to_lowercase()).is_some()
        }
        _ => false,
    }
}

/// `yes <code>` or `no <code>`, in any case.
pub fn parse_reply(text: &str, code: &str) -> Option<bool> {
    let words: Vec<String> = text.split_whitespace().map(str::to_lowercase).collect();
    match words.as_slice() {
        [word, c] if c == code => match word.as_str() {
            "yes" | "y" | "allow" => Some(true),
            "no" | "n" | "deny" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// A code fence longer than any run of backticks in `text`.
fn fence(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

/// What the tool wants to do, as Zulip markdown.
pub fn describe(tool_name: &str, input: &Value) -> String {
    let shown = match (
        tool_name,
        input["command"].as_str(),
        input["file_path"].as_str(),
    ) {
        ("Bash", Some(command), _) => command.to_owned(),
        (_, _, Some(path)) => path.to_owned(),
        _ => serde_json::to_string_pretty(input).unwrap_or_default(),
    };
    let mut shown: String = shown.chars().take(MAX_INPUT_CHARS).collect();
    if shown.chars().count() == MAX_INPUT_CHARS {
        shown.push_str("\n…");
    }
    let f = fence(&shown);
    let mut out = format!("{f}\n{shown}\n{f}");
    if let Some(why) = input["description"].as_str() {
        out.push_str(&format!("\n{why}"));
    }
    out
}

/// Ask the trusted user and wait for the answer.
pub async fn ask(
    site: &str,
    nick: &str,
    bot: &Bot,
    trusted_user_id: u64,
    place: &Place,
    tool_name: &str,
    input: &Value,
) -> Result<Answer> {
    let wait = std::env::var("GONIEC_APPROVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(WAIT, Duration::from_secs);
    let client = Client::new(site, &bot.email, &bot.api_key)?;
    // A bot gets the reaction events of a channel only as a subscriber.
    if let Place::Topic { channel, .. } = place {
        client.subscribe(channel, "", &[bot.user_id]).await?;
    }
    // Register before asking, so that even an instant answer is in the queue.
    let queue = client.register_for(&["message", "reaction"]).await?;
    let code = new_code();
    let question = format!(
        "**@{nick}** asks to use **{tool_name}**:\n{}\n\n\
         React ✅ to allow or ❌ to deny, or reply `yes {code}` / `no {code}`. \
         No answer within {} means deny.",
        describe(tool_name, input),
        duration_text(wait),
    );
    let message_id = post(&client, place, trusted_user_id, &question).await?;
    // Without the reactions, the typed answer still works.
    for name in [ALLOW_NAME, DENY_NAME] {
        if let Err(e) = client.add_reaction(message_id, name).await {
            eprintln!("goniec: cannot add the {name} reaction: {e:#}");
        }
    }

    let asked = Asked {
        message_id,
        trusted_user_id,
        place,
        code: &code,
    };
    let answer = wait_for_answer(&client, &queue, &asked, wait).await;
    let _ = client.delete_queue(&queue.queue_id).await;
    let answer = answer?;
    let note = match &answer {
        Answer::Allow => format!("✅ Allowed (request {code})."),
        Answer::Deny(why) => format!("❌ Denied (request {code}): {why}."),
    };
    let _ = post(&client, place, trusted_user_id, &note).await;
    Ok(answer)
}

async fn post(client: &Client, place: &Place, trusted_user_id: u64, text: &str) -> Result<u64> {
    match place {
        Place::Topic { channel, topic } => client.send_to_channel(channel, topic, text).await,
        Place::Direct => client.send_direct(trusted_user_id, text).await,
    }
}

/// The message is a typed answer in the place where the question was asked.
fn in_place(m: &crate::zulip::Message, place: &Place) -> bool {
    match place {
        Place::Topic { channel, topic } => {
            m.kind == "stream"
                && m.display_recipient.as_str() == Some(channel.as_str())
                && &m.subject == topic
        }
        Place::Direct => m.kind == "private",
    }
}

/// The question that was asked, and who may answer it where.
struct Asked<'a> {
    message_id: u64,
    trusted_user_id: u64,
    place: &'a Place,
    code: &'a str,
}

async fn wait_for_answer(
    client: &Client,
    queue: &Queue,
    asked: &Asked<'_>,
    wait: Duration,
) -> Result<Answer> {
    let mut last_event_id = queue.last_event_id;
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let events =
            match tokio::time::timeout_at(deadline, client.events(&queue.queue_id, last_event_id))
                .await
            {
                Ok(events) => events?,
                Err(_) => {
                    return Ok(Answer::Deny(format!(
                        "nobody answered within {}",
                        duration_text(wait)
                    )));
                }
            };
        for event in events {
            last_event_id = last_event_id.max(event.id);
            if event.kind == "reaction"
                && event.op.as_deref() == Some("add")
                && event.user_id == Some(asked.trusted_user_id)
                && event.message_id == Some(asked.message_id)
            {
                match event.emoji_code.as_deref() {
                    Some(ALLOW_CODE) => return Ok(Answer::Allow),
                    Some(DENY_CODE) => {
                        return Ok(Answer::Deny("the user denied it in Zulip".into()));
                    }
                    _ => {}
                }
            }
            if let Some(m) = event.message
                && m.sender_id == asked.trusted_user_id
                && in_place(&m, asked.place)
            {
                match parse_reply(&m.content, asked.code) {
                    Some(true) => return Ok(Answer::Allow),
                    Some(false) => return Ok(Answer::Deny("the user denied it in Zulip".into())),
                    None => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replies() {
        assert_eq!(parse_reply("yes 4f2a", "4f2a"), Some(true));
        assert_eq!(parse_reply("  YES   4F2A ", "4f2a"), Some(true));
        assert_eq!(parse_reply("no 4f2a", "4f2a"), Some(false));
        assert_eq!(parse_reply("yes 0000", "4f2a"), None);
        assert_eq!(parse_reply("yes", "4f2a"), None);
        assert_eq!(parse_reply("yes 4f2a please", "4f2a"), None);
    }

    #[test]
    fn approval_replies() {
        assert!(is_reply("yes b7c0"));
        assert!(is_reply("No B7C0"));
        assert!(!is_reply("yes please"));
        assert!(!is_reply("yes b7c0 and more"));
        assert!(!is_reply("hello b7c0"));
        assert_eq!(duration_text(Duration::from_secs(20)), "20 seconds");
        assert_eq!(duration_text(Duration::from_secs(60)), "1 minute");
        assert_eq!(duration_text(Duration::from_secs(14 * 60)), "14 minutes");
    }

    #[test]
    fn codes_are_short_hex() {
        let code = new_code();
        assert_eq!(code.len(), 4);
        assert!(code.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn descriptions() {
        let bash = describe(
            "Bash",
            &json!({ "command": "make deploy", "description": "Deploy" }),
        );
        assert_eq!(bash, "```\nmake deploy\n```\nDeploy");
        let ticks = describe("Bash", &json!({ "command": "echo ````" }));
        assert!(ticks.starts_with("`````\n"));
        assert_eq!(
            describe("Edit", &json!({ "file_path": "/a/b" })),
            "```\n/a/b\n```"
        );
    }
}
