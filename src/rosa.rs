//! A client for the sub-rosa agent, which releases a secret only after the
//! user approves it in the rosa terminal.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

/// rosa denies a request after 60 s by default and allows 5 s of grace.
const TIMEOUT: Duration = Duration::from_secs(70);

#[derive(Deserialize)]
struct Answer {
    ok: bool,
    value: Option<String>,
    error: Option<String>,
}

/// The socket path, as `rosa sock` resolves it.
fn socket_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("ROSA_SOCK") {
        return Ok(path.into());
    }
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => std::env::home_dir()
            .ok_or_else(|| anyhow!("no home directory"))?
            .join(".config"),
    };
    Ok(config.join("rosa/rosa.sock"))
}

/// Ask rosa for a secret. This blocks until the user decides.
pub fn get(name: &str) -> Result<String> {
    let path = socket_path()?;
    let mut stream = UnixStream::connect(&path).with_context(|| {
        format!(
            "cannot reach rosa at {}; is `rosa serve` running?",
            path.display()
        )
    })?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    let request = serde_json::json!({ "op": "get", "name": name });
    writeln!(stream, "{request}")?;

    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .context("no answer from rosa")?;
    let answer: Answer = serde_json::from_str(&line).context("unexpected answer from rosa")?;
    match (answer.ok, answer.value) {
        (true, Some(value)) => Ok(value),
        _ => bail!(
            "rosa refused {name}: {}",
            answer.error.as_deref().unwrap_or("no reason given")
        ),
    }
}
