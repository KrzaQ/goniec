//! `goniec rooms` and `goniec who`.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;

use crate::keys::Keys;
use crate::provision;
use crate::zulip::{Client, Presence, User};

/// Zulip treats a human whose last report is older than this as offline.
const OFFLINE_AFTER_SECS: i64 = 140;

/// The channels this session's bot has joined, one per line.
pub async fn rooms(keys: &Keys, session_dir: &Path) -> Result<String> {
    let id = provision::identity(keys, session_dir).await?;
    let client = Client::new(&keys.site, &id.bot.email, &id.bot.api_key)?;
    let mut subs = client.subscriptions().await?;
    subs.sort_by(|a, b| a.name.cmp(&b.name));
    let mut out = String::new();
    for sub in subs {
        let private = if sub.invite_only { " (private)" } else { "" };
        writeln!(out, "#{}{private}  {}", sub.name, sub.description)?;
    }
    Ok(out)
}

/// How a human's presence reads at `now`.
pub fn describe_presence(presence: Option<&Presence>, now: i64) -> String {
    match presence {
        None => "offline".to_owned(),
        Some(p) if now - p.timestamp > OFFLINE_AFTER_SECS => {
            format!("offline, last seen {} ago", ago(now - p.timestamp))
        }
        Some(p) => p.status.clone(),
    }
}

fn ago(secs: i64) -> String {
    if secs < 120 {
        format!("{secs} s")
    } else if secs < 7200 {
        format!("{} min", secs / 60)
    } else if secs < 172_800 {
        format!("{} h", secs / 3600)
    } else {
        format!("{} days", secs / 86_400)
    }
}

/// The bot's nickname: its email without `-bot@…`.
fn nick(user: &User) -> &str {
    user.email
        .split_once("-bot@")
        .map_or(&user.email, |(nick, _)| nick)
}

/// Every human with their presence, and every bot with where it is bound
/// on this host.
pub async fn who(keys: &Keys, session_dir: &Path) -> Result<String> {
    let id = provision::identity(keys, session_dir).await?;
    let client = Client::new(&keys.site, &id.bot.email, &id.bot.api_key)?;
    let users = client.users().await?;
    let presence = client.presence().await?;
    let now = presence.server_timestamp as i64;

    let mut out = String::new();
    let mut humans: Vec<&User> = users.iter().filter(|u| !u.is_bot).collect();
    humans.sort_by(|a, b| a.full_name.cmp(&b.full_name));
    for user in humans {
        let p = presence.presences.get(&user.email).map(|p| &p.aggregated);
        writeln!(out, "{}  {}", user.full_name, describe_presence(p, now))?;
    }

    let mut bots: Vec<&User> = users.iter().filter(|u| u.is_bot).collect();
    bots.sort_by(|a, b| nick(a).cmp(nick(b)));
    for user in bots {
        let nick = nick(user);
        let here = match keys.bots.get(nick).filter(|b| b.user_id == user.user_id) {
            Some(bot) => match &bot.dir {
                Some(dir) => format!("bound here to {}", dir.display()),
                None => "key on this host".to_owned(),
            },
            None => "not on this host".to_owned(),
        };
        writeln!(out, "@{nick}  {here}")?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_text() {
        let p = |status: &str, timestamp| Presence {
            status: status.into(),
            timestamp,
        };
        assert_eq!(describe_presence(None, 1000), "offline");
        assert_eq!(describe_presence(Some(&p("active", 990)), 1000), "active");
        assert_eq!(describe_presence(Some(&p("idle", 900)), 1000), "idle");
        assert_eq!(
            describe_presence(Some(&p("active", 1000 - 600)), 1000),
            "offline, last seen 10 min ago"
        );
    }
}
