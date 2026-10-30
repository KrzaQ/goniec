//! A thin client for the parts of the Zulip REST API that goniec uses.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// The role value for an organization administrator.
pub const ROLE_ADMINISTRATOR: u16 = 200;

/// Zulip sends a heartbeat about once a minute, so a long poll that takes
/// longer than this has lost its connection.
const LONG_POLL_SECS: u64 = 120;

/// The bot type for a generic bot.
const BOT_TYPE_GENERIC: u8 = 1;

pub struct Client {
    http: reqwest::Client,
    site: String,
    email: String,
    api_key: String,
}

#[derive(Deserialize)]
pub struct Me {
    pub user_id: u64,
    pub email: String,
    pub full_name: String,
    pub is_bot: bool,
}

/// A bot as `GET /bots` lists it for its owner.
#[derive(Deserialize)]
pub struct OwnedBot {
    /// The bot's email address.
    pub username: String,
    pub api_key: String,
}

#[derive(Deserialize)]
struct OwnedBots {
    bots: Vec<OwnedBot>,
}

#[derive(Deserialize)]
pub struct CreatedBot {
    pub user_id: u64,
    pub api_key: String,
}

impl Client {
    pub fn new(site: &str, email: &str, api_key: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("goniec/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            site: site.trim_end_matches('/').to_owned(),
            email: email.to_owned(),
            api_key: api_key.to_owned(),
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}/api/v1{path}", self.site))
            .basic_auth(&self.email, Some(&self.api_key))
    }

    /// The user that owns this client's API key.
    pub async fn me(&self) -> Result<Me> {
        call(self.request(reqwest::Method::GET, "/users/me")).await
    }

    /// The active bots that this client's user owns. Needs a human's key.
    pub async fn owned_bots(&self) -> Result<Vec<OwnedBot>> {
        let bots: OwnedBots = call(self.request(reqwest::Method::GET, "/bots")).await?;
        Ok(bots.bots)
    }

    /// Create a generic bot owned by this client's user. Needs a human's key.
    pub async fn create_bot(&self, short_name: &str, full_name: &str) -> Result<CreatedBot> {
        let bot_type = BOT_TYPE_GENERIC.to_string();
        let form = [
            ("short_name", short_name),
            ("full_name", full_name),
            ("bot_type", &bot_type),
        ];
        call(self.request(reqwest::Method::POST, "/bots").form(&form)).await
    }

    /// Change a user's role. Needs an administrator's or owner's key.
    pub async fn set_role(&self, user_id: u64, role: u16) -> Result<()> {
        let form = [("role", role.to_string())];
        let path = format!("/users/{user_id}");
        let _: Value = call(self.request(reqwest::Method::PATCH, &path).form(&form)).await?;
        Ok(())
    }

    /// Subscribe users to a channel, and create the channel if it does not exist.
    pub async fn subscribe(
        &self,
        channel: &str,
        description: &str,
        user_ids: &[u64],
    ) -> Result<()> {
        let subscriptions = serde_json::json!([{ "name": channel, "description": description }]);
        let form = [
            ("subscriptions", subscriptions.to_string()),
            ("principals", serde_json::to_string(user_ids)?),
        ];
        let request = self.request(reqwest::Method::POST, "/users/me/subscriptions");
        let _: Value = call(request.form(&form)).await?;
        Ok(())
    }

    /// The channels this client's user has joined.
    pub async fn subscriptions(&self) -> Result<Vec<Subscription>> {
        let subs: Subscriptions =
            call(self.request(reqwest::Method::GET, "/users/me/subscriptions")).await?;
        Ok(subs.subscriptions)
    }

    /// The presence of every human in the realm, by email. Zulip keeps no
    /// presence for bots.
    pub async fn presence(&self) -> Result<RealmPresence> {
        call(self.request(reqwest::Method::GET, "/realm/presence")).await
    }

    /// Register an event queue for new messages.
    pub async fn register(&self) -> Result<Queue> {
        self.register_for(&["message"]).await
    }

    /// Register an event queue for the given event types, for example
    /// `message` and `reaction`.
    pub async fn register_for(&self, event_types: &[&str]) -> Result<Queue> {
        let types = serde_json::to_string(event_types)?;
        let form = [("event_types", types.as_str()), ("apply_markdown", "false")];
        call(self.request(reqwest::Method::POST, "/register").form(&form)).await
    }

    /// Delete an event queue that is no longer needed.
    pub async fn delete_queue(&self, queue_id: &str) -> Result<()> {
        let query = [("queue_id", queue_id)];
        let request = self
            .request(reqwest::Method::DELETE, "/events")
            .query(&query);
        let _: Value = call(request).await?;
        Ok(())
    }

    /// Add an emoji reaction, by its Zulip name, to a message.
    pub async fn add_reaction(&self, message_id: u64, emoji_name: &str) -> Result<()> {
        let path = format!("/messages/{message_id}/reactions");
        let form = [("emoji_name", emoji_name)];
        let _: Value = call(self.request(reqwest::Method::POST, &path).form(&form)).await?;
        Ok(())
    }

    /// Wait for events after `last_event_id`. Zulip answers within about a
    /// minute even when nothing happens, with a heartbeat event.
    pub async fn events(&self, queue_id: &str, last_event_id: i64) -> Result<Vec<Event>> {
        let last = last_event_id.to_string();
        let query = [("queue_id", queue_id), ("last_event_id", &last)];
        let request = self
            .request(reqwest::Method::GET, "/events")
            .query(&query)
            .timeout(Duration::from_secs(LONG_POLL_SECS));
        let events: Events = call(request).await?;
        Ok(events.events)
    }

    /// Messages that match all `narrow` terms (`(operator, operand)`), from
    /// the first unread one on.
    pub async fn unread_messages(&self, narrow: &[(&str, &str)]) -> Result<Vec<Message>> {
        let terms: Vec<Value> = std::iter::once(("is", "unread"))
            .chain(narrow.iter().copied())
            .map(|(operator, operand)| serde_json::json!({ "operator": operator, "operand": operand }))
            .collect();
        let narrow = Value::Array(terms).to_string();
        let query = [
            ("anchor", "first_unread"),
            ("num_before", "0"),
            ("num_after", "1000"),
            ("apply_markdown", "false"),
            ("narrow", &narrow),
        ];
        let messages: Messages = call(
            self.request(reqwest::Method::GET, "/messages")
                .query(&query),
        )
        .await?;
        Ok(messages.messages)
    }

    /// The newest `count` messages that match `narrow` (JSON narrow terms)
    /// and come before `before`, or up to the newest message without it.
    /// Oldest first.
    pub async fn history(
        &self,
        narrow: &Value,
        before: Option<u64>,
        count: u32,
    ) -> Result<Vec<Message>> {
        let anchor = before.map_or("newest".to_owned(), |id| id.to_string());
        let count = count.to_string();
        let narrow = narrow.to_string();
        let mut query = vec![
            ("anchor", anchor.as_str()),
            ("num_before", count.as_str()),
            ("num_after", "0"),
            ("apply_markdown", "false"),
            ("narrow", narrow.as_str()),
        ];
        if before.is_some() {
            query.push(("include_anchor", "false"));
        }
        let messages: Messages = call(
            self.request(reqwest::Method::GET, "/messages")
                .query(&query),
        )
        .await?;
        Ok(messages.messages)
    }

    /// One message, with its reactions.
    pub async fn message(&self, id: u64) -> Result<Message> {
        let request = self
            .request(reqwest::Method::GET, &format!("/messages/{id}"))
            .query(&[("apply_markdown", "false")]);
        let one: OneMessage = call(request).await?;
        Ok(one.message)
    }

    /// Up to `count` messages that match `narrow` and come after `anchor`,
    /// oldest first.
    pub async fn after(&self, narrow: &Value, anchor: u64, count: u32) -> Result<Vec<Message>> {
        let anchor = anchor.to_string();
        let count = count.to_string();
        let narrow = narrow.to_string();
        let query = [
            ("anchor", anchor.as_str()),
            ("num_before", "0"),
            ("num_after", count.as_str()),
            ("include_anchor", "false"),
            ("apply_markdown", "false"),
            ("narrow", narrow.as_str()),
        ];
        let messages: Messages = call(
            self.request(reqwest::Method::GET, "/messages")
                .query(&query),
        )
        .await?;
        Ok(messages.messages)
    }

    /// Upload a file. Returns its path on the server, `/user_uploads/…`,
    /// which a message links as `[name](path)`.
    pub async fn upload(&self, name: &str, bytes: Vec<u8>) -> Result<String> {
        let part = reqwest::multipart::Part::bytes(bytes).file_name(name.to_owned());
        let form = reqwest::multipart::Form::new().part("file", part);
        let request = self
            .request(reqwest::Method::POST, "/user_uploads")
            .multipart(form);
        let uploaded: Uploaded = call(request).await?;
        Ok(uploaded.url)
    }

    /// Download an uploaded file by its path, `/user_uploads/…`. Zulip
    /// answers with a temporary URL that needs no authentication. Fails
    /// for a file larger than `max_bytes`.
    pub async fn download(&self, path: &str, max_bytes: u64) -> Result<Vec<u8>> {
        let temporary: Uploaded = call(self.request(reqwest::Method::GET, path)).await?;
        let mut response = self
            .http
            .get(format!("{}{}", self.site, temporary.url))
            .send()
            .await
            .context("cannot reach Zulip")?
            .error_for_status()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            bytes.extend_from_slice(&chunk);
            if bytes.len() as u64 > max_bytes {
                anyhow::bail!("larger than {} MB", max_bytes / 1_000_000);
            }
        }
        Ok(bytes)
    }

    /// Mark messages as read for this client's user.
    pub async fn mark_read(&self, ids: &[u64]) -> Result<()> {
        let form = [
            ("messages", serde_json::to_string(ids)?),
            ("op", "add".to_owned()),
            ("flag", "read".to_owned()),
        ];
        let request = self.request(reqwest::Method::POST, "/messages/flags");
        let _: Value = call(request.form(&form)).await?;
        Ok(())
    }

    /// All active users in the realm, bots included.
    pub async fn users(&self) -> Result<Vec<User>> {
        let users: Users = call(self.request(reqwest::Method::GET, "/users")).await?;
        Ok(users.members.into_iter().filter(|u| u.is_active).collect())
    }

    /// Post to a channel topic. Returns the message ID.
    pub async fn send_to_channel(&self, channel: &str, topic: &str, content: &str) -> Result<u64> {
        let form = [
            ("type", "stream"),
            ("to", channel),
            ("topic", topic),
            ("content", content),
        ];
        let sent: Sent = call(self.request(reqwest::Method::POST, "/messages").form(&form)).await?;
        Ok(sent.id)
    }

    /// Send a direct message to one user. Returns the message ID.
    pub async fn send_direct(&self, user_id: u64, content: &str) -> Result<u64> {
        let to = format!("[{user_id}]");
        let form = [("type", "direct"), ("to", &to), ("content", content)];
        let sent: Sent = call(self.request(reqwest::Method::POST, "/messages").form(&form)).await?;
        Ok(sent.id)
    }
}

#[derive(Deserialize)]
pub struct User {
    pub user_id: u64,
    pub email: String,
    pub full_name: String,
    pub is_bot: bool,
    pub is_active: bool,
}

#[derive(Deserialize)]
struct Users {
    members: Vec<User>,
}

#[derive(Deserialize)]
struct Uploaded {
    url: String,
}

#[derive(Deserialize)]
struct Sent {
    id: u64,
}

#[derive(Deserialize)]
pub struct Subscription {
    pub name: String,
    pub description: String,
    pub invite_only: bool,
}

#[derive(Deserialize)]
struct Subscriptions {
    subscriptions: Vec<Subscription>,
}

#[derive(Deserialize)]
pub struct RealmPresence {
    pub presences: HashMap<String, UserPresence>,
    pub server_timestamp: f64,
}

#[derive(Deserialize)]
pub struct UserPresence {
    pub aggregated: Presence,
}

#[derive(Deserialize)]
pub struct Presence {
    /// `active` or `idle`.
    pub status: String,
    pub timestamp: i64,
}

/// Send a request and decode Zulip's `{"result": ..., "msg": ...}` envelope.
async fn call<T: DeserializeOwned>(request: reqwest::RequestBuilder) -> Result<T> {
    let response = request.send().await.context("cannot reach Zulip")?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .with_context(|| format!("Zulip answered {status} without JSON"))?;
    if body["result"] != "success" {
        return Err(ApiError {
            status: status.as_u16(),
            code: body["code"].as_str().unwrap_or("").to_owned(),
            msg: body["msg"].as_str().unwrap_or("no message").to_owned(),
        }
        .into());
    }
    serde_json::from_value(body).context("unexpected answer from Zulip")
}

/// An error answer from Zulip. `code` is Zulip's machine-readable error
/// code, for example `BAD_EVENT_QUEUE_ID`.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub msg: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Zulip answered {}: {}", self.status, self.msg)
    }
}

impl std::error::Error for ApiError {}

/// True when Zulip no longer knows the event queue, so it must be
/// registered again.
pub fn is_bad_queue(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ApiError>()
        .is_some_and(|e| e.code == "BAD_EVENT_QUEUE_ID")
}

#[derive(Deserialize)]
pub struct Queue {
    pub queue_id: String,
    pub last_event_id: i64,
}

#[derive(Deserialize)]
struct Events {
    events: Vec<Event>,
}

/// One event: a message, a reaction, or something else such as a
/// heartbeat. Only the fields of the kinds goniec registers for are read.
#[derive(Deserialize)]
pub struct Event {
    pub id: i64,
    /// `message`, `reaction`, `heartbeat`, …
    #[serde(rename = "type", default)]
    pub kind: String,
    pub message: Option<Message>,
    #[serde(default)]
    pub flags: Vec<String>,
    /// For a reaction: `add` or `remove`.
    pub op: Option<String>,
    /// For a reaction: who reacted. Zulip takes it from the authenticated
    /// account, so nobody can react on another user's behalf.
    pub user_id: Option<u64>,
    /// For a reaction: the message reacted to.
    pub message_id: Option<u64>,
    /// For a reaction: the emoji's Unicode code point, for example `2705`.
    pub emoji_code: Option<String>,
}

#[derive(Deserialize)]
struct Messages {
    messages: Vec<Message>,
}

#[derive(Deserialize)]
pub struct Message {
    pub id: u64,
    pub sender_id: u64,
    pub sender_full_name: String,
    pub timestamp: i64,
    /// `stream` or `private`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The channel name for a channel message, a list of users otherwise.
    pub display_recipient: Value,
    /// The topic. Empty for a direct message.
    #[serde(default)]
    pub subject: String,
    pub content: String,
    /// Set in `GET /messages` answers. In events the flags come with the
    /// event instead.
    #[serde(default)]
    pub flags: Vec<String>,
    /// Set in `GET /messages` answers.
    #[serde(default)]
    pub reactions: Vec<Reaction>,
}

/// A reaction on a message. Zulip takes `user_id` from the authenticated
/// account that reacted.
#[derive(Deserialize)]
pub struct Reaction {
    pub emoji_code: String,
    pub user_id: u64,
}

#[derive(Deserialize)]
struct OneMessage {
    message: Message,
}
