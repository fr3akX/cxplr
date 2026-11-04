//! Local HTTP API and embedded web UI. Exploration runs as background jobs;
//! the UI polls their progress.

use crate::agent::{self, AgentConfig};
use crate::cache::Cache;
use crate::map::{self, Map, Meta, Node};
use crate::repo::Repo;
use crate::{prompt, verify};
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
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::sync::Notify;

const MAX_PROGRESS: usize = 5000;
const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024;

pub struct AppState {
    repo: Repo,
    cache: Cache,
    agent: AgentConfig,
    jobs: Mutex<Jobs>,
}

#[derive(Default)]
struct Jobs {
    next_id: u64,
    by_id: HashMap<u64, Arc<Job>>,
    /// Running job per scope, so concurrent requests share one exploration.
    running: HashMap<String, u64>,
}

struct Job {
    id: u64,
    scope: String,
    cancel: Notify,
    started: Instant,
    inner: Mutex<JobInner>,
}

struct JobInner {
    status: &'static str,
    progress: Vec<String>,
    error: Option<String>,
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
    pub fn new(repo: Repo, cache: Cache, agent: AgentConfig) -> Arc<AppState> {
        Arc::new(AppState { repo, cache, agent, jobs: Mutex::default() })
    }

    fn running_job(&self, scope: &str) -> Option<u64> {
        self.jobs.lock().unwrap().running.get(scope).copied()
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
        .route("/api/repo", get(repo_info))
        .route("/api/map", get(get_map))
        .route("/api/explore", post(explore))
        .route("/api/job/{id}", get(job_status))
        .route("/api/job/{id}/cancel", post(cancel_job))
        .route("/api/source", get(source))
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
        "commit": st.repo.commit,
        "model": st.agent.model,
    }))
}

#[derive(Deserialize)]
struct ScopeQuery {
    #[serde(default)]
    scope: String,
}

async fn get_map(State(st): State<Arc<AppState>>, Query(q): Query<ScopeQuery>) -> Json<Value> {
    if let Some(job) = st.running_job(&q.scope) {
        return Json(json!({ "status": "running", "job": job }));
    }
    match st.cache.load(&q.scope) {
        Some(map) => Json(json!({ "status": "ready", "map": map })),
        None => Json(json!({ "status": "missing" })),
    }
}

#[derive(Deserialize)]
struct ExploreRequest {
    #[serde(default)]
    scope: String,
    #[serde(default)]
    force: bool,
}

/// Everything needed to explore one scope below the system map.
struct Lineage {
    crumbs: Vec<(String, String)>,
    parent: Map,
    node: Node,
}

fn lineage(cache: &Cache, scope: &str) -> Result<Lineage, String> {
    let ids: Vec<&str> = scope.split('/').collect();
    if ids.iter().any(|id| id.is_empty()) {
        return Err(format!("invalid scope `{scope}`"));
    }
    let mut map = cache.load("").ok_or("the system map has not been explored yet")?;
    let mut crumbs = vec![(map.title.clone(), map.summary.clone())];
    for (depth, id) in ids.iter().enumerate() {
        let node = map
            .nodes
            .iter()
            .find(|n| n.id == *id)
            .cloned()
            .ok_or_else(|| format!("no node `{id}` in the map above it"))?;
        crumbs.push((node.name.clone(), node.summary.clone()));
        if depth + 1 == ids.len() {
            if node.paths.is_empty() {
                return Err(format!("`{}` has no repository paths to explore", node.name));
            }
            return Ok(Lineage { crumbs, parent: map, node });
        }
        let above = ids[..=depth].join("/");
        map = cache.load(&above).ok_or_else(|| format!("`{}` has not been explored yet", node.name))?;
    }
    unreachable!("scope has at least one id")
}

async fn explore(State(st): State<Arc<AppState>>, Json(req): Json<ExploreRequest>) -> ApiResult {
    let lineage = if req.scope.is_empty() {
        None
    } else {
        Some(lineage(&st.cache, &req.scope).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?)
    };
    let job = {
        let mut jobs = st.jobs.lock().unwrap();
        if let Some(&id) = jobs.running.get(&req.scope) {
            return Ok(Json(json!({ "status": "running", "job": id })));
        }
        if !req.force && st.cache.load(&req.scope).is_some() {
            return Ok(Json(json!({ "status": "ready" })));
        }
        jobs.next_id += 1;
        let job = Arc::new(Job {
            id: jobs.next_id,
            scope: req.scope.clone(),
            cancel: Notify::new(),
            started: Instant::now(),
            inner: Mutex::new(JobInner { status: "running", progress: Vec::new(), error: None }),
        });
        jobs.by_id.insert(job.id, job.clone());
        jobs.running.insert(job.scope.clone(), job.id);
        job
    };
    let id = job.id;
    tokio::spawn(run_job(st, job, lineage));
    Ok(Json(json!({ "status": "running", "job": id })))
}

async fn run_job(st: Arc<AppState>, job: Arc<Job>, lineage: Option<Lineage>) {
    let result = explore_scope(&st, &job, lineage).await;
    {
        let mut inner = job.inner.lock().unwrap();
        match result {
            Ok(()) => inner.status = "done",
            Err(e) if e == "cancelled" => inner.status = "cancelled",
            Err(e) => {
                inner.status = "failed";
                inner.error = Some(e);
            }
        }
    }
    st.jobs.lock().unwrap().running.remove(&job.scope);
}

async fn explore_scope(st: &AppState, job: &Job, lineage: Option<Lineage>) -> Result<(), String> {
    let (prompt, mut crumbs) = match &lineage {
        None => (prompt::system(&st.repo.name), vec![st.repo.name.clone()]),
        Some(l) => (
            prompt::component(&l.crumbs, &l.parent, &l.node),
            l.crumbs.iter().map(|(name, _)| name.clone()).collect(),
        ),
    };
    job.push(format!("Exploring {} with codex (read-only sandbox)", crumbs.join(" › ")));
    let started = Instant::now();
    let out = agent::explore(&st.agent, &st.repo.root, &prompt, &job.cancel, |line| job.push(line)).await?;

    job.push("Verifying citations against the repository".into());
    let mut map = out.map;
    map.diagnostics.clear();
    map::normalize(&mut map);
    verify::verify(&st.repo.root, &mut map);
    if lineage.is_none() && !map.title.trim().is_empty() {
        crumbs[0] = map.title.clone();
    }
    map.meta = Some(Meta {
        scope: job.scope.clone(),
        crumbs,
        generated_at: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        commit: st.repo.commit.clone(),
        dirty: st.repo.is_dirty(),
        model: st.agent.model.clone(),
        duration_ms: started.elapsed().as_millis() as u64,
        commands: out.commands,
        input_tokens: out.input_tokens,
        output_tokens: out.output_tokens,
    });
    st.cache.store(&job.scope, &map).map_err(|e| format!("saving map: {e}"))
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
        "scope": job.scope,
        "status": inner.status,
        "error": inner.error,
        "progress": inner.progress[from..],
        "next": inner.progress.len(),
        "elapsed_ms": job.started.elapsed().as_millis() as u64,
    })))
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

/// L3: file contents or a directory listing, confined to the repository.
async fn source(State(st): State<Arc<AppState>>, Query(q): Query<SourceQuery>) -> ApiResult {
    let (full, shown) = verify::resolve(&st.repo.root, &q.path).map_err(|e| fail(StatusCode::NOT_FOUND, e))?;
    let io = |e: std::io::Error| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    let meta = fs::metadata(&full).map_err(io)?;
    if meta.is_dir() {
        let mut entries: Vec<(bool, String)> = fs::read_dir(&full)
            .map_err(io)?
            .filter_map(Result::ok)
            .map(|e| (e.path().is_dir(), e.file_name().to_string_lossy().into_owned()))
            .filter(|(_, name)| name != ".git")
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
