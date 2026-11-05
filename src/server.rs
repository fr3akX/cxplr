//! Local HTTP API and embedded web UI. Explorations and questions run as
//! background jobs that the UI polls; source navigation goes through LSP.

use crate::explore::{Explorer, Lineage};
use crate::lsp::Lsp;
use crate::{ask, atlas, search, trace, verify};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::net::TcpListener;
use tokio::sync::Notify;

const MAX_PROGRESS: usize = 5000;
const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024;

pub struct AppState {
    ex: Arc<Explorer>,
    lsp: Lsp,
    jobs: Mutex<Jobs>,
}

/// Handlers use the explorer's repository, cache and engine sessions directly.
impl std::ops::Deref for AppState {
    type Target = Explorer;
    fn deref(&self) -> &Explorer {
        &self.ex
    }
}

#[derive(Default)]
struct Jobs {
    next_id: u64,
    by_id: HashMap<u64, Arc<Job>>,
    /// Running exploration per scope, so concurrent requests share one run.
    running: HashMap<String, u64>,
}

struct Job {
    id: u64,
    /// `explore` or `ask`.
    kind: &'static str,
    scope: String,
    cancel: Notify,
    started: Instant,
    inner: Mutex<JobInner>,
}

struct JobInner {
    status: &'static str,
    progress: Vec<String>,
    error: Option<String>,
    result: Option<Value>,
}

impl Job {
    fn push(&self, line: String) {
        let mut inner = self.inner.lock().unwrap();
        if inner.progress.len() < MAX_PROGRESS {
            inner.progress.push(line);
        }
    }
}

impl AppState {
    pub fn new(ex: Arc<Explorer>, lsp: Lsp) -> Arc<AppState> {
        Arc::new(AppState { ex, lsp, jobs: Mutex::default() })
    }

    fn running_job(&self, scope: &str) -> Option<u64> {
        self.jobs.lock().unwrap().running.get(scope).copied()
    }

    /// Register a job; the caller holds `jobs` so dedupe checks and insertion are atomic.
    fn new_job(jobs: &mut Jobs, kind: &'static str, scope: String) -> Arc<Job> {
        jobs.next_id += 1;
        let job = Arc::new(Job {
            id: jobs.next_id,
            kind,
            scope,
            cancel: Notify::new(),
            started: Instant::now(),
            inner: Mutex::new(JobInner { status: "running", progress: Vec::new(), error: None, result: None }),
        });
        jobs.by_id.insert(job.id, job.clone());
        job
    }

    fn finish(&self, job: &Job, result: Result<Option<Value>, String>) {
        {
            let mut inner = job.inner.lock().unwrap();
            match result {
                Ok(value) => {
                    inner.status = "done";
                    inner.result = value;
                }
                Err(e) if e == "cancelled" => inner.status = "cancelled",
                Err(e) => {
                    inner.status = "failed";
                    inner.error = Some(e);
                }
            }
        }
        if job.kind != "ask" {
            self.jobs.lock().unwrap().running.remove(&job.scope);
        }
    }
}

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn fail(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": msg.into() })))
}

pub async fn serve(listener: TcpListener, state: Arc<AppState>) -> std::io::Result<()> {
    let app = Router::new()
        .route("/", get(|| asset("text/html; charset=utf-8", include_str!("../web/index.html"))))
        .route("/app.js", get(|| asset("text/javascript; charset=utf-8", include_str!("../web/app.js"))))
        .route("/style.css", get(|| asset("text/css; charset=utf-8", include_str!("../web/style.css"))))
        .route("/vendor/highlight.js", get(|| asset("text/javascript; charset=utf-8", include_str!("../web/vendor/highlight.js"))))
        .route("/vendor/highlight.css", get(|| asset("text/css; charset=utf-8", include_str!("../web/vendor/highlight.css"))))
        .route("/api/repo", get(repo_info))
        .route("/api/map", get(get_map))
        .route("/api/explore", post(explore))
        .route("/api/ask", post(ask_question))
        .route("/api/trace", get(get_trace).post(start_trace))
        .route("/api/job/{id}", get(job_status))
        .route("/api/job/{id}/cancel", post(cancel_job))
        .route("/api/source", get(source))
        .route("/api/search", get(text_search))
        .route("/api/lsp/status", get(lsp_status))
        .route("/api/lsp/symbol", post(lsp_symbol))
        .route("/api/lsp/references", post(lsp_references))
        .route("/api/lsp/outline", get(lsp_outline))
        .with_state(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}

async fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

async fn repo_info(State(st): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "name": st.repo.name,
        "root": st.repo.root,
        "commit": st.repo.head(),
        "engine": st.engine.kind().id(),
        "model": st.engine.model(),
        "cache": st.cache.dir(),
    }))
}

#[derive(Deserialize)]
struct ScopeQuery {
    #[serde(default)]
    scope: String,
}

/// Cached map (fresh or stale) plus which of its nodes already have a cached map.
async fn get_map(State(st): State<Arc<AppState>>, Query(q): Query<ScopeQuery>) -> Json<Value> {
    if let Some(job) = st.running_job(&q.scope) {
        return Json(json!({ "status": "running", "job": job }));
    }
    let Some(cached) = st.cache.load(&st.repo, &q.scope) else {
        return Json(json!({ "status": "missing" }));
    };
    let children: serde_json::Map<String, Value> = cached
        .map
        .nodes
        .iter()
        .filter(|n| !n.paths.is_empty())
        .filter_map(|n| {
            let child = if q.scope.is_empty() { n.id.clone() } else { format!("{}/{}", q.scope, n.id) };
            st.cache.state(&st.repo, &child).map(|s| (n.id.clone(), json!(s)))
        })
        .collect();
    Json(json!({ "status": "ready", "map": cached.map, "freshness": cached.freshness, "children": children }))
}

#[derive(Deserialize)]
struct ExploreRequest {
    #[serde(default)]
    scope: String,
    #[serde(default)]
    force: bool,
}

async fn explore(State(st): State<Arc<AppState>>, Json(req): Json<ExploreRequest>) -> ApiResult {
    let lineage = if req.scope.is_empty() {
        None
    } else {
        Some(st.lineage(&req.scope).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?)
    };
    let job = {
        let mut jobs = st.jobs.lock().unwrap();
        if let Some(&id) = jobs.running.get(&req.scope) {
            return Ok(Json(json!({ "status": "running", "job": id })));
        }
        if !req.force && st.cache.state(&st.repo, &req.scope).is_some() {
            return Ok(Json(json!({ "status": "ready" })));
        }
        let job = AppState::new_job(&mut jobs, "explore", req.scope.clone());
        jobs.running.insert(job.scope.clone(), job.id);
        job
    };
    let id = job.id;
    tokio::spawn(run_job(st, job, lineage));
    Ok(Json(json!({ "status": "running", "job": id })))
}

async fn run_job(st: Arc<AppState>, job: Arc<Job>, lineage: Option<Lineage>) {
    let result = st.ex.explore(&job.scope, lineage.as_ref(), &job.cancel, |line| job.push(line)).await;
    if result.is_ok() {
        let ex = st.ex.clone();
        match tokio::task::spawn_blocking(move || atlas::rebuild(&ex.repo, &ex.cache)).await {
            Ok(Ok(pages)) => job.push(format!("Atlas updated ({pages} pages)")),
            Ok(Err(e)) => job.push(format!("! atlas not updated: {e}")),
            Err(e) => job.push(format!("! atlas not updated: {e}")),
        }
    }
    st.finish(&job, result.map(|_| None));
}

#[derive(Deserialize)]
struct SinceQuery {
    #[serde(default)]
    since: usize,
}

async fn job_status(
    State(st): State<Arc<AppState>>,
    UrlPath(id): UrlPath<u64>,
    Query(q): Query<SinceQuery>,
) -> ApiResult {
    let job = st.jobs.lock().unwrap().by_id.get(&id).cloned();
    let job = job.ok_or_else(|| fail(StatusCode::NOT_FOUND, "unknown job"))?;
    let inner = job.inner.lock().unwrap();
    let from = q.since.min(inner.progress.len());
    Ok(Json(json!({
        "kind": job.kind,
        "scope": job.scope,
        "status": inner.status,
        "error": inner.error,
        "result": inner.result,
        "progress": inner.progress[from..],
        "next": inner.progress.len(),
        "elapsed_ms": job.started.elapsed().as_millis() as u64,
    })))
}

async fn ask_question(State(st): State<Arc<AppState>>, Json(req): Json<ask::Request>) -> ApiResult {
    if req.question.trim().is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "the question is empty"));
    }
    let selection = match &req.context.path {
        None => None,
        Some(path) => {
            let (full, shown) = verify::resolve(&st.repo.root, path).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            if full.is_dir() {
                None
            } else {
                let bytes = fs::read(&full).map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{shown}: {e}")))?;
                let (first, text) =
                    ask::selection(&String::from_utf8_lossy(&bytes), req.context.start_line, req.context.end_line);
                Some((shown, first, text))
            }
        }
    };
    let job = AppState::new_job(&mut st.jobs.lock().unwrap(), "ask", req.context.scope.clone());
    let id = job.id;
    tokio::spawn(async move {
        let result = answer_question(&st, &job, req, selection).await;
        st.finish(&job, result.map(Some));
    });
    Ok(Json(json!({ "status": "running", "job": id })))
}

async fn answer_question(
    st: &AppState,
    job: &Job,
    req: ask::Request,
    selection: Option<(String, usize, String)>,
) -> Result<Value, String> {
    let map = st.cache.load(&st.repo, &req.context.scope).map(|c| c.map);
    let node = map.as_ref().zip(req.context.node.as_ref()).and_then(|(m, id)| m.nodes.iter().find(|n| &n.id == id));
    let crumbs = map.as_ref().and_then(|m| m.meta.as_ref()).map_or_else(|| vec![st.repo.name.clone()], |m| m.crumbs.clone());
    let mut progress = |line: String| job.push(line);
    // Follow-ups continue the conversation's session; a new conversation forks the
    // session of the map being viewed, which has already read that part of the code.
    let (session, follow_up) = match req.session.clone() {
        Some(session) => {
            progress(format!("Continuing conversation in session {session}"));
            (session, true)
        }
        None => {
            let name = st.session_name(&format!("ask · {}", crumbs.join(" › ")));
            (st.session_from(Some(&req.context.scope), &name, &mut progress).await?, false)
        }
    };
    let view = ask::View { crumbs, map: map.as_ref(), node, selection };
    let prompt = ask::prompt(&st.repo.name, &view, &req.question, follow_up);
    let started = Instant::now();
    let out = st.engine.turn::<ask::Answer>(&session, &prompt, &ask::schema(), &job.cancel, &mut progress).await?;
    let mut answer = out.answer;
    verify::verify_citations(&st.repo.root, &mut answer.citations);
    Ok(json!({
        "answer": answer.answer,
        "citations": answer.citations,
        "session": session,
        "duration_ms": started.elapsed().as_millis() as u64,
        "commands": out.commands,
        "input_tokens": out.input_tokens,
        "cached_input_tokens": out.cached_input_tokens,
        "output_tokens": out.output_tokens,
    }))
}

#[derive(Deserialize)]
struct TraceRequest {
    #[serde(default)]
    scope: String,
    /// Index into the scope's map flows.
    flow: usize,
}

/// The map, chosen flow and cache key for a trace request.
fn trace_target(st: &AppState, req: &TraceRequest) -> Result<(crate::map::Map, usize, String), (StatusCode, Json<Value>)> {
    let map = st.cache.load(&st.repo, &req.scope).ok_or_else(|| fail(StatusCode::NOT_FOUND, "this map is not cached"))?.map;
    let flow = map.flows.get(req.flow).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such flow"))?;
    let key = st.cache.trace_key(&st.repo, &req.scope, &flow.name).ok_or_else(|| fail(StatusCode::NOT_FOUND, "this map is not cached"))?;
    Ok((map, req.flow, format!("trace:{key}")))
}

async fn get_trace(State(st): State<Arc<AppState>>, Query(req): Query<TraceRequest>) -> ApiResult {
    let (_, _, job_key) = trace_target(&st, &req)?;
    if let Some(job) = st.running_job(&job_key) {
        return Ok(Json(json!({ "status": "running", "job": job })));
    }
    Ok(Json(match st.cache.load_trace(&job_key["trace:".len()..]) {
        Some(traces) => json!({ "status": "ready", "traces": traces }),
        None => json!({ "status": "missing" }),
    }))
}

async fn start_trace(State(st): State<Arc<AppState>>, Json(req): Json<TraceRequest>) -> ApiResult {
    let (map, flow, job_key) = trace_target(&st, &req)?;
    let scope = req.scope.clone();
    let job = {
        let mut jobs = st.jobs.lock().unwrap();
        if let Some(&id) = jobs.running.get(&job_key) {
            return Ok(Json(json!({ "status": "running", "job": id })));
        }
        let job = AppState::new_job(&mut jobs, "trace", job_key.clone());
        jobs.running.insert(job_key, job.id);
        job
    };
    let id = job.id;
    tokio::spawn(async move {
        let result = generate_trace(&st, &job, &scope, &map, flow).await;
        st.finish(&job, result.map(Some));
    });
    Ok(Json(json!({ "status": "running", "job": id })))
}

async fn generate_trace(st: &AppState, job: &Job, scope: &str, map: &crate::map::Map, flow: usize) -> Result<Value, String> {
    let flow = &map.flows[flow];
    let mut progress = |line: String| job.push(line);
    progress(format!("Tracing example inputs through “{}” with {} (read-only)", flow.name, st.engine.kind().id()));
    let name = st.session_name(&format!("examples · {}", flow.name));
    let session = st.session_from(Some(scope), &name, &mut progress).await?;
    let started = Instant::now();
    let prompt = trace::prompt(map, flow);
    let out = st.engine.turn::<trace::Traces>(&session, &prompt, &trace::schema(), &job.cancel, &mut progress).await?;
    let mut traces = out.answer;
    traces.diagnostics.clear();
    trace::normalize(&mut traces, map);
    for step in traces.scenarios.iter_mut().flat_map(|s| s.steps.iter_mut()) {
        verify::verify_citations(&st.repo.root, &mut step.evidence);
    }
    traces.meta = Some(trace::TraceMeta {
        flow: flow.name.clone(),
        generated_at: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        duration_ms: started.elapsed().as_millis() as u64,
        commands: out.commands,
        input_tokens: out.input_tokens,
        output_tokens: out.output_tokens,
    });
    st.cache.store_trace(&job.scope["trace:".len()..], &traces).map_err(|e| format!("saving traces: {e}"))?;
    serde_json::to_value(&traces).map_err(|e| e.to_string())
}

async fn cancel_job(State(st): State<Arc<AppState>>, UrlPath(id): UrlPath<u64>) -> ApiResult {
    let job = st.jobs.lock().unwrap().by_id.get(&id).cloned();
    let job = job.ok_or_else(|| fail(StatusCode::NOT_FOUND, "unknown job"))?;
    // notify_one stores a permit, so a cancel racing the agent start is not lost.
    job.cancel.notify_one();
    Ok(Json(json!({ "status": "cancelling" })))
}

#[derive(Deserialize)]
struct SourceQuery {
    path: String,
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    /// Whole identifiers only (case-sensitive) instead of smart-case substrings.
    #[serde(default)]
    word: bool,
    #[serde(default = "default_search_limit")]
    limit: usize,
}

fn default_search_limit() -> usize {
    300
}

async fn text_search(State(st): State<Arc<AppState>>, Query(q): Query<SearchQuery>) -> ApiResult {
    let text = if q.word { q.q.trim() } else { q.q.as_str() }.to_owned();
    if text.trim().is_empty() || text.len() > 200 {
        return Err(fail(StatusCode::BAD_REQUEST, "search for 1 to 200 characters"));
    }
    if q.word && text.contains(char::is_whitespace) {
        return Err(fail(StatusCode::BAD_REQUEST, "search for a single word"));
    }
    let query = if q.word { search::Query::Word(text) } else { search::Query::Text(text) };
    let root = st.repo.root.clone();
    let limit = q.limit.clamp(1, 1000);
    let results = tokio::task::spawn_blocking(move || search::search(&root, &query, limit))
        .await
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!(results)))
}

#[derive(Deserialize)]
struct StatusQuery {
    path: String,
    /// Start the server (and open the file) in the background if it is not running.
    #[serde(default)]
    start: bool,
}

async fn lsp_status(State(st): State<Arc<AppState>>, Query(q): Query<StatusQuery>) -> ApiResult {
    let (full, shown) = verify::resolve(&st.repo.root, &q.path).map_err(|e| fail(StatusCode::NOT_FOUND, e))?;
    let status = st.lsp.status(&shown).await;
    if q.start && status.state == "idle" {
        let st = st.clone();
        tokio::spawn(async move { st.lsp.warm(&full, &shown).await });
    }
    Ok(Json(json!(status)))
}

#[derive(Deserialize)]
struct PositionRequest {
    path: String,
    /// 1-based line.
    line: u64,
    /// 0-based UTF-16 column.
    character: u64,
}

async fn lsp_symbol(State(st): State<Arc<AppState>>, Json(req): Json<PositionRequest>) -> ApiResult {
    let (full, shown) = verify::resolve(&st.repo.root, &req.path).map_err(|e| fail(StatusCode::NOT_FOUND, e))?;
    let info = st.lsp.symbol(&full, &shown, req.line, req.character).await;
    info.map(|i| Json(json!(i))).map_err(|e| fail(StatusCode::BAD_GATEWAY, e))
}

async fn lsp_references(State(st): State<Arc<AppState>>, Json(req): Json<PositionRequest>) -> ApiResult {
    let (full, shown) = verify::resolve(&st.repo.root, &req.path).map_err(|e| fail(StatusCode::NOT_FOUND, e))?;
    let refs = st.lsp.references(&full, &shown, req.line, req.character).await;
    refs.map(|r| Json(json!({ "references": r }))).map_err(|e| fail(StatusCode::BAD_GATEWAY, e))
}

/// Functions, classes and other declarations in a file, from its language server.
async fn lsp_outline(State(st): State<Arc<AppState>>, Query(q): Query<SourceQuery>) -> ApiResult {
    let (full, shown) = verify::resolve(&st.repo.root, &q.path).map_err(|e| fail(StatusCode::NOT_FOUND, e))?;
    let items = st.lsp.outline(&full, &shown).await;
    items.map(|i| Json(json!({ "symbols": i }))).map_err(|e| fail(StatusCode::BAD_GATEWAY, e))
}

/// L3: file contents or a directory listing, confined to the repository.
async fn source(State(st): State<Arc<AppState>>, Query(q): Query<SourceQuery>) -> ApiResult {
    let (full, shown) = verify::resolve(&st.repo.root, &q.path).map_err(|e| fail(StatusCode::NOT_FOUND, e))?;
    let io = |e: std::io::Error| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    let meta = fs::metadata(&full).map_err(io)?;
    if meta.is_dir() {
        let mut entries: Vec<(bool, String)> = fs::read_dir(&full)
            .map_err(io)?
            .filter_map(Result::ok)
            .filter(|e| !crate::repo::is_tool_dir(&e.file_name()))
            .map(|e| (e.path().is_dir(), e.file_name().to_string_lossy().into_owned()))
            .collect();
        entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let entries: Vec<Value> = entries.into_iter().map(|(dir, name)| json!({ "name": name, "dir": dir })).collect();
        return Ok(Json(json!({ "path": shown, "dir": true, "entries": entries })));
    }
    if meta.len() > MAX_SOURCE_BYTES {
        return Err(fail(StatusCode::PAYLOAD_TOO_LARGE, format!("{shown} is larger than 2 MiB")));
    }
    let bytes = fs::read(&full).map_err(io)?;
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return Err(fail(StatusCode::UNSUPPORTED_MEDIA_TYPE, format!("{shown} is a binary file")));
    }
    Ok(Json(json!({ "path": shown, "dir": false, "text": String::from_utf8_lossy(&bytes) })))
}
