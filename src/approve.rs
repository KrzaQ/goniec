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

/// The longest call a question shows. Zulip takes 10,000 characters per
/// message; a longer call is not asked about.
const MAX_SHOWN_CHARS: usize = 8_000;

/// How long to wait for an answer about a call the auto-mode classifier
/// blocked. Shorter than `WAIT`: the approval counts only until
/// `MAX_BLOCKED_AGE`, and the shorter that is, the less a forged grant can
/// replay an approved call.
pub const WAIT_BLOCKED: Duration = Duration::from_secs(5 * 60);

/// How old a question about a blocked call may be when the retry comes.
pub const MAX_BLOCKED_AGE: Duration = Duration::from_secs(6 * 60);

/// Every question has this sentence, and `goniec send` refuses to post it,
/// so a session cannot pass its own message off as a question.
pub const QUESTION_SENTENCE: &str = "React ✅ to allow or ❌ to deny";

/// Where the question goes: the topic where the user is talking to the
/// session, or a direct message to the trusted user.
#[derive(Clone, Debug, PartialEq)]
pub enum Place {
    Topic { channel: String, topic: String },
    Direct,
}

impl Place {
    /// From the file named by `GONIEC_REPLY_TO_FILE`, which the responder
    /// rewrites before each turn of a headless session, else from
    /// `GONIEC_REPLY_TO`: `#channel>topic` for a topic, anything else for a
    /// direct message to the trusted user.
    pub fn from_env() -> Self {
        match crate::headless::current_thread()
            .or_else(|| std::env::var("GONIEC_REPLY_TO").ok())
            .and_then(|to| crate::send::parse_target(&to).ok())
        {
            Some(crate::send::Target::Channel { channel, topic }) => {
                Place::Topic { channel, topic }
            }
            _ => Place::Direct,
        }
    }
}

impl Place {
    /// The place as a reply target: `#channel>topic`, or `direct`.
    pub fn text(&self) -> String {
        match self {
            Place::Topic { channel, topic } => format!("#{channel}>{topic}"),
            Place::Direct => "direct".to_owned(),
        }
    }

    /// The inverse of `text`.
    pub fn parse(text: &str) -> Self {
        match crate::send::parse_target(text) {
            Ok(crate::send::Target::Channel { channel, topic }) => Place::Topic { channel, topic },
            _ => Place::Direct,
        }
    }

    /// The narrow for the place, where the trusted user answers.
    fn narrow(&self, trusted_user_id: u64) -> Value {
        match self {
            Place::Topic { channel, topic } => crate::history::narrow(channel, topic),
            Place::Direct => crate::history::narrow_direct(trusted_user_id),
        }
    }
}

/// The answer, and the question it answers.
pub struct Outcome {
    pub answer: Answer,
    pub message_id: u64,
    pub code: String,
    /// How long the question said it would wait.
    pub wait: Duration,
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

/// The whole call, as Zulip markdown, or nothing when it is too long to
/// show in full. A question never cuts a call short: the user would approve
/// what they see, and the rest could be anything. The Bash `description`
/// is left out, because the model writes it freely and nothing binds it to
/// the command.
pub fn describe(tool_name: &str, input: &Value) -> Option<String> {
    let mut call = input.clone();
    if let Some(map) = call.as_object_mut() {
        map.remove("description");
    }
    // A plain Bash call shows its command; any other field (a timeout, a
    // sandbox switch) shows the whole input.
    let plain_bash = tool_name == "Bash" && call.as_object().is_some_and(|m| m.len() == 1);
    let shown = match call["command"].as_str() {
        Some(command) if plain_bash => command.to_owned(),
        _ => serde_json::to_string_pretty(&call).ok()?,
    };
    if shown.chars().count() > MAX_SHOWN_CHARS {
        return None;
    }
    let f = fence(&shown);
    Some(format!("{f}\n{shown}\n{f}"))
}

/// The classifier's reason on one line, so that it cannot open a block
/// that hides text.
fn one_line(reason: &str) -> String {
    reason
        .chars()
        .map(|c| if c.is_control() || c == '`' { ' ' } else { c })
        .take(300)
        .collect()
}

/// The question, exactly as `ask` posts it and `verify` expects it.
fn question_text(
    nick: &str,
    tool_name: &str,
    shown: &str,
    blocked: Option<&str>,
    code: &str,
    wait: Duration,
) -> String {
    // A call the auto-mode classifier blocked says why.
    let why = blocked.map_or(String::new(), |reason| {
        format!(
            "\nThe auto-mode classifier blocked it: {}",
            one_line(reason)
        )
    });
    format!(
        "**@{nick}** asks to use **{tool_name}**:\n{shown}{why}\n\n\
         {QUESTION_SENTENCE}, or reply `yes {code}` / `no {code}`. \
         No answer within {} means deny.",
        duration_text(wait),
    )
}

/// The call to ask about.
pub struct Request<'a> {
    pub tool_name: &'a str,
    pub input: &'a Value,
    /// Why the auto-mode classifier blocked the call, for a blocked call.
    pub blocked: Option<&'a str>,
}

/// Ask the trusted user and wait for the answer.
pub async fn ask(
    site: &str,
    nick: &str,
    bot: &Bot,
    trusted_user_id: u64,
    place: &Place,
    request: &Request<'_>,
) -> Result<Outcome> {
    let Request {
        tool_name,
        input,
        blocked,
    } = *request;
    let default_wait = if blocked.is_some() {
        WAIT_BLOCKED
    } else {
        WAIT
    };
    let wait = std::env::var("GONIEC_APPROVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(default_wait, Duration::from_secs);
    let client = Client::new(site, &bot.email, &bot.api_key)?;
    let Some(shown) = describe(tool_name, input) else {
        let note = format!(
            "**@{nick}** wanted to use **{tool_name}**, but the call is too long to show \
             here in full, so it stays denied."
        );
        let _ = post(&client, place, trusted_user_id, &note).await;
        return Ok(Outcome {
            answer: Answer::Deny("the call is too long to show in the chat in full".into()),
            message_id: 0,
            code: String::new(),
            wait,
        });
    };
    // A bot gets the reaction events of a channel only as a subscriber.
    if let Place::Topic { channel, .. } = place {
        client.subscribe(channel, "", &[bot.user_id]).await?;
    }
    // Register before asking, so that even an instant answer is in the queue.
    let queue = client.register_for(&["message", "reaction"]).await?;
    let code = new_code();
    let question = question_text(nick, tool_name, &shown, blocked, &code, wait);
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
    Ok(Outcome {
        answer,
        message_id,
        code,
        wait,
    })
}

/// A question to check with Zulip.
pub struct Check<'a> {
    pub nick: &'a str,
    pub bot_user_id: u64,
    pub trusted_user_id: u64,
    pub place: &'a Place,
    pub message_id: u64,
    pub code: &'a str,
    /// With `blocked`, the classifier's reason the question showed.
    pub request: &'a Request<'a>,
    /// How long the question said it would wait.
    pub wait: Duration,
    /// An older question does not count.
    pub max_age: Duration,
}

/// Whether the trusted user approved this call in Zulip: the question was
/// sent by this bot, in the place, not longer ago than `max_age`, and is
/// exactly the question `ask` posts for this call, character for character;
/// and the trusted user reacted ✅ to it without ❌, or replied `yes <code>`
/// there. An exact match, not a search: a session could post a message as
/// its bot that hides the real call (in a spoiler, say) under a harmless
/// one. Zulip authenticates who reacted and who wrote, so a session can
/// fake none of this, not even with its bot's key.
pub async fn verify(client: &Client, check: &Check<'_>) -> Result<bool> {
    let Some(shown) = describe(check.request.tool_name, check.request.input) else {
        return Ok(false);
    };
    let expected = question_text(
        check.nick,
        check.request.tool_name,
        &shown,
        check.request.blocked,
        check.code,
        check.wait,
    );
    let question = client.message(check.message_id).await?;
    let age = chrono::Utc::now().timestamp() - question.timestamp;
    if question.sender_id != check.bot_user_id
        || !in_place(&question, check.place)
        || question.content != expected
        || age < 0
        || age as u64 > check.max_age.as_secs()
    {
        return Ok(false);
    }
    let by_user = |code: &str| {
        question
            .reactions
            .iter()
            .any(|r| r.user_id == check.trusted_user_id && r.emoji_code == code)
    };
    if by_user(DENY_CODE) {
        return Ok(false);
    }
    if by_user(ALLOW_CODE) {
        return Ok(true);
    }
    let narrow = check.place.narrow(check.trusted_user_id);
    for m in client.after(&narrow, check.message_id, 100).await? {
        if m.sender_id == check.trusted_user_id
            && let Some(yes) = parse_reply(&m.content, check.code)
        {
            return Ok(yes);
        }
    }
    Ok(false)
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
    fn descriptions_show_the_whole_call() {
        // The description is the model's own words, and not shown.
        let bash = describe(
            "Bash",
            &json!({ "command": "make deploy", "description": "Deploy" }),
        );
        assert_eq!(bash.unwrap(), "```\nmake deploy\n```");
        let ticks = describe("Bash", &json!({ "command": "echo ````" })).unwrap();
        assert!(ticks.starts_with("`````\n"));
        // Any other Bash field shows the whole input.
        let sandbox = describe(
            "Bash",
            &json!({ "command": "ls", "dangerouslyDisableSandbox": true }),
        )
        .unwrap();
        assert!(sandbox.contains("dangerouslyDisableSandbox"));
        // A write shows what it writes, an edit both strings.
        let write = describe(
            "Write",
            &json!({ "file_path": "/a/b", "content": "secret plan" }),
        );
        assert!(write.unwrap().contains("secret plan"));
        let edit = describe(
            "Edit",
            &json!({ "file_path": "/a/b", "old_string": "x = 1", "new_string": "x = 2" }),
        )
        .unwrap();
        assert!(edit.contains("x = 1") && edit.contains("x = 2"));
        // Too long to show in full: no question at all.
        let long = "a".repeat(MAX_SHOWN_CHARS + 1);
        assert_eq!(describe("Bash", &json!({ "command": long })), None);
    }

    #[test]
    fn questions_are_exact_and_one_reason_line() {
        let shown = describe("Bash", &json!({ "command": "uname -r" })).unwrap();
        let text = question_text(
            "claude",
            "Bash",
            &shown,
            Some("line one\n```spoiler\nhidden"),
            "ae65",
            WAIT_BLOCKED,
        );
        assert_eq!(
            text,
            "**@claude** asks to use **Bash**:\n```\nuname -r\n```\n\
             The auto-mode classifier blocked it: line one    spoiler hidden\n\n\
             React ✅ to allow or ❌ to deny, or reply `yes ae65` / `no ae65`. \
             No answer within 5 minutes means deny."
        );
    }
}
