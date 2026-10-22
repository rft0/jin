use std::time::Duration;

use serde_json::{Value, json};

use crate::config::{Llm, Provider};
use crate::ui;

const ANTHROPIC_VERSION: &str = "2026-10-21";

pub struct Prompt<'a> {
    pub system: &'a str,
    pub user: &'a str,
    pub schema_name: &'a str,
    pub schema: Value,
    pub max_tokens: u32,
}

/// One request, one JSON object back (shaped by `prompt.schema`). `activity`
/// is shown next to the spinner while waiting.
pub fn complete(llm: &Llm, prompt: &Prompt, activity: &str) -> Result<Value, String> {
    let _spinner = ui::Spinner::start(format!("{activity}…"));
    let text = match llm.provider {
        Provider::Anthropic => anthropic(llm, prompt)?,
        Provider::OpenAI | Provider::OpenRouter => openai_compat(llm, prompt)?,
    };
    parse_json(&text)
}

fn post(llm: &Llm, url: &str, headers: &[(&str, &str)], body: &Value) -> Result<Value, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(180))
        .build();
    let mut req = agent.post(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let name = llm.provider.name();
    match req.send_json(body) {
        Ok(resp) => resp
            .into_json::<Value>()
            .map_err(|e| format!("bad response from {name}: {e}")),
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            let msg = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(String::from))
                .unwrap_or(body);
            Err(format!("{name} returned HTTP {code}: {}", msg.trim()))
        }
        Err(e) => Err(format!("request to {name} failed: {e}")),
    }
}

fn anthropic(llm: &Llm, p: &Prompt) -> Result<String, String> {
    let mut output_config = json!({ "format": { "type": "json_schema", "schema": p.schema } });
    if let Some(effort) = &llm.effort {
        output_config["effort"] = json!(effort);
    }
    let body = json!({
        "model": llm.model,
        "max_tokens": p.max_tokens,
        "system": p.system,
        "messages": [{ "role": "user", "content": p.user }],
        "output_config": output_config,
    });
    let url = format!("{}/v1/messages", llm.base_url);
    let headers = [("x-api-key", llm.api_key.as_str()), ("anthropic-version", ANTHROPIC_VERSION)];
    let v = post(llm, &url, &headers, &body)?;

    match v["stop_reason"].as_str() {
        Some("refusal") => {
            let why = v["stop_details"]["explanation"].as_str().unwrap_or("no reason given");
            return Err(format!("the model declined: {why}"));
        }
        Some("max_tokens") => return Err(truncated()),
        _ => {}
    }
    let text: String = v["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect();
    Ok(text)
}

/// OpenAI chat completions; OpenRouter speaks the same protocol.
fn openai_compat(llm: &Llm, p: &Prompt) -> Result<String, String> {
    let mut body = json!({
        "model": llm.model,
        "messages": [
            { "role": "system", "content": p.system },
            { "role": "user", "content": p.user },
        ],
        "response_format": {
            "type": "json_schema",
            "json_schema": { "name": p.schema_name, "strict": true, "schema": p.schema },
        },
    });
    let auth = format!("Bearer {}", llm.api_key);
    let mut headers = vec![("Authorization", auth.as_str())];
    match llm.provider {
        Provider::OpenAI => {
            body["max_completion_tokens"] = json!(p.max_tokens);
            if let Some(effort) = &llm.effort {
                body["reasoning_effort"] = json!(effort);
            }
        }
        _ => {
            body["max_tokens"] = json!(p.max_tokens);
            if let Some(effort) = &llm.effort {
                body["reasoning"] = json!({ "effort": effort });
            }
            headers.push(("X-Title", "jin"));
        }
    }
    let url = format!("{}/chat/completions", llm.base_url);
    let v = post(llm, &url, &headers, &body)?;

    let choice = &v["choices"][0];
    if let Some(refusal) = choice["message"]["refusal"].as_str() {
        return Err(format!("the model declined: {refusal}"));
    }
    if choice["finish_reason"] == "length" {
        return Err(truncated());
    }
    choice["message"]["content"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| format!("empty response from {}", llm.provider.name()))
}

fn truncated() -> String {
    "the response was cut off; try a narrower request or --smart".into()
}

/// Strict parse first; then fall back to the outermost `{...}` for models
/// that wrap JSON in prose or code fences.
fn parse_json(text: &str) -> Result<Value, String> {
    let t = text.trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return Ok(v);
    }
    match (t.find('{'), t.rfind('}')) {
        (Some(a), Some(b)) if a < b => {
            serde_json::from_str(&t[a..=b]).map_err(|e| format!("model returned invalid JSON: {e}"))
        }
        _ => {
            let preview: String = t.chars().take(200).collect();
            Err(format!("model did not return JSON: {preview}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_json() {
        let v = parse_json("Sure:\n```json\n{\"command\": \"ls\"}\n```").unwrap();
        assert_eq!(v["command"], "ls");
    }
}
