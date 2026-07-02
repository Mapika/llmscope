use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;
use futures::StreamExt;
use serde::Deserialize;

use crate::diff;
use crate::protocol::{self, Provider};
use crate::record::{self, RequestRecord};
use crate::store::Store;

pub struct AppState {
    pub client: reqwest::Client,
    pub anthropic_upstream: String,
    pub openai_upstream: String,
    pub otlp_endpoint: Option<String>,
    pub store: Arc<Store>,
}

impl AppState {
    pub fn new(
        anthropic_upstream: String,
        openai_upstream: String,
        otlp_endpoint: Option<String>,
        store: Arc<Store>,
    ) -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
            anthropic_upstream: anthropic_upstream.trim_end_matches('/').to_string(),
            openai_upstream: openai_upstream.trim_end_matches('/').to_string(),
            otlp_endpoint: otlp_endpoint.map(|e| e.trim_end_matches('/').to_string()),
            store,
        }
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/_llmscope/requests", get(list_requests))
        .route("/_llmscope/diff/{id}", get(get_diff))
        .route("/_llmscope/analysis/{id}", get(get_analysis))
        .route(
            "/_llmscope/ui",
            get(|| async { axum::response::Html(include_str!("ui.html")) }),
        )
        .route(
            "/",
            get(|| async { axum::response::Redirect::temporary("/_llmscope/ui") }),
        )
        .fallback(proxy_handler)
        .with_state(state)
}

/// A request (with both bodies) plus the previous request of the same
/// session — everything the TUI needs to render a turn diff or the body
/// viewer. Bodies stay localhost-only.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct DiffPayload {
    pub curr: RequestRecord,
    pub curr_body: String,
    #[serde(default)]
    pub curr_response_body: String,
    pub prev: Option<RequestRecord>,
    pub prev_body: Option<String>,
}

async fn get_diff(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Response {
    let result = (|| -> anyhow::Result<Option<DiffPayload>> {
        let Some((curr, curr_body, curr_response_body)) = st.store.with_body(id, None)? else {
            return Ok(None);
        };
        let prev = st.store.with_body(0, Some(&curr))?;
        let (prev, prev_body) = match prev {
            Some((r, b, _)) => (Some(r), Some(b)),
            None => (None, None),
        };
        Ok(Some(DiffPayload {
            curr,
            curr_body,
            curr_response_body,
            prev,
            prev_body,
        }))
    })();
    match result {
        Ok(Some(p)) => Json(p).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such request").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(serde::Serialize)]
struct AnalysisMsg {
    /// "=" kept · "-" dropped · "+" appended
    change: &'static str,
    #[serde(flatten)]
    msg: diff::Msg,
}

/// The turn analysis the TUI's diff screen computes, as JSON — consumed by
/// the web UI, and curl-able for scripting.
#[derive(serde::Serialize)]
struct Analysis {
    curr: RequestRecord,
    prev: Option<RequestRecord>,
    system_chars: usize,
    system_changed: bool,
    tools_count: usize,
    tools_chars: usize,
    tools_changed: bool,
    kept: usize,
    est_resent_tok: i64,
    /// "first" | "miss" | "partial" | "ok" | "none"
    verdict: &'static str,
    causes: Vec<diff::MissCause>,
    messages: Vec<AnalysisMsg>,
}

async fn get_analysis(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Response {
    let result = (|| -> anyhow::Result<Option<Analysis>> {
        let Some((curr, curr_body, _)) = st.store.with_body(id, None)? else {
            return Ok(None);
        };
        let prev = st.store.with_body(0, Some(&curr))?;
        Ok(Some(analyze(curr, &curr_body, prev)))
    })();
    match result {
        Ok(Some(a)) => Json(a).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such request").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

fn analyze(
    curr: RequestRecord,
    curr_body: &str,
    prev: Option<(RequestRecord, String, String)>,
) -> Analysis {
    let provider = Provider::from_name(&curr.provider);
    let curr_convo = diff::parse_convo(provider, curr_body);
    let (prev_rec, prev_convo) = match prev {
        Some((r, b, _)) => (Some(r), diff::parse_convo(provider, &b)),
        None => (None, None),
    };

    let mut a = Analysis {
        curr,
        prev: prev_rec,
        system_chars: 0,
        system_changed: false,
        tools_count: 0,
        tools_chars: 0,
        tools_changed: false,
        kept: 0,
        est_resent_tok: 0,
        verdict: "none",
        causes: Vec::new(),
        messages: Vec::new(),
    };
    let Some(curr_convo) = curr_convo else {
        return a; // no conversation payload (embeddings, etc.)
    };
    a.system_chars = curr_convo.system_chars;
    a.tools_count = curr_convo.tools_count;
    a.tools_chars = curr_convo.tools_chars;

    let push = |list: &mut Vec<AnalysisMsg>, change, msgs: &[diff::Msg]| {
        list.extend(msgs.iter().map(|m| AnalysisMsg {
            change,
            msg: m.clone(),
        }));
    };
    match prev_convo {
        None => {
            a.verdict = "first";
            push(&mut a.messages, "=", &curr_convo.messages);
        }
        Some(prevc) => {
            let d = diff::diff(&prevc, &curr_convo);
            a.system_changed = d.system_changed;
            a.tools_changed = d.tools_changed;
            a.kept = d.kept;
            a.est_resent_tok =
                ((curr_convo.system_chars + curr_convo.tools_chars + d.kept_chars) / 4) as i64;
            let reported = a.curr.cache_read_tokens;
            let ratio = if a.est_resent_tok > 0 {
                reported as f64 / a.est_resent_tok as f64
            } else {
                0.0
            };
            // Same thresholds as the TUI's economics line.
            let miss = reported == 0 && a.est_resent_tok > 1_000;
            a.verdict = if miss {
                "miss"
            } else if ratio >= 0.7 {
                "ok"
            } else {
                "partial"
            };
            if miss || (reported > 0 && ratio < 0.7) {
                let gap_ms = a.prev.as_ref().map_or(0, |p| a.curr.ts_ms - p.ts_ms);
                a.causes = diff::diagnose_miss(
                    &prevc,
                    &curr_convo,
                    &d,
                    provider == Provider::Anthropic,
                    gap_ms,
                );
            }
            push(&mut a.messages, "=", &curr_convo.messages[..d.kept]);
            push(&mut a.messages, "-", &d.dropped);
            push(&mut a.messages, "+", &d.appended);
        }
    }
    a
}

#[derive(Deserialize)]
struct ListParams {
    since: Option<i64>,
    limit: Option<i64>,
}

async fn list_requests(
    State(st): State<Arc<AppState>>,
    Query(p): Query<ListParams>,
) -> Response {
    match st
        .store
        .recent(p.since.unwrap_or(0), p.limit.unwrap_or(500).min(5000))
    {
        Ok(recs) => Json(recs).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Request headers we must not forward: hop-by-hop, plus accept-encoding so
/// the upstream replies uncompressed and the tee can parse the stream.
fn skip_request_header(name: &header::HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host" | "content-length" | "accept-encoding" | "connection" | "transfer-encoding"
    )
}

fn skip_response_header(name: &header::HeaderName) -> bool {
    matches!(
        name.as_str(),
        "content-length" | "connection" | "transfer-encoding"
    )
}

async fn proxy_handler(State(st): State<Arc<AppState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();

    let (provider, upstream) = if path.strip_prefix("/anthropic/").is_some() {
        (Provider::Anthropic, st.anthropic_upstream.clone())
    } else if path.strip_prefix("/openai/").is_some() {
        (Provider::OpenAI, st.openai_upstream.clone())
    } else {
        return (
            StatusCode::NOT_FOUND,
            "llmscope: unknown route — expected /anthropic/* or /openai/* \
             (set via ANTHROPIC_BASE_URL / OPENAI_BASE_URL)",
        )
            .into_response();
    };
    let rest = &path[1 + provider.as_str().len()..]; // keep leading slash of the remainder

    let query = parts
        .uri
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let url = format!("{upstream}{rest}{query}");

    let body_bytes = match axum::body::to_bytes(body, 256 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("llmscope: body read error: {e}"))
                .into_response();
        }
    };
    let req_info = protocol::parse_request(&body_bytes);

    let mut rb = st.client.request(parts.method.clone(), &url);
    for (name, value) in parts.headers.iter() {
        if !skip_request_header(name) {
            rb = rb.header(name, value);
        }
    }
    if !body_bytes.is_empty() {
        rb = rb.body(body_bytes.clone());
    }

    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let started = Instant::now();

    let upstream_resp = match rb.send().await {
        Ok(r) => r,
        Err(e) => {
            finalize(
                &st,
                FinishCtx {
                    provider,
                    path: rest.to_string(),
                    ts_ms,
                    status: 502,
                    streamed: req_info.stream,
                    req_model: req_info.model,
                    session: req_info.session,
                    req_body: body_bytes,
                    is_sse: false,
                    ttft: None,
                    duration: started.elapsed(),
                    interrupted: false,
                },
                Vec::new(),
            );
            return (
                StatusCode::BAD_GATEWAY,
                format!("llmscope: upstream error for {url}: {e}"),
            )
                .into_response();
        }
    };

    let status = upstream_resp.status();
    let is_sse = upstream_resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("text/event-stream"));

    let mut resp_headers = HeaderMap::new();
    for (name, value) in upstream_resp.headers().iter() {
        if !skip_response_header(name) {
            resp_headers.insert(name.clone(), value.clone());
        }
    }

    let ctx = FinishCtx {
        provider,
        path: rest.to_string(),
        ts_ms,
        status: status.as_u16() as i64,
        streamed: is_sse,
        req_model: req_info.model,
        session: req_info.session,
        req_body: body_bytes,
        is_sse,
        ttft: None,
        duration: Duration::ZERO,
        interrupted: false,
    };

    // Tee the upstream stream: forward every chunk untouched, accumulate a
    // copy, and record usage/timing once the stream ends. `Capture` writes
    // the record on drop, so a client that disconnects mid-stream (an agent
    // user hitting Esc) still gets its billed input recorded.
    let mut cap = Capture {
        st: Arc::clone(&st),
        ctx: Some(ctx),
        buf: Vec::new(),
        started,
        completed: false,
    };
    let mut upstream_stream = upstream_resp.bytes_stream();
    let tee = async_stream::stream! {
        let mut errored = false;
        while let Some(chunk) = upstream_stream.next().await {
            match chunk {
                Ok(bytes) => {
                    if let Some(ctx) = cap.ctx.as_mut()
                        && ctx.ttft.is_none()
                    {
                        ctx.ttft = Some(cap.started.elapsed());
                    }
                    cap.buf.extend_from_slice(&bytes);
                    yield Ok::<Bytes, reqwest::Error>(bytes);
                }
                Err(e) => {
                    errored = true;
                    yield Err(e);
                    break;
                }
            }
        }
        if !errored {
            cap.mark_completed();
        }
        // cap drops here and writes the record.
    };

    let mut response = Response::builder().status(status);
    if let Some(h) = response.headers_mut() {
        *h = resp_headers;
    }
    response
        .body(Body::from_stream(tee))
        .unwrap_or_else(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("llmscope: {e}")).into_response()
        })
}

struct FinishCtx {
    provider: Provider,
    path: String,
    ts_ms: i64,
    status: i64,
    streamed: bool,
    req_model: Option<String>,
    session: String,
    req_body: Bytes,
    is_sse: bool,
    ttft: Option<Duration>,
    duration: Duration,
    /// The stream was cut short (client disconnect or upstream error), so
    /// parsed usage may be missing its final frame.
    interrupted: bool,
}

/// Owns the capture state while a response streams through the tee and
/// writes the record on drop — the one path that runs whether the stream
/// finishes, errors, or is dropped by a disconnecting client.
struct Capture {
    st: Arc<AppState>,
    ctx: Option<FinishCtx>,
    buf: Vec<u8>,
    started: Instant,
    completed: bool,
}

impl Capture {
    /// Mark the stream fully delivered; the record is still written on drop.
    fn mark_completed(&mut self) {
        self.completed = true;
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let Some(mut ctx) = self.ctx.take() else {
            return;
        };
        ctx.duration = self.started.elapsed();
        ctx.interrupted = !self.completed;
        finalize(&self.st, ctx, std::mem::take(&mut self.buf));
    }
}

fn finalize(st: &AppState, ctx: FinishCtx, resp_body: Vec<u8>) {
    let mut usage = protocol::parse_response(ctx.provider, ctx.is_sse, &resp_body);
    if usage.estimated && usage.input == 0 {
        // ~4 chars per token over the raw request JSON. Crude, but better
        // than a zero for clients that never ask for stream usage.
        usage.input = (ctx.req_body.len() / 4) as i64;
    }
    let model = usage
        .model
        .clone()
        .or(ctx.req_model)
        .unwrap_or_else(|| "unknown".to_string());
    let cost = record::cost_usd(&model, &usage);

    let rec = RequestRecord {
        id: 0,
        ts_ms: ctx.ts_ms,
        provider: ctx.provider.as_str().to_string(),
        model,
        path: ctx.path,
        status: ctx.status,
        input_tokens: usage.input,
        output_tokens: usage.output,
        cache_read_tokens: usage.cache_read,
        cache_write_tokens: usage.cache_write,
        ttft_ms: ctx.ttft.map(|d| d.as_millis() as i64).unwrap_or(-1),
        duration_ms: ctx.duration.as_millis() as i64,
        cost_usd: cost,
        streamed: ctx.streamed,
        estimated: usage.estimated || ctx.interrupted,
        session: ctx.session,
    };

    // OTLP export off the response path, fire-and-forget; failures are
    // logged once, not per request.
    if let Some(endpoint) = &st.otlp_endpoint
        && let Ok(handle) = tokio::runtime::Handle::try_current()
    {
        let url = format!("{endpoint}/v1/traces");
        let body = crate::otlp::span_json(&rec);
        let client = st.client.clone();
        handle.spawn(async move {
            use std::sync::atomic::{AtomicBool, Ordering};
            static WARNED: AtomicBool = AtomicBool::new(false);
            let sent = client.post(&url).json(&body).send().await;
            if let Err(e) = sent.and_then(|r| r.error_for_status())
                && !WARNED.swap(true, Ordering::Relaxed)
            {
                eprintln!("llmscope: OTLP export to {url} failed (further errors muted): {e}");
            }
        });
    }

    let store = Arc::clone(&st.store);
    let req_body = String::from_utf8_lossy(&ctx.req_body).into_owned();
    let resp_body = String::from_utf8_lossy(&resp_body).into_owned();
    // SQLite insert off the response path; errors are logged, never fatal.
    tokio::task::spawn_blocking(move || {
        if let Err(e) = store.insert(&rec, &req_body, &resp_body) {
            eprintln!("llmscope: failed to persist record: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dropping the tee mid-stream (client hit Esc) must still persist the
    /// record — the input tokens were billed regardless.
    #[tokio::test]
    async fn dropped_capture_still_records() {
        let db = std::env::temp_dir().join(format!("llmscope-drop-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db);
        let store = Arc::new(Store::open(&db).unwrap());
        let st = Arc::new(AppState::new(
            "http://unused".into(),
            "http://unused".into(),
            None,
            Arc::clone(&store),
        ));

        let cap = Capture {
            st,
            ctx: Some(FinishCtx {
                provider: Provider::Anthropic,
                path: "/v1/messages".into(),
                ts_ms: 1,
                status: 200,
                streamed: true,
                req_model: Some("claude-sonnet-5".into()),
                session: "s1".into(),
                req_body: Bytes::from_static(b"{}"),
                is_sse: true,
                ttft: Some(Duration::from_millis(200)),
                duration: Duration::ZERO,
                interrupted: false,
            }),
            // A truncated Anthropic stream: usage arrived in message_start,
            // two content deltas, but no final message_delta frame.
            buf: concat!(
                "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-5\",\
                 \"usage\":{\"input_tokens\":7,\"cache_read_input_tokens\":9000}}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hel\"}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"lo\"}}\n\n",
            )
            .as_bytes()
            .to_vec(),
            started: Instant::now(),
            completed: false,
        };
        drop(cap);

        // The insert happens on a blocking task; poll briefly.
        for _ in 0..100 {
            if let Ok(recs) = store.recent(0, 10)
                && let Some(r) = recs.first()
            {
                assert!(r.estimated, "interrupted record must be marked estimated");
                assert_eq!(r.input_tokens, 7);
                assert_eq!(r.cache_read_tokens, 9000);
                assert_eq!(r.output_tokens, 2, "output estimated from deltas");
                assert!(r.cost_usd > 0.0);
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("record was not persisted after the capture was dropped");
    }
}
