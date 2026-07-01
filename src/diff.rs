use serde_json::Value;

use crate::protocol::Provider;

/// One conversation message, reduced to what the diff needs. `fp` is a
/// fingerprint of the full serialized message, so any rewrite — even a
/// one-character edit deep in a tool result — breaks prefix matching.
#[derive(Clone, Debug)]
pub struct Msg {
    pub role: String,
    pub kind: String,
    pub chars: usize,
    pub preview: String,
    pub fp: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Convo {
    pub system_chars: usize,
    pub system_fp: u64,
    pub tools_count: usize,
    pub tools_chars: usize,
    pub tools_fp: u64,
    pub messages: Vec<Msg>,
}

pub struct TurnDiff {
    pub kept: usize,
    pub kept_chars: usize,
    pub appended: Vec<Msg>,
    pub dropped: Vec<Msg>,
    pub system_changed: bool,
    pub tools_changed: bool,
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Collect human-readable text out of arbitrarily nested content, capped —
/// used only for previews.
fn extract_text(v: &Value, out: &mut String) {
    if out.len() >= 200 {
        return;
    }
    match v {
        Value::String(s) => {
            out.push_str(s);
            out.push(' ');
        }
        Value::Array(items) => {
            for item in items {
                extract_text(item, out);
            }
        }
        Value::Object(map) => {
            if let Some(Value::String(s)) = map.get("text") {
                out.push_str(s);
                out.push(' ');
            } else if let Some(content) = map.get("content") {
                extract_text(content, out);
            }
        }
        _ => {}
    }
}

fn clean_preview(raw: &str) -> String {
    let joined: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut p: String = joined.chars().take(70).collect();
    if joined.chars().count() > 70 {
        p.push('…');
    }
    p
}

fn message_kind(m: &Value) -> String {
    // OpenAI shapes first.
    if m.get("tool_calls").is_some() {
        return "tool_use".to_string();
    }
    if m.get("role").and_then(Value::as_str) == Some("tool") {
        return "tool_result".to_string();
    }
    // Anthropic content blocks.
    if let Some(blocks) = m.get("content").and_then(Value::as_array) {
        let mut kinds: Vec<&str> = Vec::new();
        for b in blocks {
            let t = b.get("type").and_then(Value::as_str).unwrap_or("?");
            if !kinds.contains(&t) {
                kinds.push(t);
            }
        }
        if !kinds.is_empty() {
            return kinds.join("+");
        }
    }
    "text".to_string()
}

pub fn parse_convo(_provider: Provider, body: &str) -> Option<Convo> {
    let v: Value = serde_json::from_str(body).ok()?;
    let raw_msgs = v.get("messages")?.as_array()?;

    let mut messages = Vec::with_capacity(raw_msgs.len());
    for m in raw_msgs {
        let raw = serde_json::to_string(m).unwrap_or_default();
        let mut text = String::new();
        extract_text(m.get("content").unwrap_or(&Value::Null), &mut text);
        if text.trim().is_empty() {
            // Tool-use messages often carry no prose; show the tool name.
            if let Some(name) = m
                .pointer("/tool_calls/0/function/name")
                .or_else(|| {
                    m.get("content")
                        .and_then(Value::as_array)
                        .and_then(|blocks| {
                            blocks.iter().find_map(|b| {
                                (b.get("type").and_then(Value::as_str) == Some("tool_use"))
                                    .then(|| b.get("name"))
                                    .flatten()
                            })
                        })
                })
                .and_then(Value::as_str)
            {
                text = format!("→ {name}");
            }
        }
        messages.push(Msg {
            role: m
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string(),
            kind: message_kind(m),
            chars: raw.len(),
            preview: clean_preview(&text),
            fp: fnv1a(raw.as_bytes()),
        });
    }

    // Anthropic keeps the system prompt in a separate field; in the OpenAI
    // protocol it is messages[0] and diffs as a normal message.
    let (system_chars, system_fp) = match v.get("system") {
        Some(s) => {
            let raw = serde_json::to_string(s).unwrap_or_default();
            (raw.len(), fnv1a(raw.as_bytes()))
        }
        None => (0, 0),
    };
    let (tools_count, tools_chars, tools_fp) = match v.get("tools").and_then(Value::as_array) {
        Some(tools) => {
            let raw = serde_json::to_string(tools).unwrap_or_default();
            (tools.len(), raw.len(), fnv1a(raw.as_bytes()))
        }
        None => (0, 0, 0),
    };

    Some(Convo {
        system_chars,
        system_fp,
        tools_count,
        tools_chars,
        tools_fp,
        messages,
    })
}

pub fn diff(prev: &Convo, curr: &Convo) -> TurnDiff {
    let mut kept = 0;
    while kept < prev.messages.len()
        && kept < curr.messages.len()
        && prev.messages[kept].fp == curr.messages[kept].fp
    {
        kept += 1;
    }
    TurnDiff {
        kept,
        kept_chars: curr.messages[..kept].iter().map(|m| m.chars).sum(),
        appended: curr.messages[kept..].to_vec(),
        dropped: prev.messages[kept..].to_vec(),
        system_changed: prev.system_fp != curr.system_fp,
        tools_changed: prev.tools_fp != curr.tools_fp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anthropic_body(messages: &str) -> String {
        format!(
            r#"{{"model":"claude-sonnet-4-5","system":"be helpful","tools":[{{"name":"bash"}}],"messages":[{messages}]}}"#
        )
    }

    #[test]
    fn append_only_turn() {
        let prev = anthropic_body(r#"{"role":"user","content":"hi"}"#);
        let curr = anthropic_body(
            r#"{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"text","text":"hello"}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}"#,
        );
        let p = parse_convo(Provider::Anthropic, &prev).unwrap();
        let c = parse_convo(Provider::Anthropic, &curr).unwrap();
        let d = diff(&p, &c);
        assert_eq!(d.kept, 1);
        assert_eq!(d.appended.len(), 2);
        assert!(d.dropped.is_empty());
        assert!(!d.system_changed);
        assert!(!d.tools_changed);
        assert_eq!(d.appended[1].kind, "tool_result");
    }

    #[test]
    fn compaction_breaks_prefix() {
        let prev = anthropic_body(
            r#"{"role":"user","content":"a"},{"role":"assistant","content":"b"},{"role":"user","content":"c"}"#,
        );
        let curr = anthropic_body(r#"{"role":"user","content":"summary of a-c"}"#);
        let p = parse_convo(Provider::Anthropic, &prev).unwrap();
        let c = parse_convo(Provider::Anthropic, &curr).unwrap();
        let d = diff(&p, &c);
        assert_eq!(d.kept, 0);
        assert_eq!(d.dropped.len(), 3);
        assert_eq!(d.appended.len(), 1);
    }

    #[test]
    fn openai_tool_call_kinds() {
        let body = r#"{"model":"gpt-4o-mini","messages":[
            {"role":"system","content":"be brief"},
            {"role":"assistant","tool_calls":[{"function":{"name":"search","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"1","content":"result text"}]}"#;
        let c = parse_convo(Provider::OpenAI, body).unwrap();
        assert_eq!(c.messages[1].kind, "tool_use");
        assert_eq!(c.messages[1].preview, "→ search");
        assert_eq!(c.messages[2].kind, "tool_result");
        assert!(c.messages[2].preview.contains("result text"));
    }
}
