//! `goniec send`: post as this session's bot.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::keys::Keys;
use crate::provision;
use crate::uploads;
use crate::zulip::{Client, User};

#[derive(Debug, PartialEq)]
pub enum Target {
    Channel { channel: String, topic: String },
    User(String),
    UserId(u64),
}

/// `#channel>topic` names a channel topic. `user:<id>` names a user by Zulip
/// user ID, which is unique. `@name` or `name` names a user by nickname or
/// full name.
pub fn parse_target(to: &str) -> Result<Target> {
    if let Some(id) = to.strip_prefix("user:") {
        return match id.trim().parse() {
            Ok(id) => Ok(Target::UserId(id)),
            Err(_) => bail!("bad user ID in {to:?}: use user:<number>"),
        };
    }
    if let Some(rest) = to.strip_prefix('#') {
        match rest.split_once('>') {
            Some((channel, topic)) if !channel.trim().is_empty() && !topic.trim().is_empty() => {
                Ok(Target::Channel {
                    channel: channel.trim().to_owned(),
                    topic: topic.trim().to_owned(),
                })
            }
            _ => bail!("a channel target needs a topic: #channel>topic"),
        }
    } else {
        let name = to.strip_prefix('@').unwrap_or(to).trim();
        if name.is_empty() {
            bail!("no target given");
        }
        Ok(Target::User(name.to_owned()))
    }
}

/// A bot matches by its nickname (`<nick>-bot@…`), a human by full name.
pub fn resolve<'a>(users: &'a [User], name: &str) -> Result<&'a User> {
    let bot_prefix = format!("{name}-bot@");
    let found: Vec<&User> = users
        .iter()
        .filter(|u| {
            if u.is_bot {
                u.email.starts_with(&bot_prefix)
            } else {
                u.full_name.eq_ignore_ascii_case(name)
            }
        })
        .collect();
    match found.as_slice() {
        [user] => Ok(user),
        [] => bail!("no user @{name}"),
        _ => bail!(
            "@{name} is ambiguous: {}",
            found
                .iter()
                .map(|u| u.email.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The first line of every @guest message, so the user can tell guests apart.
pub fn guest_header(host: &str, dir: &Path, home: Option<&Path>) -> String {
    let dir = match home.and_then(|home| dir.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => dir.display().to_string(),
    };
    format!("`{host}:{dir}`")
}

/// Post `text` as this session's bot. Returns the message ID.
pub async fn send(
    keys: &Keys,
    session_dir: &Path,
    to: &str,
    text: &str,
    attach: &[PathBuf],
) -> Result<u64> {
    let target = parse_target(to)?;
    if text.trim().is_empty() && attach.is_empty() {
        bail!("the message is empty");
    }
    // The session directory, and the session's own temp directory, where
    // its scratchpad is.
    let mut roots = vec![session_dir.to_owned()];
    roots.extend(uploads::current_session(session_dir).map(|(_, tmp)| tmp));
    let files = attach
        .iter()
        .map(|f| uploads::check_attachment(f, &roots))
        .collect::<Result<Vec<_>>>()?;
    let id = provision::identity(keys, session_dir).await?;
    let content = match id.dir {
        Some(_) => text.to_owned(),
        None => {
            let host = gethostname::gethostname();
            let home = std::env::home_dir();
            let header = guest_header(&host.to_string_lossy(), session_dir, home.as_deref());
            format!("{header}\n{text}")
        }
    };
    let client = Client::new(&keys.site, &id.bot.email, &id.bot.api_key)?;
    let links = uploads::attach(&client, &files).await?;
    let content = [content.trim_end().to_owned()]
        .into_iter()
        .filter(|c| !c.is_empty())
        .chain(links)
        .collect::<Vec<_>>()
        .join("\n");
    match target {
        Target::Channel { channel, topic } => {
            client.send_to_channel(&channel, &topic, &content).await
        }
        Target::User(name) => {
            let users = client.users().await?;
            let user = resolve(&users, &name)?;
            client.send_direct(user.user_id, &content).await
        }
        Target::UserId(id) => client.send_direct(id, &content).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(
            parse_target("#ops>PHP 8.5.12 rollout").unwrap(),
            Target::Channel {
                channel: "ops".into(),
                topic: "PHP 8.5.12 rollout".into()
            }
        );
        assert_eq!(parse_target("@acme").unwrap(), Target::User("acme".into()));
        assert_eq!(parse_target("acme").unwrap(), Target::User("acme".into()));
        assert_eq!(parse_target("user:8").unwrap(), Target::UserId(8));
        assert!(parse_target("user:x").is_err());
        assert!(parse_target("#ops").is_err());
        assert!(parse_target("#ops>").is_err());
        assert!(parse_target("#>topic").is_err());
        assert!(parse_target("@").is_err());
    }

    fn user(user_id: u64, email: &str, full_name: &str, is_bot: bool) -> User {
        User {
            user_id,
            email: email.into(),
            full_name: full_name.into(),
            is_bot,
            is_active: true,
        }
    }

    #[test]
    fn resolve_users() {
        let users = [
            user(8, "user8@zulip.example", "alice", false),
            user(10, "guest-bot@zulip.example", "guest", true),
            user(11, "acme-bot@zulip.example", "acme", true),
            user(12, "acme-old-bot@zulip.example", "acme", true),
        ];
        assert_eq!(resolve(&users, "acme").unwrap().user_id, 11);
        assert_eq!(resolve(&users, "Alice").unwrap().user_id, 8);
        assert!(resolve(&users, "nobody").is_err());
    }

    #[test]
    fn guest_headers() {
        let home = Path::new("/home/alice");
        let header = guest_header("box", Path::new("/home/alice/code/misc"), Some(home));
        assert_eq!(header, "`box:~/code/misc`");
        assert_eq!(guest_header("box", home, Some(home)), "`box:~`");
        assert_eq!(
            guest_header("h", Path::new("/srv/data"), Some(home)),
            "`h:/srv/data`"
        );
    }
}
