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
    /// Canonical system text, kept whole so a miss can be pinned to the
    /// exact character where two turns diverge.
    pub system_text: String,
    pub tools_count: usize,
    pub tools_chars: usize,
    pub tools_fp: u64,
    /// (name, fingerprint) per tool, for naming which definition churned.
    pub tools: Vec<(String, u64)>,
    /// Whether the request sets any `cache_control` breakpoint (Anthropic
    /// explicit caching).
    pub has_cache_control: bool,
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

pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
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
    let (system_chars, system_fp, system_text) = match v.get("system") {
        Some(s) => {
            let raw = serde_json::to_string(s).unwrap_or_default();
            let mut text = String::new();
            crate::protocol::canonical_text(s, &mut text);
            (raw.len(), fnv1a(raw.as_bytes()), text)
        }
        None => (0, 0, String::new()),
    };
    let (tools_count, tools_chars, tools_fp, tools) = match v.get("tools").and_then(Value::as_array)
    {
        Some(ts) => {
            let raw = serde_json::to_string(ts).unwrap_or_default();
            let list = ts
                .iter()
                .map(|t| {
                    let name = t
                        .get("name")
                        .and_then(Value::as_str)
                        .or_else(|| t.pointer("/function/name").and_then(Value::as_str))
                        .unwrap_or("?")
                        .to_string();
                    let raw_t = serde_json::to_string(t).unwrap_or_default();
                    (name, fnv1a(raw_t.as_bytes()))
                })
                .collect();
            (ts.len(), raw.len(), fnv1a(raw.as_bytes()), list)
        }
        None => (0, 0, 0, Vec::new()),
    };

    Some(Convo {
        system_chars,
        system_fp,
        system_text,
        tools_count,
        tools_chars,
        tools_fp,
        tools,
        has_cache_control: body.contains("\"cache_control\""),
        messages,
    })
}

/// Why a follow-up turn missed (or partially missed) the prompt cache.
/// Ordered root-cause-first; multiple causes can apply to one turn.
#[derive(Debug, PartialEq)]
pub enum MissCause {
    /// The request sets no `cache_control` breakpoints at all, so explicit
    /// (Anthropic) caching was never enabled.
    NoCacheControl,
    /// The system prompt is not byte-stable across turns — the classic
    /// culprit is an embedded timestamp re-rendered every request.
    SystemChanged {
        at_char: usize,
        prev_snippet: String,
        curr_snippet: String,
    },
    /// Tool definitions changed between turns.
    ToolsChanged { detail: String },
    /// An earlier message was rewritten (compaction, summarization,
    /// truncation), invalidating everything after it.
    HistoryRewritten { at_msg: usize },
    /// Prefix is byte-stable but the previous turn was long enough ago for
    /// the cache to expire.
    TtlExpired { gap_secs: i64 },
}

const CACHE_TTL_MS: i64 = 5 * 60 * 1000;

/// Diagnose a cache miss: `gap_ms` is the time since the session's previous
/// request. Returns an empty vec when the prefix looks cache-friendly and
/// the miss is provider-side (eviction, cold shard).
pub fn diagnose_miss(
    prev: &Convo,
    curr: &Convo,
    d: &TurnDiff,
    anthropic: bool,
    gap_ms: i64,
) -> Vec<MissCause> {
    let mut causes = Vec::new();
    if anthropic && !curr.has_cache_control {
        causes.push(MissCause::NoCacheControl);
    }
    if d.system_changed {
        let (at_char, prev_snippet, curr_snippet) =
            text_divergence(&prev.system_text, &curr.system_text);
        causes.push(MissCause::SystemChanged {
            at_char,
            prev_snippet,
            curr_snippet,
        });
    }
    if d.tools_changed {
        causes.push(MissCause::ToolsChanged {
            detail: tools_change_detail(prev, curr),
        });
    }
    if !d.dropped.is_empty() {
        causes.push(MissCause::HistoryRewritten { at_msg: d.kept });
    }
    if causes.is_empty() && gap_ms > CACHE_TTL_MS {
        causes.push(MissCause::TtlExpired {
            gap_secs: gap_ms / 1000,
        });
    }
    causes
}

/// First char where two texts diverge, with a short context snippet from
/// each side — enough to spot `time: 09:14:22` vs `time: 09:14:37`.
fn text_divergence(prev: &str, curr: &str) -> (usize, String, String) {
    let mut at = 0;
    let mut pc = prev.chars();
    let mut cc = curr.chars();
    loop {
        match (pc.next(), cc.next()) {
            (Some(a), Some(b)) if a == b => at += 1,
            _ => break,
        }
    }
    (at, snippet_at(prev, at), snippet_at(curr, at))
}

fn snippet_at(s: &str, at: usize) -> String {
    let start = at.saturating_sub(24);
    let window: String = s.chars().skip(start).take(56).collect();
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(&window.split_whitespace().collect::<Vec<_>>().join(" "));
    if s.chars().count() > start + 56 {
        out.push('…');
    }
    out
}

fn tools_change_detail(prev: &Convo, curr: &Convo) -> String {
    use std::collections::HashMap;
    let pm: HashMap<&str, u64> = prev.tools.iter().map(|(n, f)| (n.as_str(), *f)).collect();
    let cm: HashMap<&str, u64> = curr.tools.iter().map(|(n, f)| (n.as_str(), *f)).collect();

    let names = |v: Vec<&str>| {
        if v.len() <= 3 {
            v.join(", ")
        } else {
            format!("{} +{} more", v[..3].join(", "), v.len() - 3)
        }
    };
    let changed: Vec<&str> = curr
        .tools
        .iter()
        .filter(|(n, f)| pm.get(n.as_str()).is_some_and(|pf| pf != f))
        .map(|(n, _)| n.as_str())
        .collect();
    let added: Vec<&str> = curr
        .tools
        .iter()
        .filter(|(n, _)| !pm.contains_key(n.as_str()))
        .map(|(n, _)| n.as_str())
        .collect();
    let removed: Vec<&str> = prev
        .tools
        .iter()
        .filter(|(n, _)| !cm.contains_key(n.as_str()))
        .map(|(n, _)| n.as_str())
        .collect();

    let mut parts = Vec::new();
    if !changed.is_empty() {
        parts.push(format!("changed: {}", names(changed)));
    }
    if !added.is_empty() {
        parts.push(format!("added: {}", names(added)));
    }
    if !removed.is_empty() {
        parts.push(format!("removed: {}", names(removed)));
    }
    if parts.is_empty() {
        // Same names, same definitions — the serialization order moved.
        parts.push("reordered".to_string());
    }
    parts.join(" · ")
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

    fn convo(body: &str) -> Convo {
        parse_convo(Provider::Anthropic, body).unwrap()
    }

    #[test]
    fn timestamp_in_system_prompt_is_pinpointed() {
        let body = |t: &str| {
            format!(
                r#"{{"model":"m","system":"You are helpful. Current time: 09:14:{t}. Be concise.",
                    "messages":[{{"role":"user","content":[{{"type":"text","text":"hi",
                    "cache_control":{{"type":"ephemeral"}}}}]}}]}}"#
            )
        };
        let (prev, curr) = (convo(&body("22")), convo(&body("37")));
        let d = diff(&prev, &curr);
        let causes = diagnose_miss(&prev, &curr, &d, true, 30_000);
        assert_eq!(causes.len(), 1);
        let MissCause::SystemChanged { prev_snippet, curr_snippet, .. } = &causes[0] else {
            panic!("expected SystemChanged, got {causes:?}");
        };
        assert!(prev_snippet.contains("09:14:22"), "{prev_snippet}");
        assert!(curr_snippet.contains("09:14:37"), "{curr_snippet}");
    }

    #[test]
    fn missing_breakpoints_is_the_root_cause() {
        let body = r#"{"model":"m","system":"stable","messages":[{"role":"user","content":"hi"}]}"#;
        let (prev, curr) = (convo(body), convo(body));
        let d = diff(&prev, &curr);
        assert_eq!(
            diagnose_miss(&prev, &curr, &d, true, 30_000),
            vec![MissCause::NoCacheControl],
        );
        // Same request over the OpenAI protocol: caching is automatic, so a
        // stable prefix with a short gap has no client-side explanation.
        assert!(diagnose_miss(&prev, &curr, &d, false, 30_000).is_empty());
    }

    #[test]
    fn idle_gap_diagnosed_as_ttl_expiry() {
        let body = r#"{"model":"m","system":"stable","messages":[{"role":"user",
            "content":[{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}]}]}"#;
        let (prev, curr) = (convo(body), convo(body));
        let d = diff(&prev, &curr);
        let causes = diagnose_miss(&prev, &curr, &d, true, 7 * 60 * 1000);
        assert_eq!(causes, vec![MissCause::TtlExpired { gap_secs: 420 }]);
    }

    #[test]
    fn tool_definition_churn_is_named() {
        let body = |desc: &str| {
            format!(
                r#"{{"model":"m","tools":[{{"name":"bash","description":"{desc}"}},
                    {{"name":"edit","description":"stable"}}],
                    "messages":[{{"role":"user","content":[{{"type":"text","text":"hi",
                    "cache_control":{{"type":"ephemeral"}}}}]}}]}}"#
            )
        };
        let (prev, curr) = (convo(&body("v1")), convo(&body("v2")));
        let d = diff(&prev, &curr);
        let causes = diagnose_miss(&prev, &curr, &d, true, 1_000);
        let MissCause::ToolsChanged { detail } = &causes[0] else {
            panic!("expected ToolsChanged, got {causes:?}");
        };
        assert!(detail.contains("bash"), "{detail}");
        assert!(!detail.contains("edit"), "{detail}");
    }

    #[test]
    fn history_rewrite_points_at_first_divergence() {
        let prev = anthropic_body(
            r#"{"role":"user","content":"a"},{"role":"assistant","content":"b"},{"role":"user","content":"c"}"#,
        );
        let curr = anthropic_body(r#"{"role":"user","content":"summary of a-c"}"#);
        let (p, c) = (convo(&prev), convo(&curr));
        let d = diff(&p, &c);
        let causes = diagnose_miss(&p, &c, &d, false, 1_000);
        assert_eq!(causes, vec![MissCause::HistoryRewritten { at_msg: 0 }]);
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
