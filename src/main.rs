mod admin;
mod auth;
mod config;
mod error;
mod git_http;
mod repo;

use crate::auth::{
    TokensFile, extract_token, load_tokens_file, log_auth_failure, save_tokens_file,
    token_allowed_for_project,
};
use crate::config::Config;
use crate::git_http::run_git_http_backend;
use crate::repo::ensure_repo;
use axum::body::to_bytes;
use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{Request, Response, StatusCode},
    response::IntoResponse,
    routing::{any, delete, get, post},
};
use dashmap::DashMap;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::ErrorKind;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};
use url::form_urlencoded;

/// Shared application state
#[derive(Clone)]
pub struct AppState {
    pub cfg: Config,
    /// Per-project mutexes so we don't race syncs
    pub locks: Arc<DashMap<String, Arc<Mutex<()>>>>,
    /// Global lock for tokens.json read/write
    pub tokens_lock: Arc<Mutex<()>>,
    /// Active admin sessions mapped to expiry instants; keyed by hashed token
    pub admin_sessions: Arc<Mutex<HashMap<String, Instant>>>,
    /// Recent failed admin login attempts for throttling
    pub admin_login_failures: Arc<Mutex<VecDeque<Instant>>>,
}

impl AppState {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            locks: Arc::new(DashMap::new()),
            tokens_lock: Arc::new(Mutex::new(())),
            admin_sessions: Arc::new(Mutex::new(HashMap::new())),
            admin_login_failures: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    fn project_lock(&self, project_id: &str) -> Arc<Mutex<()>> {
        if let Some(entry) = self.locks.get(project_id) {
            return entry.clone();
        }
        let new_lock = Arc::new(Mutex::new(()));
        self.locks
            .entry(project_id.to_string())
            .or_insert_with(|| new_lock.clone())
            .clone()
    }

    pub async fn login_throttle_status(&self) -> Option<Duration> {
        const WINDOW: Duration = Duration::from_secs(60);
        const MAX_ATTEMPTS: usize = 5;

        let mut attempts = self.admin_login_failures.lock().await;
        let now = Instant::now();
        while attempts
            .front()
            .map(|ts| now.duration_since(*ts) > WINDOW)
            .unwrap_or(false)
        {
            attempts.pop_front();
        }
        if attempts.len() >= MAX_ATTEMPTS {
            if let Some(oldest) = attempts.front() {
                let elapsed = now.duration_since(*oldest);
                return WINDOW.checked_sub(elapsed);
            }
        }
        None
    }

    pub async fn note_login_failure(&self) {
        let mut attempts = self.admin_login_failures.lock().await;
        attempts.push_back(Instant::now());
    }

    pub async fn reset_login_failures(&self) {
        let mut attempts = self.admin_login_failures.lock().await;
        attempts.clear();
    }
}

#[tokio::main]
async fn main() {
    // init tracing/logging
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).init();

    let cfg = Config::from_env();
    info!("starting sharelatex-gitbridge-ro on port {}", cfg.port);

    if let Err(e) = init_storage(&cfg) {
        error!("failed to initialize storage: {e}");
        std::process::exit(1);
    }

    cfg.log_summary();

    let state = AppState::new(cfg.clone());
    let router = Router::new()
        // health
        .route("/", get(health))
        // git smart http endpoint
        .route("/git/{*tail}", any(git_handler))
        // admin UI SPA + APIs
        .route("/admin", get(admin::admin_app))
        .route("/admin/api/login", post(admin::admin_login_api))
        .route("/admin/api/logout", post(admin::admin_logout_api))
        .route(
            "/admin/api/tokens",
            get(admin::admin_tokens_api).post(admin::admin_create_token_api),
        )
        .route(
            "/admin/api/tokens/{token}",
            delete(admin::admin_delete_token_api),
        )
        .route("/assets/tailwind.js", get(admin::admin_tailwind_asset))
        .route("/assets/logo.webp", get(admin::admin_logo_asset))
        .route("/assets/favicon.png", get(admin::admin_favicon_asset))
        .route("/favicon.ico", get(admin::admin_favicon_asset))
        .with_state(Arc::new(state));

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], cfg.port));
    axum::serve(
        tokio::net::TcpListener::bind(addr)
            .await
            .expect("bind port"),
        router.into_make_service(),
    )
    .await
    .expect("server crashed");
}

fn init_storage(cfg: &Config) -> Result<(), String> {
    fs::create_dir_all(&cfg.git_root)
        .map_err(|e| format!("cannot create git_root '{}': {e}", cfg.git_root.display()))?;

    let tokens_path = cfg.tokens_file();
    if !tokens_path.exists() {
        let default_tokens = TokensFile::default();
        save_tokens_file(cfg, &default_tokens)
            .map_err(|e| format!("cannot create tokens file '{}': {e}", tokens_path.display()))?;
    }

    if !cfg.sharelatex_data_path.exists() {
        warn!(path = %cfg.sharelatex_data_path.display(), "sharelatex data path does not exist (yet)");
    } else if !cfg.sharelatex_data_path.is_dir() {
        warn!(path = %cfg.sharelatex_data_path.display(), "sharelatex data path is not a directory");
    }

    let projects_root = cfg.sharelatex_data_path.join(&cfg.projects_dir);
    if !projects_root.exists() {
        warn!(path = %projects_root.display(), "projects directory does not exist (will cause 404 until created)");
    }

    Ok(())
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "sharelatex-gitbridge-ro up\n")
}

/// Core handler for Git Smart HTTP.
/// Matches /git/<projectId>.git/... for *any* method, but we only really do GET/POST.
async fn git_handler(
    State(state): State<Arc<AppState>>,
    Path(tail): Path<String>,
    req: Request<Body>,
) -> Response<Body> {
    // tail e.g. "1234567890abcdef.git/info/refs"
    // Extract <projectId>.git as first segment
    let mut segments = tail.splitn(2, '/');
    let first = match segments.next() {
        Some(s) => s,
        None => return response_400("invalid path (no project)"),
    };

    let remaining = segments.next().unwrap_or(""); // may be ""
    let project_id = match first.strip_suffix(".git") {
        Some(id) => id,
        None => return response_400("invalid path (no .git suffix)"),
    };

    // --- Auth ---
    let token_opt = extract_token(&req);

    // Load tokens.json with lock to avoid partial write reads
    let tokens_file = {
        let _guard = state.tokens_lock.lock().await;
        match load_tokens_file(&state.cfg) {
            Ok(tf) => tf,
            Err(e) => {
                error!("cannot load tokens.json: {e}");
                return response_500("internal auth error");
            }
        }
    };

    let mut authed = token_opt
        .as_deref()
        .map(|t| token_allowed_for_project(&tokens_file, t, project_id))
        .unwrap_or(false);

    if !authed {
        if let Some(token) = token_opt.as_deref() {
            if let Some(project_token) = read_project_token(&state.cfg, project_id).await {
                if project_token == token {
                    authed = true;
                }
            }
        }
    }

    if !authed {
        log_auth_failure(&token_opt, project_id);
        return unauthorized_response();
    }

    // --- Sync repo ---
    // ensure only one sync at a time per project
    {
        let lock = state.project_lock(project_id);
        let _guard = lock.lock().await;
        match ensure_repo(state.cfg.clone(), project_id).await {
            Ok(_) => {}
            Err(e) => {
                return match e {
                    crate::error::BridgeError::ProjectNotFound(_) => {
                        response_with_status(StatusCode::NOT_FOUND, "project not found\n")
                    }
                    other => {
                        error!("ensure_repo error: {other}");
                        response_500("repo sync error")
                    }
                };
            }
        }
    }

    // --- Block pushes ---
    // If path is .../git-receive-pack OR query service=git-receive-pack
    if remaining.ends_with("git-receive-pack") || is_receive_pack(req.uri().query()) {
        return response_with_status(StatusCode::FORBIDDEN, "push disabled (read-only)\n");
    }

    // --- Call git http-backend ---
    let method = req.method().clone();
    let query = req.uri().query().map(|s| s.to_string());
    let headers = req.headers().clone();

    let body_bytes = match collect_body(req).await {
        Ok(b) => b,
        Err(_) => return response_500("failed to read request body"),
    };

    let content_type = headers.get("content-type");
    let content_length = headers.get("content-length");

    let backend_res = match run_git_http_backend(
        &state.cfg,
        &format!("{first}/{}", remaining),
        &method,
        query.as_deref(),
        content_type,
        content_length,
        &body_bytes,
    ) {
        Ok(r) => r,
        Err(e) => {
            error!("git http-backend error: {e}");
            return response_500("git backend error");
        }
    };

    let (status, backend_headers, body) = backend_res;
    let mut builder = axum::http::Response::builder().status(status);

    if let Some(headers_mut) = builder.headers_mut() {
        for (name, value) in backend_headers.iter() {
            headers_mut.append(name.clone(), value.clone());
        }
    } else {
        return response_500("failed to build response");
    }

    builder
        .body(Body::from(body))
        .unwrap_or_else(|_| response_500("failed to build response"))
}

/// Drain the request body fully into Bytes.
async fn collect_body(req: Request<Body>) -> Result<Vec<u8>, ()> {
    let (_, body) = req.into_parts();
    let bytes = to_bytes(body, usize::MAX).await.map_err(|_| ())?;
    Ok(bytes.to_vec())
}

/// Quick helper: check if ?service=git-receive-pack
fn is_receive_pack(query: Option<&str>) -> bool {
    if let Some(q) = query {
        for (k, v) in form_urlencoded::parse(q.as_bytes()) {
            if k == "service" && v == "git-receive-pack" {
                return true;
            }
        }
    }
    false
}

/// 401 with WWW-Authenticate header
fn unauthorized_response() -> Response<Body> {
    axum::http::Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("WWW-Authenticate", r#"Basic realm="ShareLatex Git Readonly""#)
        .body(Body::from("Unauthorized\n"))
        .unwrap()
}

fn response_500(msg: &str) -> Response<Body> {
    response_with_status(StatusCode::INTERNAL_SERVER_ERROR, msg)
}

fn response_400(msg: &str) -> Response<Body> {
    response_with_status(StatusCode::BAD_REQUEST, msg)
}

fn response_with_status(code: StatusCode, msg: &str) -> Response<Body> {
    axum::http::Response::builder()
        .status(code)
        .body(Body::from(msg.to_string()))
        .unwrap()
}

async fn read_project_token(cfg: &Config, project_id: &str) -> Option<String> {
    let path = cfg.project_source_dir(project_id).join(".gitbridge");
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "failed to read project token file");
            None
        }
    }
}
