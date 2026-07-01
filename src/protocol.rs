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
}

pub fn parse_request(body: &[u8]) -> ReqInfo {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return ReqInfo {
            model: None,
            stream: false,
        };
    };
    ReqInfo {
        model: v
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string),
        stream: v.get("stream").and_then(Value::as_bool).unwrap_or(false),
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
        }
    }
    u
}

fn i64_at(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}
