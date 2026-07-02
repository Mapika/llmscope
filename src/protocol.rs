use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    OpenAI,
}

impl Provider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
            Provider::OpenAI => "openai",
        }
    }
}

pub struct ReqInfo {
    pub model: Option<String>,
    pub stream: bool,
    /// Conversation identity: hash of the system prompt + the first
    /// non-system message. Stable while an agent's history grows, distinct
    /// across separate runs and side agents. Empty for non-chat requests.
    pub session: String,
}

pub fn parse_request(body: &[u8]) -> ReqInfo {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return ReqInfo {
            model: None,
            stream: false,
            session: String::new(),
        };
    };
    ReqInfo {
        model: v
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string),
        stream: v.get("stream").and_then(Value::as_bool).unwrap_or(false),
        session: session_fingerprint(&v),
    }
}

fn session_fingerprint(v: &Value) -> String {
    let Some(messages) = v.get("messages").and_then(Value::as_array) else {
        return String::new();
    };
    // Hash extracted TEXT, not JSON: clients re-serialize the same message
    // differently between turns (string vs content-parts array, moving
    // cache_control breakpoints), but the words stay the same.
    let mut canon = String::new();
    // Anthropic keeps the system prompt in a separate field; in the OpenAI
    // protocol it is a leading system/developer message.
    if let Some(system) = v.get("system") {
        canonical_text(system, &mut canon);
    }
    let mut anchor = None;
    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        if matches!(role, "system" | "developer") {
            canonical_text(m, &mut canon);
        } else {
            anchor = Some(m);
            break;
        }
    }
    let Some(anchor) = anchor else {
        return String::new();
    };
    canon.push('\0');
    canonical_text(anchor, &mut canon);
    if canon.trim_matches('\0').is_empty() {
        return String::new();
    }
    format!("{:016x}", crate::diff::fnv1a(canon.as_bytes()))
}

/// Collect every human-readable string out of arbitrarily shaped content.
fn canonical_text(v: &Value, out: &mut String) {
    match v {
        Value::String(s) => out.push_str(s),
        Value::Array(items) => items.iter().for_each(|i| canonical_text(i, out)),
        Value::Object(map) => {
            if let Some(Value::String(s)) = map.get("text") {
                out.push_str(s);
            } else if let Some(content) = map.get("content") {
                canonical_text(content, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_fingerprint_survives_reserialization() {
        // Turn 1: content-parts array with a cache breakpoint.
        let turn1 = br#"{"model":"m","messages":[
            {"role":"system","content":"be brief"},
            {"role":"user","content":[{"type":"text","text":"fix the bug","cache_control":{"type":"ephemeral"}}]}]}"#;
        // Turn 2: same message re-serialized as a plain string, more history.
        let turn2 = br#"{"model":"m","messages":[
            {"role":"system","content":"be brief"},
            {"role":"user","content":"fix the bug"},
            {"role":"assistant","content":"on it"}]}"#;
        let other = br#"{"model":"m","messages":[
            {"role":"system","content":"be brief"},
            {"role":"user","content":"different task"}]}"#;

        let s1 = parse_request(turn1).session;
        let s2 = parse_request(turn2).session;
        let s3 = parse_request(other).session;
        assert!(!s1.is_empty());
        assert_eq!(s1, s2);
        assert_ne!(s1, s3);
    }

    #[test]
    fn openai_sse_picks_up_gateway_cost() {
        // OpenRouter appends usage (with an exact cost) to the final chunk.
        let body = concat!(
            "data: {\"model\":\"anthropic/claude-sonnet-5\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":2,\
             \"prompt_tokens_details\":{\"cached_tokens\":8},\"cost\":0.00042}}\n\n",
            "data: [DONE]\n\n",
        );
        let u = parse_response(Provider::OpenAI, true, body.as_bytes());
        assert_eq!(u.reported_cost, Some(0.00042));
        assert_eq!(u.input, 4);
        assert_eq!(u.cache_read, 8);
        assert!(!u.estimated);
    }
}

/// Normalized usage across providers. `input` is billed, non-cached input:
/// Anthropic already reports it that way; for OpenAI we subtract
/// `cached_tokens` from `prompt_tokens` so the two mean the same thing.
#[derive(Clone, Debug, Default)]
pub struct Usage {
    pub model: Option<String>,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub estimated: bool,
    /// Exact USD cost when the gateway reports one (OpenRouter puts it in
    /// `usage.cost` on every response); first-party APIs never send this.
    pub reported_cost: Option<f64>,
}

pub fn parse_response(provider: Provider, is_sse: bool, body: &[u8]) -> Usage {
    if is_sse {
        match provider {
            Provider::Anthropic => anthropic_sse(body),
            Provider::OpenAI => openai_sse(body),
        }
    } else {
        json_usage(provider, body)
    }
}

fn sse_data_events(body: &[u8]) -> impl Iterator<Item = Value> + '_ {
    body.split(|&b| b == b'\n').filter_map(|line| {
        let line = std::str::from_utf8(line).ok()?.trim();
        let data = line.strip_prefix("data:")?.trim();
        if data == "[DONE]" {
            return None;
        }
        serde_json::from_str::<Value>(data).ok()
    })
}

fn anthropic_sse(body: &[u8]) -> Usage {
    let mut u = Usage::default();
    for v in sse_data_events(body) {
        match v.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                let m = &v["message"];
                u.model = m.get("model").and_then(Value::as_str).map(str::to_string);
                let usage = &m["usage"];
                u.input = i64_at(usage, "input_tokens");
                u.cache_read = i64_at(usage, "cache_read_input_tokens");
                u.cache_write = i64_at(usage, "cache_creation_input_tokens");
            }
            Some("message_delta") => {
                // Cumulative — the last one wins.
                u.output = i64_at(&v["usage"], "output_tokens");
            }
            _ => {}
        }
    }
    u
}

fn openai_sse(body: &[u8]) -> Usage {
    let mut u = Usage::default();
    let mut found_usage = false;
    let mut content_chunks: i64 = 0;
    for v in sse_data_events(body) {
        if u.model.is_none() {
            u.model = v.get("model").and_then(Value::as_str).map(str::to_string);
        }
        if v["choices"][0]["delta"]["content"].is_string() {
            content_chunks += 1;
        }
        let usage = &v["usage"];
        if usage.is_object() {
            let cached = i64_at(&usage["prompt_tokens_details"], "cached_tokens");
            u.input = i64_at(usage, "prompt_tokens") - cached;
            u.cache_read = cached;
            u.output = i64_at(usage, "completion_tokens");
            u.reported_cost = usage.get("cost").and_then(Value::as_f64);
            found_usage = true;
        }
    }
    if !found_usage {
        // Client didn't set stream_options.include_usage. One content delta is
        // roughly one token; the caller fills in an input estimate from the
        // request body size.
        u.output = content_chunks;
        u.estimated = true;
    }
    u
}

fn json_usage(provider: Provider, body: &[u8]) -> Usage {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return Usage::default();
    };
    let mut u = Usage {
        model: v.get("model").and_then(Value::as_str).map(str::to_string),
        ..Usage::default()
    };
    let usage = &v["usage"];
    if !usage.is_object() {
        return u;
    }
    match provider {
        Provider::Anthropic => {
            u.input = i64_at(usage, "input_tokens");
            u.output = i64_at(usage, "output_tokens");
            u.cache_read = i64_at(usage, "cache_read_input_tokens");
            u.cache_write = i64_at(usage, "cache_creation_input_tokens");
        }
        Provider::OpenAI => {
            let cached = i64_at(&usage["prompt_tokens_details"], "cached_tokens");
            u.input = i64_at(usage, "prompt_tokens") - cached;
            u.cache_read = cached;
            u.output = i64_at(usage, "completion_tokens");
            u.reported_cost = usage.get("cost").and_then(Value::as_f64);
        }
    }
    u
}

fn i64_at(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}
