//! A thin client for the parts of the Zulip REST API that goniec uses.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// The role value for an organization administrator.
pub const ROLE_ADMINISTRATOR: u16 = 200;

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
        let msg = body["msg"].as_str().unwrap_or("no message");
        bail!("Zulip answered {status}: {msg}");
    }
    serde_json::from_value(body).context("unexpected answer from Zulip")
}
