//! Bots on pi, the coding agent, with a local model instead of Claude.
//!
//! pi runs in RPC mode: JSON lines on stdin and stdout. It has no permission
//! system of its own, so a `tool_call` extension gates every tool call.
//! Measured on 2026-10-31 with pi 1.4.0: the handler may wait as long as it
//! likes, a handler that throws blocks the call, and pi exits at once if an
//! extension named with `-e` is missing or broken, so the gate cannot be
//! silently absent.
//!
//! goniec gives pi a private config directory, `<state>/pi/agent`. Nothing in
//! the user's `~/.pi` loads, so no other extension runs in a bot.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::inbox;
use crate::keys::Keys;

/// Calls `goniec hook gate` for every tool call. Anything but a clear allow
/// blocks: a failed `goniec`, or an answer that is not JSON, throws, and pi
/// blocks the call when a handler throws.
const GATE_EXTENSION: &str = r#"import { execFile } from "node:child_process";

export default function (pi: any) {
  pi.on("tool_call", async (event: any) => {
    const input = JSON.stringify({ tool_name: event.toolName, tool_input: event.input });
    const answer = await new Promise<string>((resolve, reject) => {
      const child = execFile(
        process.env.GONIEC_BIN!,
        ["hook", "gate"],
        { maxBuffer: 1 << 20 },
        (error, stdout) => (error ? reject(error) : resolve(stdout)),
      );
      child.stdin!.end(input);
    });
    const decision = JSON.parse(answer);
    if (decision.allow === true) return undefined;
    return { block: true, reason: String(decision.reason ?? "Not approved.") };
  });
}
"#;

/// The default base model. Its name carries the variant: `-unc` or `-oblit`
/// for the flavor, `-96k` or `-262k` for the context; 192 has no suffix.
const DEFAULT_MODEL: &str = "qwen3.8-27b";

/// The variants that exist: context label, suffix, tokens, flavors.
const SIZES: [(&str, &str, u64, &[&str]); 3] = [
    ("96", "-96k", 98_304, &["", "unc"]),
    ("192", "", 196_608, &["", "unc", "oblit"]),
    ("262", "-262k", 262_144, &["", "unc", "oblit"]),
];

/// The variant a bot starts with.
const DEFAULT_VARIANT: &str = "192";

fn base(keys: &Keys) -> &str {
    keys.pi_model.as_deref().unwrap_or(DEFAULT_MODEL)
}

/// The `pi` binary. Default: `~/.local/bin/pi`, because a service's PATH may
/// not have it.
pub fn binary(keys: &Keys) -> Result<PathBuf> {
    let home = std::env::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    Ok(keys
        .pi_path
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/bin/pi")))
}

/// The OpenAI-compatible URL of the local model server.
pub fn url(keys: &Keys) -> &str {
    keys.pi_url
        .as_deref()
        .unwrap_or("http://localhost:11434/v1")
}

fn model_id(base: &str, size_suffix: &str, flavor: &str) -> String {
    match flavor {
        "" => format!("{base}{size_suffix}"),
        flavor => format!("{base}-{flavor}{size_suffix}"),
    }
}

/// The model for `!model` arguments, like `96`, `192 unc` or `unc 262`.
pub fn parse_model(keys: &Keys, args: &str) -> Result<String> {
    let mut size = DEFAULT_VARIANT;
    let mut flavor = "";
    for word in args.split_whitespace() {
        let word = word.to_lowercase();
        if let Some((label, ..)) = SIZES.iter().find(|(label, ..)| *label == word) {
            size = label;
        } else if word == "unc" || word == "oblit" {
            flavor = if word == "unc" { "unc" } else { "oblit" };
        } else if word != "normal" {
            bail!("unknown model word `{word}`. {}", usage());
        }
    }
    let (label, suffix, _, flavors) = SIZES
        .iter()
        .find(|(label, ..)| *label == size)
        .expect("size comes from SIZES");
    if !flavors.contains(&flavor) {
        bail!("there is no {flavor} variant at {label}. {}", usage());
    }
    Ok(model_id(base(keys), suffix, flavor))
}

/// What `!model` accepts.
pub fn usage() -> String {
    let sizes: Vec<String> = SIZES
        .iter()
        .map(|(label, _, _, flavors)| {
            let names: Vec<&str> = flavors
                .iter()
                .map(|f| if f.is_empty() { "normal" } else { f })
                .collect();
            format!("`{label}` ({})", names.join(", "))
        })
        .collect();
    format!("Sizes and flavors: {}.", sizes.join("; "))
}

/// The model a thread without a choice runs.
pub fn default_model(keys: &Keys) -> String {
    let (_, suffix, ..) = SIZES
        .iter()
        .find(|(label, ..)| *label == DEFAULT_VARIANT)
        .expect("the default variant exists");
    model_id(base(keys), suffix, "")
}

/// The context window of a model, from its name.
pub fn window(model: &str) -> u64 {
    SIZES
        .iter()
        .filter(|(_, suffix, ..)| !suffix.is_empty())
        .find(|(_, suffix, ..)| model.ends_with(suffix))
        .or_else(|| SIZES.iter().find(|(_, suffix, ..)| suffix.is_empty()))
        .map_or(0, |(_, _, tokens, _)| *tokens)
}

/// The conversation's size after a run: the tokens of the last assistant
/// message of an `agent_end` event.
pub fn context_tokens(event: &Value) -> Option<u64> {
    event["messages"]
        .as_array()?
        .iter()
        .rev()
        .find(|m| m["role"] == "assistant")?["usage"]["totalTokens"]
        .as_u64()
}

/// pi's `models.json`: every variant, on the local server.
fn models_json(keys: &Keys) -> Value {
    let models: Vec<Value> = SIZES
        .iter()
        .flat_map(|(_, suffix, tokens, flavors)| {
            flavors.iter().map(move |flavor| {
                json!({
                    "id": model_id(base(keys), suffix, flavor),
                    "input": ["text", "image"],
                    "contextWindow": tokens,
                    "maxTokens": tokens / 4,
                })
            })
        })
        .collect();
    json!({ "providers": { PROVIDER: {
        "baseUrl": url(keys),
        "api": "openai-completions",
        "apiKey": "local",
        "models": models,
    } } })
}

/// The provider name in `models.json`.
pub const PROVIDER: &str = "local";

/// The files a pi process needs.
pub struct Setup {
    /// `PI_CODING_AGENT_DIR`: goniec's own, not `~/.pi`.
    pub agent_dir: PathBuf,
    pub gate: PathBuf,
}

/// Write pi's config and the gate. Each start rewrites them, so a session
/// cannot leave a changed gate behind for the next one.
pub fn prepare(keys: &Keys) -> Result<Setup> {
    let root = inbox::state_dir()?.join("pi");
    let agent_dir = root.join("agent");
    fs::create_dir_all(&agent_dir)?;
    write_file(&root.join("gate.ts"), GATE_EXTENSION.as_bytes())?;
    write_file(
        &agent_dir.join("models.json"),
        serde_json::to_string_pretty(&models_json(keys))?.as_bytes(),
    )?;
    Ok(Setup {
        gate: root.join("gate.ts"),
        agent_dir,
    })
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// The prompt command, with images in pi's form.
pub fn prompt_line(prompt: &str, images: &[Value]) -> Value {
    let images: Vec<Value> = images
        .iter()
        .map(|block| {
            json!({
                "type": "image",
                "data": block["source"]["data"],
                "mimeType": block["source"]["media_type"],
            })
        })
        .collect();
    if images.is_empty() {
        json!({ "type": "prompt", "message": prompt })
    } else {
        json!({ "type": "prompt", "message": prompt, "images": images })
    }
}

/// The text of the last assistant message of an `agent_end` event, and the
/// error if that message failed.
pub fn answer(event: &Value) -> (String, Option<String>) {
    let messages = event["messages"].as_array().map_or(&[][..], Vec::as_slice);
    let Some(last) = messages.iter().rev().find(|m| m["role"] == "assistant") else {
        return (String::new(), None);
    };
    let text: Vec<&str> = last["content"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter(|c| c["type"] == "text")
        .filter_map(|c| c["text"].as_str())
        .collect();
    let error = (last["stopReason"] == "error").then(|| {
        last["errorMessage"]
            .as_str()
            .unwrap_or("the model answered with an error")
            .to_owned()
    });
    (text.join("\n").trim().to_owned(), error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Keys {
        toml::from_str("site = 'https://z'\nowner_email = 'o@z'\n").unwrap()
    }

    #[test]
    fn models() {
        let k = keys();
        assert_eq!(parse_model(&k, "96").unwrap(), "qwen3.8-27b-96k");
        assert_eq!(parse_model(&k, "192").unwrap(), "qwen3.8-27b");
        assert_eq!(parse_model(&k, "262").unwrap(), "qwen3.8-27b-262k");
        assert_eq!(parse_model(&k, "96 unc").unwrap(), "qwen3.8-27b-unc-96k");
        assert_eq!(parse_model(&k, "UNC 262").unwrap(), "qwen3.8-27b-unc-262k");
        assert_eq!(parse_model(&k, "oblit").unwrap(), "qwen3.8-27b-oblit");
        assert!(parse_model(&k, "96 oblit").is_err());
        assert!(parse_model(&k, "large").is_err());
        assert_eq!(default_model(&k), "qwen3.8-27b");
    }

    #[test]
    fn every_variant_is_in_the_config() {
        let config = models_json(&keys());
        let ids: Vec<&str> = config["providers"][PROVIDER]["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 8);
        assert!(ids.contains(&"qwen3.8-27b-oblit-262k"));
    }

    #[test]
    fn answers() {
        let event = json!({ "messages": [
            { "role": "user", "content": [{ "type": "text", "text": "hi" }] },
            { "role": "assistant", "content": [
                { "type": "thinking", "thinking": "hm" },
                { "type": "text", "text": " done " },
            ] },
        ] });
        assert_eq!(answer(&event), ("done".to_owned(), None));
        let failed = json!({ "messages": [
            { "role": "assistant", "content": [], "stopReason": "error", "errorMessage": "boom" },
        ] });
        assert_eq!(answer(&failed), (String::new(), Some("boom".to_owned())));
    }

    #[test]
    fn context_sizes() {
        assert_eq!(window("qwen3.8-27b-96k"), 98_304);
        assert_eq!(window("qwen3.8-27b-unc-262k"), 262_144);
        assert_eq!(window("qwen3.8-27b"), 196_608);
        let event = json!({ "messages": [
            { "role": "assistant", "usage": { "totalTokens": 10 } },
            { "role": "assistant", "usage": { "totalTokens": 2651 } },
            { "role": "toolResult" },
        ] });
        assert_eq!(context_tokens(&event), Some(2651));
        assert_eq!(context_tokens(&json!({})), None);
    }

    #[test]
    fn images_change_form() {
        let block = json!({ "type": "image", "source": {
            "type": "base64", "media_type": "image/png", "data": "QQ==",
        } });
        let line = prompt_line("look", &[block]);
        assert_eq!(line["images"][0]["mimeType"], "image/png");
        assert_eq!(line["images"][0]["data"], "QQ==");
        assert!(prompt_line("look", &[]).get("images").is_none());
    }
}
