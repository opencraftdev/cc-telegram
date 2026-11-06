//! HTTP API gateway: OpenAI-compatible `/v1/chat/completions` + native `/v1/runs`, served by
//! `claude -p` on this machine (the user's Claude subscription; no Anthropic API key involved).

use crate::profile::{ApiKey, Config, Profile, expand_tilde, random_bytes};
use crate::queue::Queues;
use crate::runner::{self, Event, Outcome, RunOpts, RunResult};
use crate::state::SessionOwners;
use anyhow::{Context, Result};
use axum::Extension;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Path as UrlPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event as Sse, KeepAlive};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{info, warn};

pub const BODY_LIMIT: usize = 20 * 1024 * 1024;
const ALIASES: [&str; 3] = ["opus", "sonnet", "haiku"];
const SESSION_HEADER: &str = "x-cc-session";
const RUN_HEADER: &str = "x-cc-run";
const STALE_MSG: &str =
    "unknown session id (X-CC-Session / session_id) for this key: omit it to start a new session";

// ---------- keys ----------

const B62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// `ccg_` + 43 base62 chars (~256 bits), unbiased via rejection sampling.
pub fn new_key() -> Result<String> {
    let mut out = String::from("ccg_");
    while out.len() < 4 + 43 {
        for b in random_bytes(64)? {
            if b < 248 && out.len() < 4 + 43 {
                out.push(B62[(b % 62) as usize] as char);
            }
        }
    }
    Ok(out)
}

pub fn hash_key(key: &str) -> String {
    hex(&Sha256::digest(key.as_bytes()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Constant-time match of a presented key against stored hashes (checks every key).
pub fn find_key<'a>(keys: &'a [ApiKey], presented: &str) -> Option<&'a ApiKey> {
    let h = hash_key(presented);
    let mut hit = None;
    for k in keys {
        if ct_eq(k.sha256.as_bytes(), h.as_bytes()) {
            hit = Some(k);
        }
    }
    hit
}

/// Session ids go to `--resume`; only accept plain id characters.
fn valid_session(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

// ---------- errors ----------

/// OpenAI-shaped error body.
fn err(status: StatusCode, kind: &str, msg: impl Into<String>) -> Response {
    let body = json!({"error": {"message": msg.into(), "type": kind, "code": Value::Null}});
    (status, Json(body)).into_response()
}

fn bad_request(msg: impl Into<String>) -> Response {
    err(StatusCode::BAD_REQUEST, "invalid_request_error", msg)
}

/// Map a failed run to (status, type, message). None = success.
fn failure(out: &Outcome, resumed: bool) -> Option<(StatusCode, &'static str, String)> {
    let stale = (
        StatusCode::NOT_FOUND,
        "session_not_found",
        STALE_MSG.to_string(),
    );
    match out {
        Outcome::Done(r) if !r.is_error => None,
        Outcome::Done(r) if resumed && r.is_stale_session() => Some(stale),
        Outcome::Done(r) => Some((
            StatusCode::BAD_GATEWAY,
            "api_error",
            if r.errors.is_empty() {
                format!("claude error: {}", r.text)
            } else {
                format!("claude error: {}", r.errors.join("; "))
            },
        )),
        Outcome::Failed(e) if resumed && runner::is_stale_text(e) => Some(stale),
        Outcome::Failed(e) => Some((
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            format!("claude failed: {e} → run: cc-gateway doctor"),
        )),
        Outcome::TimedOut(_) => Some((
            StatusCode::GATEWAY_TIMEOUT,
            "timeout",
            "run timed out (run_timeout_secs)".into(),
        )),
        Outcome::Stopped(_) => Some((StatusCode::CONFLICT, "cancelled", "run was stopped".into())),
    }
}

// ---------- OpenAI request mapping ----------

#[derive(Debug)]
pub struct ChatInput {
    pub system: String,
    pub prompt: String,
    pub model: Option<String>,
    pub stream: bool,
}

/// Strict base64: one alphabet (standard or URL-safe), optional padding only at the end,
/// no whitespace, valid length, zero leftover bits.
pub fn b64decode(s: &str) -> Option<Vec<u8>> {
    let url = s.contains(['-', '_']);
    if url && s.contains(['+', '/']) {
        return None;
    }
    let body = s.trim_end_matches('=');
    let pad = s.len() - body.len();
    if pad > 2 || (pad > 0 && !s.len().is_multiple_of(4)) || body.len() % 4 == 1 {
        return None;
    }
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' if !url => Some(62),
        b'/' if !url => Some(63),
        b'-' if url => Some(62),
        b'_' if url => Some(63),
        _ => None,
    };
    let (mut out, mut acc, mut bits) = (Vec::with_capacity(body.len() * 3 / 4), 0u32, 0);
    for c in body.bytes() {
        acc = ((acc << 6) | val(c)? as u32) & 0xFFFF;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    (acc & ((1 << bits) - 1) == 0).then_some(out)
}

/// `data:<mime>;base64,<payload>` -> (mime, bytes)
fn data_url(url: &str) -> Option<(String, Vec<u8>)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let mime = meta.strip_suffix(";base64")?;
    Some((mime.to_string(), b64decode(payload)?))
}

type SaveImage<'a> = dyn FnMut(&str, &[u8]) -> Result<PathBuf, String> + 'a;

fn content_text(content: &Value, save: &mut SaveImage) -> Result<String, String> {
    match content {
        Value::String(s) => Ok(s.clone()),
        Value::Null => Ok(String::new()),
        Value::Array(parts) => {
            let mut out = vec![];
            for p in parts {
                match p["type"].as_str() {
                    Some("text") => out.push(p["text"].as_str().unwrap_or("").to_string()),
                    Some("image_url") => {
                        let url = p["image_url"]["url"]
                            .as_str()
                            .or(p["image_url"].as_str())
                            .unwrap_or("");
                        let Some((mime, bytes)) = data_url(url) else {
                            return Err("image_url must be a base64 data: URL (data:image/png;base64,...); remote URLs are not fetched".into());
                        };
                        let path = save(&mime, &bytes)?;
                        out.push(format!("[Attached image: {}]", path.display()));
                    }
                    _ => {} // unsupported part types are ignored
                }
            }
            Ok(out.join("\n"))
        }
        _ => Err("message content must be a string or an array of parts".into()),
    }
}

/// OpenAI messages -> (extra system prompt, claude prompt). Stateless: the conversation is
/// rendered into the prompt; with `resume`, only the last user message is sent (and only its
/// images are saved).
pub fn chat_input(body: &Value, resume: bool, save: &mut SaveImage) -> Result<ChatInput, String> {
    let msgs = body["messages"]
        .as_array()
        .filter(|m| !m.is_empty())
        .ok_or("`messages` must be a non-empty array")?;
    let role = |m: &Value| m["role"].as_str().unwrap_or("user").to_string();
    let is_system = |r: &str| r == "system" || r == "developer";
    let mut system = vec![];
    for m in msgs.iter().filter(|m| is_system(&role(m))) {
        system.push(content_text(&m["content"], save)?);
    }
    let turns: Vec<&Value> = msgs.iter().filter(|m| !is_system(&role(m))).collect();
    let last_user = turns
        .iter()
        .rposition(|m| role(m) == "user")
        .ok_or("no user message")?;
    let prompt = if resume || turns.len() == 1 {
        content_text(&turns[last_user]["content"], save)?
    } else {
        let mut s = String::from("Conversation so far (oldest first):\n");
        for m in &turns {
            let who = match role(m).as_str() {
                "assistant" => "Assistant",
                "user" => "User",
                _ => "Tool",
            };
            s.push_str(&format!(
                "\n{who}: {}\n",
                content_text(&m["content"], save)?
            ));
        }
        s.push_str("\nReply to the last User message.");
        s
    };
    Ok(ChatInput {
        system: if system.is_empty() {
            String::new()
        } else {
            format!(
                "\n\n## Instructions from the API client\n{}",
                system.join("\n\n")
            )
        },
        prompt,
        model: body["model"].as_str().map(str::to_string),
        stream: body["stream"].as_bool().unwrap_or(false),
    })
}

// ---------- OpenAI response shapes ----------

fn usage(r: &RunResult) -> Value {
    json!({
        "prompt_tokens": r.input_tokens,
        "completion_tokens": r.output_tokens,
        "total_tokens": r.input_tokens + r.output_tokens,
        "total_cost_usd": r.cost_usd,
    })
}

pub fn completion(id: &str, model: &str, created: i64, r: &RunResult) -> Value {
    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": r.text},
            "finish_reason": "stop",
        }],
        "usage": usage(r),
    })
}

pub fn chunk(id: &str, model: &str, created: i64, delta: Value, finish: Option<&str>) -> Value {
    json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    })
}

// ---------- server ----------

pub struct Api {
    profile: Profile,
    queues: Arc<Queues>,
    owners: Arc<SessionOwners>,
    sem: Arc<Semaphore>,
    max_concurrent: usize,
    /// run id -> (owning key, stop handle)
    runs: Mutex<HashMap<String, (String, Arc<Notify>)>>,
}

impl Api {
    pub fn new(
        profile: Profile,
        queues: Arc<Queues>,
        owners: Arc<SessionOwners>,
        max_concurrent: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            profile,
            queues,
            owners,
            sem: Arc::new(Semaphore::new(max_concurrent)),
            max_concurrent,
            runs: Mutex::default(),
        })
    }
}

pub fn router(api: Arc<Api>) -> Router {
    // Auth runs as middleware, before any handler reads the body.
    let v1 = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/runs", post(runs))
        .route("/v1/runs/{id}", delete(stop_run))
        .route_layer(axum::middleware::from_fn_with_state(api.clone(), auth_mw));
    Router::new()
        .route("/health", get(health))
        .merge(v1)
        .fallback(|| async { err(StatusCode::NOT_FOUND, "not_found", "unknown endpoint") })
        .method_not_allowed_fallback(|| async {
            err(
                StatusCode::METHOD_NOT_ALLOWED,
                "invalid_request_error",
                "method not allowed for this endpoint",
            )
        })
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(api)
}

pub async fn serve(
    profile: Profile,
    queues: Arc<Queues>,
    owners: Arc<SessionOwners>,
) -> Result<()> {
    let cfg = profile.load_config()?;
    let addr: SocketAddr = cfg.api.bind.parse().with_context(|| {
        format!(
            "✗ invalid [api] bind {:?} → run: cc-gateway profile edit {}",
            cfg.api.bind, profile.name
        )
    })?;
    if !addr.ip().is_loopback() {
        let msg = format!(
            "⚠️  API bound to {addr} (not loopback). Every API key holder can run arbitrary commands \
             as your user via claude. A leaked key = remote code execution. Prefer 127.0.0.1 + a tunnel."
        );
        warn!("{msg}");
        eprintln!("{msg}");
    }
    if cfg.api.keys.is_empty() {
        warn!(
            "API has no keys; every request will get 401 → run: cc-gateway key create {} <name>",
            profile.name
        );
    }
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("✗ could not bind API to {addr} (port in use?)"))?;
    info!(profile = %profile.name, "API listening on http://{addr}/v1");
    let api = Api::new(profile, queues, owners, cfg.api.max_concurrent.max(1));
    axum::serve(listener, router(api)).await?;
    Ok(())
}

async fn health() -> Json<Value> {
    Json(json!({"ok": true, "version": env!("CARGO_PKG_VERSION")}))
}

/// The authenticated key and the config it was checked against.
#[derive(Clone)]
struct Authed {
    key: String,
    cfg: Arc<Config>,
}

/// `Authorization: Bearer <key>`; the scheme is case-insensitive.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, key) = v.trim().split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| key.trim())
}

/// Authenticate against the live config (keys can be created/revoked while running).
#[allow(clippy::result_large_err)]
fn auth(api: &Api, headers: &HeaderMap) -> Result<Authed, Response> {
    let cfg = api.profile.load_config().map_err(|e| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            format!("{e:#}"),
        )
    })?;
    let Some(presented) = bearer(headers) else {
        return Err(err(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "missing API key: send Authorization: Bearer ccg_...",
        ));
    };
    match find_key(&cfg.api.keys, presented) {
        Some(k) => Ok(Authed {
            key: k.name.clone(),
            cfg: Arc::new(cfg.clone()),
        }),
        None => Err(err(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid or revoked API key",
        )),
    }
}

async fn auth_mw(
    State(api): State<Arc<Api>>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    match auth(&api, req.headers()) {
        Ok(a) => {
            req.extensions_mut().insert(a);
            next.run(req).await
        }
        Err(r) => r,
    }
}

/// Body as JSON; size-limit and parse failures in OpenAI error shape.
#[allow(clippy::result_large_err)]
fn json_body(body: Result<Bytes, BytesRejection>) -> Result<Value, Response> {
    match body {
        Ok(b) => serde_json::from_slice(&b).map_err(|_| bad_request("request body must be JSON")),
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => Err(err(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            format!("request body larger than {} MB", BODY_LIMIT / 1024 / 1024),
        )),
        Err(e) => Err(bad_request(e.body_text())),
    }
}

/// Only the key that created a session may resume it (Telegram sessions are never resumable).
#[allow(clippy::result_large_err)]
fn check_owner(api: &Api, key: &str, session: &Option<String>) -> Result<(), Response> {
    match session {
        Some(s) if api.owners.owner(s).as_deref() != Some(&format!("key:{key}")) => {
            Err(err(StatusCode::NOT_FOUND, "session_not_found", STALE_MSG))
        }
        _ => Ok(()),
    }
}

async fn models(Extension(a): Extension<Authed>) -> Response {
    let mut ids: Vec<String> = ALIASES.iter().map(|s| s.to_string()).collect();
    if !ids.contains(&a.cfg.default_model) {
        ids.push(a.cfg.default_model.clone());
    }
    let data: Vec<Value> = ids
        .iter()
        .map(|id| json!({"id": id, "object": "model", "created": 0, "owned_by": "anthropic"}))
        .collect();
    Json(json!({"object": "list", "data": data})).into_response()
}

/// Messages from a running job to the request handler.
enum Msg {
    Ev(Event),
    Done(Outcome),
}

/// Stops the run if the client goes away (handler future or SSE stream dropped).
struct StopOnDrop(Arc<Notify>);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

struct Started {
    run_id: String,
    rx: mpsc::UnboundedReceiver<Msg>,
    guard: StopOnDrop,
}

struct RunReq<'a> {
    key: &'a str,
    prompt: String,
    system_extra: &'a str,
    model: Option<String>,
    session: Option<String>,
    /// Request files (saved images) deleted once the run ends.
    cleanup: Vec<PathBuf>,
}

#[allow(clippy::result_large_err)]
fn start(
    api: &Arc<Api>,
    cfg: &Config,
    req: RunReq,
    permit: OwnedSemaphorePermit,
) -> Result<Started, Response> {
    let RunReq {
        key,
        prompt,
        system_extra,
        model,
        session,
        cleanup,
    } = req;
    let ise = |e: anyhow::Error| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            format!("{e:#}"),
        )
    };
    let run_id = hex(&random_bytes(8).map_err(ise)?);
    let (cwd, cwd_args) = api.profile.claude_cwd(cfg);
    let system = crate::prompt::assemble(
        &api.profile,
        &format!("key-{key}"),
        crate::prompt::Channel::Api {
            key: key.to_string(),
        },
        &cwd,
        None,
    )
    .map_err(ise)?
        + system_extra;
    let opts = RunOpts {
        claude_bin: expand_tilde(&cfg.claude_bin).to_string_lossy().into(),
        prompt,
        system_prompt: system,
        model: model.unwrap_or_else(|| cfg.default_model.clone()),
        max_turns: cfg.max_turns,
        resume: session.clone(),
        extra_args: [cwd_args, cfg.extra_claude_args.clone()].concat(),
        cwd,
        timeout: Duration::from_secs(cfg.run_timeout_secs),
    };
    // Queued by session id alone, so one session never runs twice at once; new sessions
    // run in parallel.
    let qkey = match session {
        Some(s) => format!("session:{s}"),
        None => format!("api-new:{run_id}"),
    };
    let owner = format!("key:{key}");
    let (tx, rx) = mpsc::unbounded_channel();
    let (a, id) = (api.clone(), run_id.clone());
    let mut runs = api.runs.lock().unwrap(); // held across push+insert; the job removes its entry
    let stop = api.queues.push(
        &qkey,
        Box::new(move |stop| {
            Box::pin(async move {
                let _permit = permit;
                let (etx, mut erx) = mpsc::unbounded_channel();
                let fwd = async {
                    while let Some(e) = erx.recv().await {
                        if let Event::Init(sid) = &e {
                            a.owners.claim(sid, &owner);
                        }
                        let _ = tx.send(Msg::Ev(e));
                    }
                };
                let (out, ()) = tokio::join!(runner::run(&opts, etx, stop), fwd);
                for f in &cleanup {
                    let _ = std::fs::remove_file(f);
                }
                let _ = tx.send(Msg::Done(out));
                a.runs.lock().unwrap().remove(&id);
            })
        }),
    );
    runs.insert(run_id.clone(), (key.to_string(), stop.clone()));
    Ok(Started {
        run_id,
        rx,
        guard: StopOnDrop(stop),
    })
}

fn acquire(api: &Arc<Api>) -> Option<OwnedSemaphorePermit> {
    api.sem.clone().try_acquire_owned().ok()
}

fn too_many(api: &Api) -> Response {
    err(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limit_error",
        format!(
            "too many concurrent requests (max_concurrent = {})",
            api.max_concurrent
        ),
    )
}

fn with_headers(mut r: Response, run_id: &str, session: Option<&str>) -> Response {
    if let Ok(v) = HeaderValue::from_str(run_id) {
        r.headers_mut().insert(RUN_HEADER, v);
    }
    if let Some(v) = session.and_then(|s| HeaderValue::from_str(s).ok()) {
        r.headers_mut().insert(SESSION_HEADER, v);
    }
    r
}

/// Wait for the session id (init) or an early finish; buffer whatever came before.
async fn first_session(rx: &mut mpsc::UnboundedReceiver<Msg>) -> (Option<String>, Vec<Msg>) {
    let mut buf = vec![];
    while let Some(m) = rx.recv().await {
        match m {
            Msg::Ev(Event::Init(id)) => return (Some(id), buf),
            Msg::Done(o) => {
                buf.push(Msg::Done(o));
                return (None, buf);
            }
            m => buf.push(m),
        }
    }
    (None, buf)
}

/// Drain to the final outcome.
async fn finish(rx: &mut mpsc::UnboundedReceiver<Msg>, mut buf: Vec<Msg>) -> Outcome {
    if let Some(Msg::Done(o)) = buf.pop() {
        return o;
    }
    while let Some(m) = rx.recv().await {
        if let Msg::Done(o) = m {
            return o;
        }
    }
    Outcome::Failed("run ended without a result".into())
}

fn session_of(o: &Outcome) -> Option<&str> {
    match o {
        Outcome::Done(r) => r.session_id.as_deref(),
        Outcome::TimedOut(s) | Outcome::Stopped(s) => s.as_deref(),
        Outcome::Failed(_) => None,
    }
}

/// A run that ended before streaming started (bad session, spawn failure, ...): plain JSON.
async fn early_error(
    rx: &mut mpsc::UnboundedReceiver<Msg>,
    buf: Vec<Msg>,
    run_id: &str,
    resumed: bool,
) -> Response {
    let out = finish(rx, buf).await;
    let r = match failure(&out, resumed) {
        Some((st, kind, msg)) => err(st, kind, msg),
        None => err(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "claude produced no session",
        ),
    };
    with_headers(r, run_id, session_of(&out))
}

fn sse_response(
    rx: mpsc::UnboundedReceiver<Sse>,
    guard: StopOnDrop,
    run_id: &str,
    session: Option<&str>,
) -> Response {
    let stream = UnboundedReceiverStream::new(rx).map(move |e| {
        let _keep = &guard; // dropped with the stream: client gone -> stop the run
        Ok::<_, Infallible>(e)
    });
    let r = axum::response::Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response();
    with_headers(r, run_id, session)
}

fn header_session(headers: &HeaderMap) -> Result<Option<String>, String> {
    match headers.get(SESSION_HEADER).map(|v| v.to_str()) {
        None => Ok(None),
        Some(Ok(s)) if valid_session(s.trim()) => Ok(Some(s.trim().to_string())),
        Some(_) => Err("invalid X-CC-Session header".into()),
    }
}

async fn chat(
    State(api): State<Arc<Api>>,
    Extension(Authed { key, cfg }): Extension<Authed>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let session = match header_session(&headers) {
        Ok(s) => s,
        Err(e) => return bad_request(e),
    };
    let body = match json_body(body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if let Err(r) = check_owner(&api, &key, &session) {
        return r;
    }
    // Take a slot before writing anything to disk.
    let Some(permit) = acquire(&api) else {
        return too_many(&api);
    };
    let inbox = api
        .profile
        .workspace()
        .join("inbox")
        .join(format!("key-{key}"));
    let mut saved: Vec<PathBuf> = vec![];
    let parsed = {
        let mut save = |mime: &str, bytes: &[u8]| -> Result<PathBuf, String> {
            let ext = match mime {
                "image/png" => "png",
                "image/jpeg" | "image/jpg" => "jpg",
                "image/gif" => "gif",
                "image/webp" => "webp",
                _ => "bin",
            };
            std::fs::create_dir_all(&inbox).map_err(|e| e.to_string())?;
            let p = inbox.join(format!(
                "{}-{}.{ext}",
                chrono::Local::now().format("%Y%m%d-%H%M%S%3f"),
                saved.len() + 1
            ));
            std::fs::write(&p, bytes).map_err(|e| e.to_string())?;
            saved.push(p.clone());
            Ok(p)
        };
        chat_input(&body, session.is_some(), &mut save)
    };
    let input = match parsed {
        Ok(i) => i,
        Err(e) => {
            for f in &saved {
                let _ = std::fs::remove_file(f);
            }
            return bad_request(e);
        }
    };
    let model = input.model.clone().unwrap_or(cfg.default_model.clone());
    let resumed = session.is_some();
    let Started {
        run_id,
        mut rx,
        guard,
    } = match start(
        &api,
        &cfg,
        RunReq {
            key: &key,
            prompt: input.prompt,
            system_extra: &input.system,
            model: input.model,
            session,
            cleanup: saved,
        },
        permit,
    ) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let id = format!("chatcmpl-{run_id}");
    let created = chrono::Utc::now().timestamp();

    if !input.stream {
        let out = finish(&mut rx, vec![]).await;
        drop(guard);
        return match (failure(&out, resumed), &out) {
            (Some((st, kind, msg)), _) => {
                with_headers(err(st, kind, msg), &run_id, session_of(&out))
            }
            (None, Outcome::Done(r)) => with_headers(
                Json(completion(&id, &model, created, r)).into_response(),
                &run_id,
                r.session_id.as_deref(),
            ),
            (None, _) => unreachable!("success is always Done"),
        };
    }

    let (sid, buf) = first_session(&mut rx).await;
    if sid.is_none() {
        return early_error(&mut rx, buf, &run_id, resumed).await;
    }
    let (stx, srx) = mpsc::unbounded_channel::<Sse>();
    let data = |v: Value| Sse::default().data(v.to_string());
    let _ = stx.send(data(chunk(
        &id,
        &model,
        created,
        json!({"role": "assistant", "content": ""}),
        None,
    )));
    tokio::spawn(async move {
        let mut streamed = false;
        let mut pending = buf.into_iter();
        loop {
            let m = match pending.next() {
                Some(m) => m,
                None => match rx.recv().await {
                    Some(m) => m,
                    None => break,
                },
            };
            match m {
                Msg::Ev(Event::Text(t)) => {
                    let t = if streamed { format!("\n\n{t}") } else { t };
                    streamed = true;
                    let _ = stx.send(data(chunk(
                        &id,
                        &model,
                        created,
                        json!({"content": t}),
                        None,
                    )));
                }
                Msg::Ev(_) => {}
                Msg::Done(out) => {
                    match (failure(&out, resumed), &out) {
                        (None, Outcome::Done(r)) => {
                            if !streamed && !r.text.is_empty() {
                                let delta = json!({"content": r.text});
                                let _ = stx.send(data(chunk(&id, &model, created, delta, None)));
                            }
                            let mut last = chunk(&id, &model, created, json!({}), Some("stop"));
                            last["usage"] = usage(r);
                            let _ = stx.send(data(last));
                        }
                        (Some((_, kind, msg)), _) => {
                            let e = json!({"error": {"message": msg, "type": kind, "code": Value::Null}});
                            let _ = stx.send(data(e));
                        }
                        (None, _) => {}
                    }
                    let _ = stx.send(Sse::default().data("[DONE]"));
                    break;
                }
            }
        }
    });
    sse_response(srx, guard, &run_id, sid.as_deref())
}

async fn runs(
    State(api): State<Arc<Api>>,
    Extension(Authed { key, cfg }): Extension<Authed>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match json_body(body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let Some(prompt) = body["prompt"].as_str().filter(|p| !p.trim().is_empty()) else {
        return bad_request("`prompt` (string) is required");
    };
    let session = match body["session_id"].as_str() {
        None => None,
        Some(s) if valid_session(s) => Some(s.to_string()),
        Some(_) => return bad_request("invalid session_id"),
    };
    if let Err(r) = check_owner(&api, &key, &session) {
        return r;
    }
    let stream = body["stream"].as_bool().unwrap_or(false);
    let Some(permit) = acquire(&api) else {
        return too_many(&api);
    };
    let resumed = session.is_some();
    let Started {
        run_id,
        mut rx,
        guard,
    } = match start(
        &api,
        &cfg,
        RunReq {
            key: &key,
            prompt: prompt.to_string(),
            system_extra: "",
            model: body["model"].as_str().map(str::to_string),
            session,
            cleanup: vec![],
        },
        permit,
    ) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let result_json = |r: &RunResult, run_id: &str| {
        json!({
            "run_id": run_id,
            "text": r.text,
            "session_id": r.session_id,
            "cost_usd": r.cost_usd,
            "duration_ms": r.duration_ms,
            "is_error": r.is_error,
        })
    };

    if !stream {
        let out = finish(&mut rx, vec![]).await;
        drop(guard);
        return match (failure(&out, resumed), &out) {
            (Some((st, kind, msg)), _) => {
                with_headers(err(st, kind, msg), &run_id, session_of(&out))
            }
            (None, Outcome::Done(r)) => with_headers(
                Json(result_json(r, &run_id)).into_response(),
                &run_id,
                r.session_id.as_deref(),
            ),
            (None, _) => unreachable!("success is always Done"),
        };
    }

    // Wait for the session so it can go in the response headers, like chat streaming.
    let (sid, buf) = first_session(&mut rx).await;
    let Some(sid) = sid else {
        return early_error(&mut rx, buf, &run_id, resumed).await;
    };
    let (stx, srx) = mpsc::unbounded_channel::<Sse>();
    let ev = |name: &str, v: Value| Sse::default().event(name).data(v.to_string());
    let _ = stx.send(ev("init", json!({"run_id": run_id, "session_id": sid})));
    let id = run_id.clone();
    tokio::spawn(async move {
        let mut pending = buf.into_iter();
        loop {
            let m = match pending.next() {
                Some(m) => m,
                None => match rx.recv().await {
                    Some(m) => m,
                    None => break,
                },
            };
            let e = match m {
                Msg::Ev(Event::Tool { name, input }) => {
                    ev("tool", json!({"name": name, "input": input}))
                }
                Msg::Ev(Event::Text(t)) => ev("text", json!({"text": t})),
                Msg::Ev(_) => continue,
                Msg::Done(out) => {
                    let e = match (failure(&out, resumed), &out) {
                        (None, Outcome::Done(r)) => ev("result", result_json(r, &id)),
                        (Some((_, kind, msg)), _) => {
                            ev("error", json!({"type": kind, "message": msg}))
                        }
                        (None, _) => break,
                    };
                    let _ = stx.send(e);
                    break;
                }
            };
            let _ = stx.send(e);
        }
    });
    sse_response(srx, guard, &run_id, Some(&sid))
}

async fn stop_run(
    State(api): State<Arc<Api>>,
    Extension(Authed { key, .. }): Extension<Authed>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let found = api
        .runs
        .lock()
        .unwrap()
        .get(&id)
        .filter(|(owner, _)| *owner == key)
        .map(|(_, stop)| stop.clone());
    match found {
        Some(stop) => {
            stop.notify_one();
            Json(json!({"run_id": id, "stopped": true})).into_response()
        }
        None => err(
            StatusCode::NOT_FOUND,
            "not_found",
            "no running run with that id for this key",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::ApiKey;

    #[test]
    fn keys_hash_and_match() {
        let k = new_key().unwrap();
        assert!(k.starts_with("ccg_") && k.len() == 47);
        assert!(k[4..].bytes().all(|b| B62.contains(&b)));
        assert_ne!(k, new_key().unwrap());
        let stored = vec![ApiKey {
            name: "laptop".into(),
            sha256: hash_key(&k),
            created_at: String::new(),
        }];
        assert_eq!(find_key(&stored, &k).unwrap().name, "laptop");
        assert!(find_key(&stored, "ccg_wrong").is_none());
        assert_eq!(hash_key("abc").len(), 64);
    }

    #[test]
    fn base64_and_data_urls() {
        assert_eq!(b64decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(b64decode("aGVsbG8").unwrap(), b"hello"); // unpadded ok
        assert_eq!(b64decode("aGk").unwrap(), b"hi");
        assert_eq!(b64decode("-_8=").unwrap(), vec![0xfb, 0xff]); // url-safe
        assert_eq!(b64decode("+/8=").unwrap(), vec![0xfb, 0xff]); // standard
        assert_eq!(b64decode("").unwrap(), b"");
        for bad in [
            "aGVs\nbG8", // whitespace
            "a$b",       // invalid char
            "aG=k",      // padding in the middle
            "aGk==",     // too much padding for the length
            "aGVsbG8==", // wrong padding
            "a",         // impossible length
            "aGl=",      // leftover bits set
            "+_8=",      // mixed alphabets
        ] {
            assert!(b64decode(bad).is_none(), "{bad:?}");
        }
        let (mime, b) = data_url("data:image/png;base64,iVBORw==").unwrap();
        assert_eq!((mime.as_str(), b.len()), ("image/png", 4));
        assert!(data_url("https://x/y.png").is_none());
    }

    fn no_images(_: &str, _: &[u8]) -> Result<PathBuf, String> {
        panic!("no image expected")
    }

    #[test]
    fn openai_messages_to_prompt() {
        let body = json!({"model": "opus", "stream": true, "temperature": 0.2, "messages": [
            {"role": "system", "content": "Be terse."},
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"},
            {"role": "user", "content": [{"type": "text", "text": "what's 2+2?"}]},
        ]});
        let i = chat_input(&body, false, &mut no_images).unwrap();
        assert!(i.stream);
        assert_eq!(i.model.as_deref(), Some("opus"));
        assert!(
            i.system
                .contains("Instructions from the API client\nBe terse.")
        );
        assert!(
            i.prompt
                .contains("User: hi\n\nAssistant: hello\n\nUser: what's 2+2?")
        );
        assert!(i.prompt.ends_with("Reply to the last User message."));
        // with a session only the last user message is sent
        assert_eq!(
            chat_input(&body, true, &mut no_images).unwrap().prompt,
            "what's 2+2?"
        );
        // a single user message is passed as-is
        let one = json!({"messages": [{"role": "user", "content": "yo"}]});
        let i = chat_input(&one, false, &mut no_images).unwrap();
        assert_eq!(
            (i.prompt.as_str(), i.system.as_str(), i.stream),
            ("yo", "", false)
        );
        assert!(chat_input(&json!({"messages": []}), false, &mut no_images).is_err());
        assert!(
            chat_input(
                &json!({"messages": [{"role": "system", "content": "x"}]}),
                false,
                &mut no_images
            )
            .is_err()
        );
    }

    #[test]
    fn images_saved_and_remote_urls_rejected() {
        let mut saved = vec![];
        let mut save = |mime: &str, b: &[u8]| {
            saved.push((mime.to_string(), b.to_vec()));
            Ok(PathBuf::from("/inbox/1.png"))
        };
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "what is this?"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGk="}},
        ]}]});
        let i = chat_input(&body, false, &mut save).unwrap();
        assert_eq!(i.prompt, "what is this?\n[Attached image: /inbox/1.png]");
        assert_eq!(saved, vec![("image/png".to_string(), b"hi".to_vec())]);
        let remote = json!({"messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": "http://169.254.169.254/x"}},
        ]}]});
        let e = chat_input(&remote, false, &mut no_images).unwrap_err();
        assert!(e.contains("remote URLs are not fetched"));
        // with a session only the last user message is sent, so older images aren't saved
        let history = json!({"messages": [
            {"role": "user", "content": [{"type": "image_url", "image_url": {"url": "data:image/png;base64,aGk="}}]},
            {"role": "assistant", "content": "a cat"},
            {"role": "user", "content": "thanks"},
        ]});
        assert_eq!(
            chat_input(&history, true, &mut no_images).unwrap().prompt,
            "thanks"
        );
    }

    // ---------- HTTP tests against a fake claude ----------

    struct Fixture {
        _dir: tempfile::TempDir,
        base: String,
        key: String,
        other_key: String,
        args: PathBuf,
        profile: Profile,
        owners: Arc<SessionOwners>,
    }

    /// Fake claude: records its args, prints stream-json. `extra` runs right after `init`.
    async fn fixture(extra: &str, max_concurrent: usize) -> Fixture {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let args = dir.path().join("args.txt");
        let bin = dir.path().join("claude.sh");
        std::fs::write(
            &bin,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$@" > '{args}'
case "$*" in *bogus-session*)
  echo '{{"type":"result","is_error":true,"session_id":"bogus-session","errors":["No conversation found with session ID: bogus-session"]}}'; exit 1;;
esac
echo '{{"type":"system","subtype":"init","session_id":"sess-1"}}'
{extra}
echo '{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"Bash","input":{{"command":"ls"}}}}]}}}}'
echo '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"four"}}]}}}}'
echo '{{"type":"result","is_error":false,"result":"four","session_id":"sess-1","total_cost_usd":0.01,"duration_ms":5,"usage":{{"input_tokens":3,"output_tokens":2}}}}'
"#,
                args = args.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (key, other_key) = (new_key().unwrap(), new_key().unwrap());
        let mut cfg = Config::new(String::new());
        cfg.claude_bin = bin.to_string_lossy().into();
        cfg.api.enabled = true;
        for (name, k) in [("laptop", &key), ("other", &other_key)] {
            cfg.api.keys.push(ApiKey {
                name: name.into(),
                sha256: hash_key(k),
                created_at: String::new(),
            });
        }
        let profile = Profile::create_at(dir.path().join("profile"), "t", &cfg).unwrap();
        let owners = Arc::new(SessionOwners::open(profile.sessions_path()));
        let api = Api::new(
            profile.clone(),
            Arc::new(Queues::default()),
            owners.clone(),
            max_concurrent,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router(api)).await.unwrap() });
        Fixture {
            _dir: dir,
            base,
            key,
            other_key,
            args,
            profile,
            owners,
        }
    }

    async fn post(
        f: &Fixture,
        path: &str,
        key: Option<&str>,
        session: Option<&str>,
        body: Value,
    ) -> (u16, HeaderMap, String) {
        let mut req = reqwest::Client::new()
            .post(format!("{}{path}", f.base))
            .header("content-type", "application/json")
            .body(body.to_string());
        if let Some(k) = key {
            req = req.header("authorization", format!("Bearer {k}"));
        }
        if let Some(s) = session {
            req = req.header("x-cc-session", s);
        }
        let r = req.send().await.unwrap();
        let (st, h) = (r.status().as_u16(), r.headers().clone());
        (st, h, r.text().await.unwrap())
    }

    fn hi() -> Value {
        json!({"model": "haiku", "messages": [{"role": "user", "content": "2+2?"}]})
    }

    #[tokio::test]
    async fn auth_missing_bad_and_revoked() {
        let f = fixture("", 4).await;
        let health = reqwest::get(format!("{}/health", f.base)).await.unwrap();
        assert_eq!(health.status().as_u16(), 200);
        let (st, _, body) = post(&f, "/v1/chat/completions", None, None, hi()).await;
        assert_eq!(st, 401);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["error"]["type"], "authentication_error");
        let (st, _, _) = post(&f, "/v1/chat/completions", Some("ccg_nope"), None, hi()).await;
        assert_eq!(st, 401);
        let (st, _, _) = post(&f, "/v1/chat/completions", Some(&f.key), None, hi()).await;
        assert_eq!(st, 200);
        // revoke: removed from config -> rejected on the next request (config is re-read)
        let mut cfg = f.profile.load_config().unwrap();
        cfg.api.keys.clear();
        f.profile.save_config(&cfg).unwrap();
        let (st, _, _) = post(&f, "/v1/chat/completions", Some(&f.key), None, hi()).await;
        assert_eq!(st, 401);
    }

    #[tokio::test]
    async fn non_stream_completion_shape_and_session_header() {
        let f = fixture("", 4).await;
        let (st, h, body) = post(&f, "/v1/chat/completions", Some(&f.key), None, hi()).await;
        assert_eq!(st, 200, "{body}");
        assert_eq!(h["x-cc-session"], "sess-1");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["object"], "chat.completion");
        assert_eq!(v["model"], "haiku");
        assert_eq!(v["choices"][0]["message"]["role"], "assistant");
        assert_eq!(v["choices"][0]["message"]["content"], "four");
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
        assert_eq!(v["usage"]["total_tokens"], 5);
        assert_eq!(v["usage"]["total_cost_usd"], 0.01);
        let args = std::fs::read_to_string(&f.args).unwrap();
        assert!(!args.contains("--resume"));
        assert!(args.contains("haiku"));
        // memory namespace per key
        assert!(f.profile.ns_dir("key-laptop").join("MEMORY.md").exists());
    }

    #[tokio::test]
    async fn stream_chunks_and_done() {
        let f = fixture("", 4).await;
        let mut body = hi();
        body["stream"] = json!(true);
        let (st, h, text) = post(&f, "/v1/chat/completions", Some(&f.key), None, body).await;
        assert_eq!(st, 200);
        assert_eq!(h["content-type"], "text/event-stream");
        assert_eq!(h["x-cc-session"], "sess-1");
        let datas: Vec<&str> = text
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .collect();
        assert_eq!(*datas.last().unwrap(), "[DONE]");
        let chunks: Vec<Value> = datas[..datas.len() - 1]
            .iter()
            .map(|d| serde_json::from_str(d).unwrap())
            .collect();
        assert!(
            chunks
                .iter()
                .all(|c| c["object"] == "chat.completion.chunk")
        );
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        let content: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert_eq!(content, "four");
        let last = chunks.last().unwrap();
        assert_eq!(last["choices"][0]["finish_reason"], "stop");
        assert_eq!(last["usage"]["completion_tokens"], 2);
    }

    #[tokio::test]
    async fn session_header_resumes_and_sends_last_message_only() {
        let f = fixture("", 4).await;
        // not owned yet -> 404 without running claude
        let (st, _, _) = post(
            &f,
            "/v1/chat/completions",
            Some(&f.key),
            Some("sess-1"),
            hi(),
        )
        .await;
        assert_eq!(st, 404);
        // a fresh run creates sess-1 and records this key as its owner
        let (st, _, _) = post(&f, "/v1/chat/completions", Some(&f.key), None, hi()).await;
        assert_eq!(st, 200);
        assert_eq!(f.owners.owner("sess-1").as_deref(), Some("key:laptop"));
        let body = json!({"messages": [
            {"role": "user", "content": "first"},
            {"role": "assistant", "content": "ok"},
            {"role": "user", "content": "second"},
        ]});
        let (st, h, _) = post(
            &f,
            "/v1/chat/completions",
            Some(&f.key),
            Some("sess-1"),
            body.clone(),
        )
        .await;
        assert_eq!(st, 200);
        assert_eq!(h["x-cc-session"], "sess-1");
        let args = std::fs::read_to_string(&f.args).unwrap();
        let lines: Vec<&str> = args.lines().collect();
        let i = lines.iter().position(|l| *l == "--resume").unwrap();
        assert_eq!(lines[i + 1], "sess-1");
        assert_eq!(*lines.last().unwrap(), "second");
        // another key, or a Telegram chat's session, can't be resumed
        let (st, _, text) = post(
            &f,
            "/v1/runs",
            Some(&f.other_key),
            None,
            json!({"prompt": "x", "session_id": "sess-1"}),
        )
        .await;
        assert_eq!(st, 404, "{text}");
        f.owners.claim("tg-session", "tg:42");
        let (st, _, _) = post(
            &f,
            "/v1/chat/completions",
            Some(&f.key),
            Some("tg-session"),
            hi(),
        )
        .await;
        assert_eq!(st, 404);
        // unknown session -> 404 in OpenAI error shape
        let (st, _, text) = post(
            &f,
            "/v1/chat/completions",
            Some(&f.key),
            Some("bogus-session"),
            body,
        )
        .await;
        assert_eq!(st, 404, "{text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["error"]["type"], "session_not_found");
        let (st, _, _) = post(
            &f,
            "/v1/chat/completions",
            Some(&f.key),
            Some("bad id!"),
            hi(),
        )
        .await;
        assert_eq!(st, 400);
    }

    #[tokio::test]
    async fn concurrency_cap_returns_429() {
        let f = fixture("sleep 2", 1).await;
        let slow = {
            let (base, key) = (f.base.clone(), f.key.clone());
            tokio::spawn(async move {
                reqwest::Client::new()
                    .post(format!("{base}/v1/runs"))
                    .header("authorization", format!("Bearer {key}"))
                    .body(json!({"prompt": "slow"}).to_string())
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (st, _, text) = post(&f, "/v1/chat/completions", Some(&f.key), None, hi()).await;
        assert_eq!(st, 429);
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["error"]["type"], "rate_limit_error");
        assert_eq!(slow.await.unwrap(), 200);
    }

    #[tokio::test]
    async fn native_runs_json_sse_and_stop() {
        let f = fixture("", 4).await;
        let (st, h, text) = post(&f, "/v1/runs", Some(&f.key), None, json!({"prompt": "go"})).await;
        assert_eq!(st, 200);
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            (v["text"].as_str(), v["session_id"].as_str()),
            (Some("four"), Some("sess-1"))
        );
        assert_eq!(v["run_id"], h["x-cc-run"].to_str().unwrap());

        let (_, h, text) = post(
            &f,
            "/v1/runs",
            Some(&f.key),
            None,
            json!({"prompt": "go", "stream": true}),
        )
        .await;
        assert_eq!(h["x-cc-session"], "sess-1");
        let events: Vec<&str> = text
            .lines()
            .filter_map(|l| l.strip_prefix("event: "))
            .collect();
        assert_eq!(events, vec!["init", "tool", "text", "result"]);
        assert!(text.contains(r#""name":"Bash""#) && text.contains(r#""input":"ls""#));

        let (st, _, _) = post(&f, "/v1/runs", Some(&f.key), None, json!({})).await;
        assert_eq!(st, 400);
        let r = reqwest::Client::new()
            .delete(format!("{}/v1/runs/nope", f.base))
            .header("authorization", format!("Bearer {}", f.key))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 404);
    }

    #[tokio::test]
    async fn delete_stops_a_streaming_run() {
        let f = fixture("sleep 30", 4).await;
        let mut r = reqwest::Client::new()
            .post(format!("{}/v1/runs", f.base))
            .header("authorization", format!("Bearer {}", f.key))
            .body(json!({"prompt": "slow", "stream": true}).to_string())
            .send()
            .await
            .unwrap();
        let mut text = String::new();
        while !text.contains("\n\n") {
            text.push_str(&String::from_utf8_lossy(&r.chunk().await.unwrap().unwrap()));
        }
        let data = text.lines().find_map(|l| l.strip_prefix("data: ")).unwrap();
        let run_id = serde_json::from_str::<Value>(data).unwrap()["run_id"]
            .as_str()
            .unwrap()
            .to_string();
        let del = |key: String| {
            reqwest::Client::new()
                .delete(format!("{}/v1/runs/{run_id}", f.base))
                .header("authorization", format!("Bearer {key}"))
                .send()
        };
        assert_eq!(
            del("ccg_other".into()).await.unwrap().status().as_u16(),
            401
        );
        assert_eq!(del(f.key.clone()).await.unwrap().status().as_u16(), 200);
        let t = std::time::Instant::now();
        while let Some(c) = r.chunk().await.unwrap() {
            text.push_str(&String::from_utf8_lossy(&c));
        }
        assert!(t.elapsed() < Duration::from_secs(5));
        assert!(
            text.contains("event: error") && text.contains("cancelled"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn errors_are_openai_shaped_and_bearer_is_case_insensitive() {
        let f = fixture("", 4).await;
        let c = reqwest::Client::new();
        let kind =
            |text: String| serde_json::from_str::<Value>(&text).unwrap()["error"]["type"].clone();
        let r = c.get(format!("{}/nope", f.base)).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 404);
        assert_eq!(kind(r.text().await.unwrap()), "not_found");
        let r = c.get(format!("{}/v1/runs", f.base)).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 405);
        assert_eq!(kind(r.text().await.unwrap()), "invalid_request_error");
        let big = "x".repeat(BODY_LIMIT + 1);
        let r = c
            .post(format!("{}/v1/runs", f.base))
            .header("authorization", format!("bearer {}", f.key))
            .body(big.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 413);
        assert_eq!(kind(r.text().await.unwrap()), "invalid_request_error");
        // without a key the body is never read: 401, not 413
        let r = c.post(format!("{}/v1/runs", f.base)).body(big).send().await;
        if let Ok(r) = r {
            assert_eq!(r.status().as_u16(), 401);
        }
        let r = c
            .get(format!("{}/v1/models", f.base))
            .header("authorization", format!("BEARER {}", f.key))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn request_images_are_deleted_after_the_run() {
        let f = fixture("", 4).await;
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "what is this?"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGk="}},
        ]}]});
        let (st, _, _) = post(&f, "/v1/chat/completions", Some(&f.key), None, body).await;
        assert_eq!(st, 200);
        let args = std::fs::read_to_string(&f.args).unwrap();
        assert!(args.contains("[Attached image: "));
        let inbox = f.profile.workspace().join("inbox/key-laptop");
        assert_eq!(std::fs::read_dir(inbox).unwrap().count(), 0);
    }
}
