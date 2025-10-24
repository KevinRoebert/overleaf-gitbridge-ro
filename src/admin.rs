use crate::AppState;
use crate::auth::{load_tokens_file, save_tokens_file};
use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, Response, StatusCode, header},
    response::IntoResponse,
};
use hex::encode as hex_encode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::error;
use uuid::Uuid;

const ADMIN_APP_HTML: &str = include_str!("../templates/admin_dashboard.html");
const TAILWIND_CSS: &str = include_str!("../templates/tailwind.js");
const LOGO_WEBP: &[u8] = include_bytes!("../templates/gitbridge.webp");
const FAVICON_PNG: &[u8] = include_bytes!("../templates/favicon-64.png");

#[derive(Deserialize)]
pub struct LoginPayload {
    password: String,
}

#[derive(Deserialize)]
pub struct CreateTokenRequest {
    description: String,
}

#[derive(Serialize)]
struct TokenEntry {
    token: String,
    description: String,
}

fn extract_admin_cookie(headers: &HeaderMap) -> Option<String> {
    for val in headers.get_all("cookie").iter() {
        if let Ok(cookie_line) = val.to_str() {
            for cookie in cookie_line.split(';') {
                let c = cookie.trim();
                if let Some((name, value)) = c.split_once('=') {
                    if name == "gb_admin" && !value.is_empty() {
                        return Some(value.to_string());
                    }
                }
            }
        }
    }
    None
}

fn hash_session_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex_encode(hasher.finalize())
}

async fn has_admin_session(headers: &HeaderMap, app: &AppState) -> bool {
    if app.cfg.admin_password.is_none() {
        return false;
    }

    let token = match extract_admin_cookie(headers) {
        Some(t) => t,
        None => return false,
    };

    let hashed = hash_session_token(&token);
    let now = Instant::now();
    let mut sessions = app.admin_sessions.lock().await;
    if let Some(&expiry) = sessions.get(&hashed) {
        if expiry > now {
            return true;
        }
        sessions.remove(&hashed);
    }
    false
}

fn json_response(
    status: StatusCode,
    value: serde_json::Value,
    set_cookie: Option<String>,
) -> Response<Body> {
    let body = serde_json::to_vec(&value).expect("json serialization");
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(cookie) = set_cookie {
        builder = builder.header(header::SET_COOKIE, cookie);
    }
    builder.body(Body::from(body)).expect("json response")
}

pub async fn admin_app(State(app): State<Arc<AppState>>) -> Response<Body> {
    if app.cfg.admin_password.is_none() {
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(
                "<h1>Admin UI disabled</h1><p>Set ADMIN_PASSWORD env to enable.</p>".to_string(),
            ))
            .expect("disabled admin response");
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(ADMIN_APP_HTML))
        .expect("admin app html")
}

pub async fn admin_login_api(
    State(app): State<Arc<AppState>>,
    Json(payload): Json<LoginPayload>,
) -> Response<Body> {
    if app.cfg.admin_password.is_none() {
        return json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "admin ui disabled"}),
            None,
        );
    }

    if let Some(wait) = app.login_throttle_status().await {
        let seconds = wait.as_secs().max(1);
        return json_response(
            StatusCode::TOO_MANY_REQUESTS,
            json!({
                "error": format!(
                    "Too many login attempts. Try again in {} second{}.",
                    seconds,
                    if seconds == 1 { "" } else { "s" }
                )
            }),
            None,
        );
    }

    let cfg = &app.cfg;
    if let Some(expected) = &cfg.admin_password {
        if expected == &payload.password {
            let raw_token = Uuid::new_v4().to_string();
            let hashed = hash_session_token(&raw_token);
            let ttl = cfg.admin_session_ttl_seconds;
            let expiry = Instant::now() + Duration::from_secs(ttl);
            {
                let mut sessions = app.admin_sessions.lock().await;
                sessions.insert(hashed, expiry);
            }
            app.reset_login_failures().await;

            let mut cookie = format!(
                "gb_admin={raw_token}; HttpOnly; Path=/admin; SameSite=Strict; Max-Age={ttl}"
            );
            if cfg.admin_cookie_secure {
                cookie.push_str("; Secure");
            }

            return json_response(
                StatusCode::OK,
                json!({ "success": true, "ttl": ttl }),
                Some(cookie),
            );
        }
    }

    app.note_login_failure().await;
    json_response(
        StatusCode::UNAUTHORIZED,
        json!({"error": "Invalid password"}),
        None,
    )
}

pub async fn admin_logout_api(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Some(token) = extract_admin_cookie(&headers) {
        let hashed = hash_session_token(&token);
        let mut sessions = app.admin_sessions.lock().await;
        sessions.remove(&hashed);
    }

    let mut cookie = "gb_admin=; HttpOnly; Path=/admin; SameSite=Strict; Max-Age=0".to_string();
    if app.cfg.admin_cookie_secure {
        cookie.push_str("; Secure");
    }

    json_response(StatusCode::OK, json!({ "success": true }), Some(cookie))
}

pub async fn admin_tokens_api(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response<Body> {
    if app.cfg.admin_password.is_none() {
        return json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "admin ui disabled"}),
            None,
        );
    }

    if !has_admin_session(&headers, &app).await {
        return json_response(
            StatusCode::UNAUTHORIZED,
            json!({"error": "unauthorized"}),
            None,
        );
    }

    let entries = {
        let _lock = app.tokens_lock.lock().await;
        match load_tokens_file(&app.cfg) {
            Ok(tf) => {
                let mut items: Vec<TokenEntry> = tf
                    .managed_tokens
                    .iter()
                    .map(|(token, desc)| TokenEntry {
                        token: token.clone(),
                        description: desc.clone(),
                    })
                    .collect();
                items.sort_by(|a, b| a.token.cmp(&b.token));
                items
            }
            Err(e) => {
                error!("cannot load tokens.json: {e}");
                return json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"error": "failed to load tokens"}),
                    None,
                );
            }
        }
    };

    Json(entries).into_response()
}

pub async fn admin_create_token_api(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<CreateTokenRequest>,
) -> Response<Body> {
    if app.cfg.admin_password.is_none() {
        return json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "admin ui disabled"}),
            None,
        );
    }

    if !has_admin_session(&headers, &app).await {
        return json_response(
            StatusCode::UNAUTHORIZED,
            json!({"error": "unauthorized"}),
            None,
        );
    }

    let description = payload.description.trim().to_string();
    let token = Uuid::new_v4().to_string();

    {
        let _lock = app.tokens_lock.lock().await;
        let mut tf = match load_tokens_file(&app.cfg) {
            Ok(tf) => tf,
            Err(e) => {
                error!("load_tokens_file failed in create: {e}");
                return json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"error": "failed to load tokens"}),
                    None,
                );
            }
        };

        tf.managed_tokens.insert(token.clone(), description.clone());

        if let Err(e) = save_tokens_file(&app.cfg, &tf) {
            error!("save_tokens_file failed in create: {e}");
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": "failed to save tokens"}),
                None,
            );
        }
    }

    json_response(
        StatusCode::CREATED,
        json!({
            "token": token,
            "description": description,
        }),
        None,
    )
}

pub async fn admin_delete_token_api(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Response<Body> {
    if app.cfg.admin_password.is_none() {
        return json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "admin ui disabled"}),
            None,
        );
    }

    if !has_admin_session(&headers, &app).await {
        return json_response(
            StatusCode::UNAUTHORIZED,
            json!({"error": "unauthorized"}),
            None,
        );
    }

    {
        let _lock = app.tokens_lock.lock().await;
        let mut tf = match load_tokens_file(&app.cfg) {
            Ok(tf) => tf,
            Err(e) => {
                error!("load_tokens_file failed in delete: {e}");
                return json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"error": "failed to load tokens"}),
                    None,
                );
            }
        };

        tf.managed_tokens.remove(&token);

        if let Err(e) = save_tokens_file(&app.cfg, &tf) {
            error!("save_tokens_file failed in delete: {e}");
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": "failed to save tokens"}),
                None,
            );
        }
    }

    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .expect("delete response")
}

pub async fn admin_tailwind_asset() -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )
        .header(header::CACHE_CONTROL, "public, max-age=86400, immutable")
        .body(Body::from(TAILWIND_CSS))
        .expect("tailwind response")
}

pub async fn admin_logo_asset() -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/webp")
        .header(header::CACHE_CONTROL, "public, max-age=86400, immutable")
        .body(Body::from(LOGO_WEBP.to_vec()))
        .expect("logo response")
}

pub async fn admin_favicon_asset() -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, "public, max-age=86400, immutable")
        .body(Body::from(FAVICON_PNG.to_vec()))
        .expect("favicon response")
}
