use crate::config::Config;
use crate::error::BridgeError;
use axum::body::Body;
use axum::http::{Request, header};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STD;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use tracing::{trace, warn};
use url::form_urlencoded;

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct TokensFile {
    // Managed tokens (global / super rights) with description.
    // { "<token-uuid>": "Description text", ... }
    #[serde(default)]
    pub managed_tokens: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    #[test]
    fn extract_basic_token_username() {
        let req = Request::builder()
            .uri("/git/foo.git/info/refs")
            .header(
                header::AUTHORIZATION,
                "Basic MDc3YjJlMzktYjM0NS00OTVlLWE1YWQtMWU3N2I4NTU3NTcwOmJsYQ==",
            )
            .body(Body::empty())
            .unwrap();
        let token = extract_token(&req);
        assert_eq!(
            token,
            Some("077b2e39-b345-495e-a5ad-1e77b8557570".to_string())
        );
    }
}

pub fn load_tokens_file(cfg: &Config) -> Result<TokensFile, BridgeError> {
    let path = cfg.tokens_file();
    match fs::read_to_string(&path) {
        Ok(data) => {
            let parsed: TokensFile = serde_json::from_str(&data)?;
            Ok(parsed)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // No file yet? Return empty default
            Ok(TokensFile::default())
        }
        Err(e) => Err(BridgeError::Other(format!("cannot read tokens.json: {e}"))),
    }
}

pub fn save_tokens_file(cfg: &Config, tf: &TokensFile) -> Result<(), BridgeError> {
    let serialized = serde_json::to_string_pretty(tf)?;
    let path = cfg.tokens_file();
    // Write atomically-ish: write to temp then rename.
    let tmp_path = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(serialized.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp_path, &path)?;
    Ok(())
}

pub fn token_allowed_for_project(tf: &TokensFile, token: &str, _project_id: &str) -> bool {
    tf.managed_tokens.contains_key(token)
}

/// Extract token from Authorization header (Basic or Bearer) or from `?token=...`
pub fn extract_token(req: &Request<Body>) -> Option<String> {
    // 1. Authorization header
    if let Some(auth_val) = req.headers().get(header::AUTHORIZATION) {
        if let Ok(auth_str) = auth_val.to_str() {
            // Basic <base64(TOKEN:anything)>
            if let Some(rest) = auth_str.strip_prefix("Basic ") {
                if let Ok(decoded) = BASE64_STD.decode(rest.trim()) {
                    if let Ok(decoded_str) = String::from_utf8(decoded) {
                        // Split by first ':'
                        if let Some((token, _rest)) = decoded_str.split_once(':') {
                            return Some(token.to_string());
                        } else {
                            // No colon, treat entire thing as token
                            return Some(decoded_str);
                        }
                    }
                }
            }
            // Bearer TOKEN
            if let Some(rest) = auth_str.strip_prefix("Bearer ") {
                return Some(rest.trim().to_string());
            }
        }
    }

    // 2. Query param ?token=...
    if let Some(q) = req.uri().query() {
        for (k, v) in form_urlencoded::parse(q.as_bytes()) {
            if k == "token" {
                return Some(v.into_owned());
            }
        }
    }

    // nothing
    None
}

/// Quick helper for logging failed auth attempts
pub fn log_auth_failure(token_opt: &Option<String>, project_id: &str) {
    match token_opt {
        Some(token) => warn!(token, project = project_id, "auth failed"),
        None => trace!(project = project_id, "auth failed without credentials"),
    }
}
