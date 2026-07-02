//! Minimal OTLP/HTTP JSON trace export — just the wire shape, no
//! OpenTelemetry SDK. One span per captured request, using gen_ai semantic
//! conventions where they exist and `llmscope.*` attributes where they don't.

use serde_json::{Value, json};

use crate::record::RequestRecord;

/// Trace/span ids derived deterministically from the record: one trace per
/// session, one span per request. Good enough for correlation, and stable
/// across process restarts.
fn ids(rec: &RequestRecord) -> (String, String) {
    let t1 = crate::diff::fnv1a(rec.session.as_bytes());
    let t2 = crate::diff::fnv1a(format!("trace:{}", rec.session).as_bytes());
    let span = crate::diff::fnv1a(format!("{}:{}:{}", rec.session, rec.id, rec.ts_ms).as_bytes());
    (format!("{t1:016x}{t2:016x}"), format!("{span:016x}"))
}

pub fn span_json(rec: &RequestRecord) -> Value {
    let (trace_id, span_id) = ids(rec);
    let start_ns = rec.ts_ms.saturating_mul(1_000_000);
    let end_ns = (rec.ts_ms + rec.duration_ms.max(0)).saturating_mul(1_000_000);
    let attr = |k: &str, v: Value| json!({"key": k, "value": v});
    let s = |v: &str| json!({"stringValue": v});
    let i = |v: i64| json!({"intValue": v.to_string()});

    json!({
        "resourceSpans": [{
            "resource": {"attributes": [attr("service.name", s("llmscope"))]},
            "scopeSpans": [{
                "scope": {"name": "llmscope"},
                "spans": [{
                    "traceId": trace_id,
                    "spanId": span_id,
                    "name": rec.model,
                    "kind": 3, // SPAN_KIND_CLIENT
                    "startTimeUnixNano": start_ns.to_string(),
                    "endTimeUnixNano": end_ns.to_string(),
                    "status": {"code": if rec.status >= 400 || rec.status == 0 { 2 } else { 1 }},
                    "attributes": [
                        attr("gen_ai.system", s(&rec.provider)),
                        attr("gen_ai.response.model", s(&rec.model)),
                        attr("gen_ai.usage.input_tokens", i(rec.input_tokens)),
                        attr("gen_ai.usage.output_tokens", i(rec.output_tokens)),
                        attr("llmscope.cache_read_tokens", i(rec.cache_read_tokens)),
                        attr("llmscope.cache_write_tokens", i(rec.cache_write_tokens)),
                        attr("llmscope.cost_usd", json!({"doubleValue": rec.cost_usd})),
                        attr("llmscope.session", s(&rec.session)),
                        attr("llmscope.ttft_ms", i(rec.ttft_ms)),
                        attr("llmscope.estimated", json!({"boolValue": rec.estimated})),
                        attr("http.response.status_code", i(rec.status)),
                        attr("url.path", s(&rec.path)),
                    ],
                }],
            }],
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(status: i64) -> RequestRecord {
        RequestRecord {
            id: 7,
            ts_ms: 1_700_000_000_000,
            provider: "anthropic".into(),
            model: "claude-sonnet-5".into(),
            path: "/v1/messages".into(),
            status,
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 30,
            cache_write_tokens: 0,
            ttft_ms: 100,
            duration_ms: 900,
            cost_usd: 0.01,
            streamed: true,
            estimated: false,
            session: "s".into(),
        }
    }

    #[test]
    fn span_shape_is_otlp() {
        let v = span_json(&rec(200));
        let span = &v["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(span["traceId"].as_str().unwrap().len(), 32);
        assert_eq!(span["spanId"].as_str().unwrap().len(), 16);
        assert_eq!(span["status"]["code"], 1);
        assert_eq!(span["name"], "claude-sonnet-5");
        // Same session, different request → same trace, different span.
        let mut other = rec(200);
        other.id = 8;
        let v2 = span_json(&other);
        let span2 = &v2["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(span["traceId"], span2["traceId"]);
        assert_ne!(span["spanId"], span2["spanId"]);
    }

    #[test]
    fn errors_map_to_span_error_status() {
        let v = span_json(&rec(429));
        assert_eq!(
            v["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["status"]["code"],
            2
        );
    }
}
