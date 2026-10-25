//! Create bots and channels with the owner's key, and check that a key
//! belongs to the bot it claims to be.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::binding::{self, Binding};
use crate::keys::{Bot, Keys};
use crate::zulip::{Client, OwnedBot, ROLE_ADMINISTRATOR};

/// The name of the owner's key in rosa.
pub const OWNER_KEY_SECRET: &str = "ZULIP_OWNER_API_KEY";

const OPS_CHANNEL: &str = "ops";
const OPS_DESCRIPTION: &str = "Announcements from goniec and the agents";

/// The owner, with the bots the owner has, as Zulip lists them.
pub struct Owner {
    client: Client,
    user_id: u64,
    bots: Vec<OwnedBot>,
}

#[cfg(unix)]
fn owner_key() -> Result<String> {
    crate::rosa::get(OWNER_KEY_SECRET)
}

#[cfg(not(unix))]
fn owner_key() -> Result<String> {
    bail!("rosa runs only on Unix; create bots on the host that runs rosa")
}

impl Owner {
    /// Ask rosa for the owner's key. The user approves it in the rosa terminal.
    pub async fn connect(keys: &Keys) -> Result<Self> {
        eprintln!("goniec: asking rosa for {OWNER_KEY_SECRET}; approve it in the rosa terminal");
        let key = tokio::task::spawn_blocking(owner_key).await??;
        let client = Client::new(&keys.site, &keys.owner_email, &key)?;
        let me = client.me().await.context("the owner's key does not work")?;
        if me.is_bot {
            bail!("{OWNER_KEY_SECRET} belongs to a bot, not to the owner");
        }
        let bots = client.owned_bots().await?;
        Ok(Self {
            client,
            user_id: me.user_id,
            bots,
        })
    }

    fn find(&self, nick: &str) -> Option<&OwnedBot> {
        let prefix = format!("{nick}-bot@");
        self.bots
            .iter()
            .find(|bot| bot.username.starts_with(&prefix))
    }

    /// Make sure that @nick exists and that its key is in `keys`.
    ///
    /// A bot that exists in Zulip but has no key on this host may be held by
    /// another host. Two hosts with one key both get every message, so this
    /// takes such a bot over only when `reuse` is true.
    pub async fn ensure_bot(&mut self, keys: &mut Keys, nick: &str, reuse: bool) -> Result<Bot> {
        if let Some(bot) = keys.bots.get(nick) {
            verify(&keys.site, nick, bot).await?;
            return Ok(bot.clone());
        }
        if self.find(nick).is_some() {
            if !reuse {
                bail!(
                    "@{nick} exists in Zulip, but this host has no key for it. \
                     If another host holds it, both hosts would get every message. \
                     Pass --reuse to take it over anyway."
                );
            }
        } else {
            eprintln!("goniec: creating @{nick}");
            self.client.create_bot(nick, nick).await?;
            self.bots = self.client.owned_bots().await?;
        }
        let owned = self
            .find(nick)
            .ok_or_else(|| anyhow!("Zulip does not list @{nick} after creating it"))?;
        let me = Client::new(&keys.site, &owned.username, &owned.api_key)?
            .me()
            .await?;
        let bot = Bot {
            user_id: me.user_id,
            email: owned.username.clone(),
            api_key: owned.api_key.clone(),
            dir: None,
        };
        keys.bots.insert(nick.to_owned(), bot.clone());
        keys.save()?;
        Ok(bot)
    }
}

/// Check with Zulip that the key belongs to the bot with this user ID.
pub async fn verify(site: &str, nick: &str, bot: &Bot) -> Result<()> {
    let me = Client::new(site, &bot.email, &bot.api_key)?
        .me()
        .await
        .with_context(|| format!("the key for @{nick} does not work"))?;
    if me.user_id != bot.user_id || !me.is_bot {
        bail!(
            "the key for @{nick} belongs to {} (user {}), not to user {}",
            me.full_name,
            me.user_id,
            bot.user_id
        );
    }
    Ok(())
}

/// Create the shared bots and the #ops channel. Running it again is safe.
pub async fn provision() -> Result<()> {
    let mut keys = Keys::load()?;
    let mut owner = Owner::connect(&keys).await?;

    let admin = owner.ensure_bot(&mut keys, "admin", true).await?;
    owner
        .client
        .set_role(admin.user_id, ROLE_ADMINISTRATOR)
        .await?;
    let guest = owner.ensure_bot(&mut keys, "guest", true).await?;

    let members = [owner.user_id, admin.user_id, guest.user_id];
    Client::new(&keys.site, &admin.email, &admin.api_key)?
        .subscribe(OPS_CHANNEL, OPS_DESCRIPTION, &members)
        .await?;
    println!(
        "@admin (user {}), @guest (user {}), #{OPS_CHANNEL}",
        admin.user_id, guest.user_id
    );
    Ok(())
}

/// Give `dir` its own bot and write its `.goniec`.
pub async fn bind(dir: &Path, nick: Option<String>, reuse: bool) -> Result<()> {
    if dir.join(binding::FILE).exists() {
        bail!(
            "{} is already bound; see its {}",
            dir.display(),
            binding::FILE
        );
    }
    let nick = match nick {
        Some(nick) => nick,
        None => dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("{} has no usable name; pass a nickname", dir.display()))?
            .to_owned(),
    };
    binding::check_nick(&nick).context("pass a nickname to `goniec bind`")?;

    let mut keys = Keys::load()?;
    if let Some(other) = keys.bots.get(&nick).and_then(|bot| bot.dir.as_ref())
        && other != dir
        && other.join(binding::FILE).exists()
    {
        bail!("@{nick} is already bound to {}", other.display());
    }

    let mut owner = Owner::connect(&keys).await?;
    let bot = owner.ensure_bot(&mut keys, &nick, reuse).await?;
    if let Some(entry) = keys.bots.get_mut(&nick) {
        entry.dir = Some(dir.to_owned());
    }
    keys.save()?;
    binding::write(
        dir,
        &Binding {
            nick: nick.clone(),
            user_id: bot.user_id,
        },
    )?;
    println!("bound {} as @{nick} (user {})", dir.display(), bot.user_id);
    Ok(())
}

/// Who this session is: the bot bound to the nearest `.goniec`, else @guest.
pub struct Identity {
    pub nick: String,
    pub bot: Bot,
    /// The bound directory, or none for @guest.
    pub dir: Option<PathBuf>,
}

/// Find this session's identity and check it with Zulip.
pub async fn identity(keys: &Keys, cwd: &Path) -> Result<Identity> {
    let (nick, dir) = match binding::find(cwd)? {
        Some((dir, binding)) => {
            let bot = keys.bot(&binding.nick)?;
            if bot.user_id != binding.user_id {
                bail!(
                    "{} says @{} is user {}, but this host's key is for user {}",
                    dir.join(binding::FILE).display(),
                    binding.nick,
                    binding.user_id,
                    bot.user_id
                );
            }
            (binding.nick, Some(dir))
        }
        None => ("guest".to_owned(), None),
    };
    let bot = keys.bot(&nick)?.clone();
    verify(&keys.site, &nick, &bot).await?;
    Ok(Identity { nick, bot, dir })
}
