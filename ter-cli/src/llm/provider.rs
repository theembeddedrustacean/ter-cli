//! The hint model providers a learner can bring a key for, and one call
//! to each. Two wire formats cover them: the chat completions format most
//! providers speak, and Anthropic's messages format.
//!
//! The key goes in one request header and nowhere else. Whatever comes
//! back is cleaned of it before it is shown, in case a provider echoes it.

use std::time::Duration;

use serde_json::{Value, json};

use crate::output::CliError;

/// How long one hint may take to come back.
const TIMEOUT: Duration = Duration::from_secs(180);
/// Room for the answer. Reasoning models spend part of it thinking.
const MAX_TOKENS: u32 = 4000;
/// Provider error text shown to the learner is cut to this.
const ERROR_LIMIT: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// `POST <base>/chat/completions`, bearer key.
    ChatCompletions,
    /// `POST <base>/messages`, `x-api-key`.
    Messages,
}

/// A provider ter knows by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub name: &'static str,
    pub wire: Wire,
    /// `None`: the learner gives one with `--base-url`.
    pub base_url: Option<&'static str>,
    /// `None`: the learner names one with `--model`.
    pub default_model: Option<&'static str>,
    pub needs_key: bool,
    /// Where the learner creates a key.
    pub key_page: Option<&'static str>,
    /// The token limit is `max_completion_tokens` (newer OpenAI models
    /// refuse `max_tokens`).
    pub completion_tokens: bool,
}

pub const PRESETS: &[Preset] = &[
    Preset {
        name: "openai",
        wire: Wire::ChatCompletions,
        base_url: Some("https://api.openai.com/v1"),
        default_model: Some("gpt-5-mini"),
        needs_key: true,
        key_page: Some("https://platform.openai.com/api-keys"),
        completion_tokens: true,
    },
    Preset {
        name: "anthropic",
        wire: Wire::Messages,
        base_url: Some("https://api.anthropic.com/v1"),
        default_model: Some("claude-sonnet-5"),
        needs_key: true,
        key_page: Some("https://console.anthropic.com/settings/keys"),
        completion_tokens: false,
    },
    Preset {
        name: "gemini",
        wire: Wire::ChatCompletions,
        base_url: Some("https://generativelanguage.googleapis.com/v1beta/openai"),
        default_model: Some("gemini-flash-latest"),
        needs_key: true,
        key_page: Some("https://aistudio.google.com/apikey"),
        completion_tokens: false,
    },
    Preset {
        name: "openrouter",
        wire: Wire::ChatCompletions,
        base_url: Some("https://openrouter.ai/api/v1"),
        default_model: Some("openai/gpt-5-mini"),
        needs_key: true,
        key_page: Some("https://openrouter.ai/keys"),
        completion_tokens: false,
    },
    Preset {
        name: "ollama",
        wire: Wire::ChatCompletions,
        base_url: Some("http://localhost:11434/v1"),
        default_model: None,
        needs_key: false,
        key_page: None,
        completion_tokens: false,
    },
    Preset {
        name: "openai-compatible",
        wire: Wire::ChatCompletions,
        base_url: None,
        default_model: None,
        needs_key: false,
        key_page: None,
        completion_tokens: false,
    },
];

pub fn names() -> String {
    PRESETS
        .iter()
        .map(|p| p.name)
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn preset(name: &str) -> Result<&'static Preset, CliError> {
    PRESETS.iter().find(|p| p.name == name).ok_or_else(|| {
        CliError::new(
            "llm_not_configured",
            format!(
                "ter does not know the provider {name:?}. Known: {}.",
                names()
            ),
        )
    })
}

/// Everything one call needs.
pub struct Provider {
    pub preset: &'static Preset,
    pub model: String,
    pub base_url: String,
    pub key: Option<String>,
}

impl Provider {
    /// One question, one answer.
    pub async fn ask(&self, system: &str, prompt: &str) -> Result<String, CliError> {
        let base = self.base_url.trim_end_matches('/');
        let http = reqwest::Client::builder()
            .user_agent(concat!("ter-cli/", env!("CARGO_PKG_VERSION")))
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| self.fail("llm_unavailable", &e.to_string()))?;
        let req = match self.preset.wire {
            Wire::ChatCompletions => {
                let tokens = if self.preset.completion_tokens {
                    "max_completion_tokens"
                } else {
                    "max_tokens"
                };
                let mut body = json!({
                    "model": self.model,
                    "messages": [
                        {"role": "system", "content": system},
                        {"role": "user", "content": prompt},
                    ],
                });
                body[tokens] = MAX_TOKENS.into();
                let req = http.post(format!("{base}/chat/completions")).json(&body);
                match &self.key {
                    Some(key) => req.bearer_auth(key),
                    None => req,
                }
            }
            Wire::Messages => {
                let body = json!({
                    "model": self.model,
                    "max_tokens": MAX_TOKENS,
                    "system": system,
                    "messages": [{"role": "user", "content": prompt}],
                });
                http.post(format!("{base}/messages"))
                    .header("x-api-key", self.key.as_deref().unwrap_or_default())
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
            }
        };
        let resp = req
            .send()
            .await
            .map_err(|e| self.fail("llm_unavailable", &chain(&e)))?;
        let status = resp.status().as_u16();
        let body = resp
            .bytes()
            .await
            .map_err(|e| self.fail("llm_unavailable", &chain(&e)))?;
        let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        if !(200..300).contains(&status) {
            let text = error_text(&json).unwrap_or_else(|| {
                String::from_utf8_lossy(&body[..body.len().min(ERROR_LIMIT)]).into_owned()
            });
            let code = error_code(status, &text);
            return Err(self.fail(code, &format!("HTTP {status}: {text}")));
        }
        answer_text(self.preset.wire, &json)
            .map(|t| self.redact(&t))
            .ok_or_else(|| {
                self.fail(
                    "llm_error",
                    "the answer had no text (the model may have used its whole token budget)",
                )
            })
    }

    fn fail(&self, code: &str, detail: &str) -> CliError {
        let detail = self.redact(detail);
        let detail: String = detail.chars().take(ERROR_LIMIT).collect();
        let detail = detail.trim_end_matches(['.', ' ']);
        let what = format!("{} ({})", self.preset.name, self.model);
        let message = match code {
            "llm_key_invalid" => format!(
                "{what} did not accept your key: {detail}. Store a new one with `ter llm setup --provider {}`.",
                self.preset.name
            ),
            "llm_unavailable" => format!(
                "{what} could not answer: {detail}. Your account may be out of credit or the service busy; try again later."
            ),
            _ => format!("{what} refused the request: {detail}."),
        };
        CliError::new(code, message)
    }

    /// `text` with the key, and anything that looks like part of it, cut
    /// out.
    pub fn redact(&self, text: &str) -> String {
        match &self.key {
            Some(key) => redact(text, key),
            None => text.to_string(),
        }
    }
}

/// Replace `key` in `text`, and any word that carries its first or last
/// eight characters (a provider's masked echo of it), with `[key]`.
pub fn redact(text: &str, key: &str) -> String {
    let key = key.trim();
    if key.len() < 8 {
        return text.to_string();
    }
    let text = text.replace(key, "[key]");
    let head = &key[..8];
    let tail = &key[key.len() - 8..];
    text.split_inclusive(char::is_whitespace)
        .map(|word| {
            let bare = word.trim_end();
            if bare.contains(head) || bare.contains(tail) {
                format!("[key]{}", &word[bare.len()..])
            } else {
                word.to_string()
            }
        })
        .collect()
}

/// Out of credit is the account, not the request, whatever the status
/// (one provider answers it with a 400).
fn error_code(status: u16, text: &str) -> &'static str {
    let t = text.to_ascii_lowercase();
    if ["credit", "quota", "billing", "balance"]
        .iter()
        .any(|w| t.contains(w))
    {
        return "llm_unavailable";
    }
    match status {
        401 | 403 => "llm_key_invalid",
        400 | 404 | 422 => "llm_error",
        _ => "llm_unavailable",
    }
}

fn error_text(json: &Value) -> Option<String> {
    let err = json.get("error")?;
    let text = err
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| err.as_str())?;
    Some(text.trim().to_string())
}

fn answer_text(wire: Wire, json: &Value) -> Option<String> {
    let text = match wire {
        Wire::ChatCompletions => json
            .pointer("/choices/0/message/content")?
            .as_str()?
            .to_string(),
        Wire::Messages => json
            .get("content")?
            .as_array()?
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
    };
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn chain(e: &reqwest::Error) -> String {
    let mut msg = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        msg = format!("{msg}: {s}");
        source = s.source();
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-test-0123456789abcdefghijklmnop";

    #[test]
    fn the_key_and_masked_echoes_of_it_are_cut_out() {
        let echoed = format!(
            "Incorrect API key provided: {}. Also sk-test-01****mnop and ****ghijklmnop.",
            KEY
        );
        let clean = redact(&echoed, KEY);
        assert!(!clean.contains(KEY), "{clean}");
        assert!(!clean.contains("sk-test-01"), "{clean}");
        assert!(!clean.contains("ghijklmnop"), "{clean}");
        assert!(
            clean.starts_with("Incorrect API key provided: [key]"),
            "{clean}"
        );
        assert_eq!(redact("nothing here", KEY), "nothing here");
    }

    #[test]
    fn answers_are_read_from_both_wire_formats() {
        let chat =
            json!({"choices": [{"message": {"role": "assistant", "content": " Look at GPIO3. "}}]});
        assert_eq!(
            answer_text(Wire::ChatCompletions, &chat).as_deref(),
            Some("Look at GPIO3.")
        );
        let messages = json!({"content": [
            {"type": "thinking", "thinking": "..."},
            {"type": "text", "text": "Look at "},
            {"type": "text", "text": "GPIO3."}
        ]});
        assert_eq!(
            answer_text(Wire::Messages, &messages).as_deref(),
            Some("Look at GPIO3.")
        );
        let empty = json!({"choices": [{"message": {"content": null}}]});
        assert_eq!(answer_text(Wire::ChatCompletions, &empty), None);
    }

    #[test]
    fn provider_errors_read_both_shapes() {
        let openai = json!({"error": {"message": "You exceeded your current quota", "type": "insufficient_quota"}});
        let messages = json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}});
        assert_eq!(
            error_text(&openai).as_deref(),
            Some("You exceeded your current quota")
        );
        assert_eq!(error_text(&messages).as_deref(), Some("invalid x-api-key"));
    }

    #[test]
    fn running_out_of_credit_is_unavailable_whatever_the_status() {
        // As two providers answered on 2026-09-27.
        let low = "Your credit balance is too low to access the API. Please go to Plans & Billing to upgrade or purchase credits.";
        assert_eq!(error_code(400, low), "llm_unavailable");
        assert_eq!(
            error_code(429, "You have no credits remaining."),
            "llm_unavailable"
        );
        assert_eq!(error_code(400, "max_tokens: must be positive"), "llm_error");
        assert_eq!(error_code(401, "invalid x-api-key"), "llm_key_invalid");
        assert_eq!(error_code(503, "overloaded"), "llm_unavailable");
    }

    #[test]
    fn every_preset_can_be_found_and_unknown_names_are_refused() {
        for p in PRESETS {
            assert_eq!(preset(p.name).unwrap(), p);
            assert!(p.key_page.is_some() == p.needs_key, "{}", p.name);
        }
        assert_eq!(preset("nope").unwrap_err().code, "llm_not_configured");
    }
}
