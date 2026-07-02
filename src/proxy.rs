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

use crate::protocol::{self, Provider};
use crate::record::{self, RequestRecord};
use crate::store::Store;

pub struct AppState {
    pub client: reqwest::Client,
    pub anthropic_upstream: String,
    pub openai_upstream: String,
    pub store: Arc<Store>,
}

impl AppState {
    pub fn new(anthropic_upstream: String, openai_upstream: String, store: Arc<Store>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
            anthropic_upstream: anthropic_upstream.trim_end_matches('/').to_string(),
            openai_upstream: openai_upstream.trim_end_matches('/').to_string(),
            store,
        }
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/_llmscope/requests", get(list_requests))
        .route("/_llmscope/diff/{id}", get(get_diff))
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
    };

    // Tee the upstream stream: forward every chunk untouched, accumulate a
    // copy, and record usage/timing once the stream ends. If the client
    // disconnects mid-stream the record is dropped (v0 limitation).
    let st2 = Arc::clone(&st);
    let mut upstream_stream = upstream_resp.bytes_stream();
    let tee = async_stream::stream! {
        let mut ctx = ctx;
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = upstream_stream.next().await {
            match chunk {
                Ok(bytes) => {
                    if ctx.ttft.is_none() {
                        ctx.ttft = Some(started.elapsed());
                    }
                    buf.extend_from_slice(&bytes);
                    yield Ok::<Bytes, reqwest::Error>(bytes);
                }
                Err(e) => {
                    yield Err(e);
                    break;
                }
            }
        }
        ctx.duration = started.elapsed();
        finalize(&st2, ctx, buf);
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
    let cost = record::cost_usd(ctx.provider, &model, &usage);

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
        estimated: usage.estimated,
        session: ctx.session,
    };

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
