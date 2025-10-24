use crate::config::Config;
use crate::error::BridgeError;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use tracing::{error, warn};

/// Call `git http-backend` like a CGI and translate its output into (StatusCode, headers, body).
pub fn run_git_http_backend(
    cfg: &Config,
    path_tail: &str,
    method: &Method,
    query: Option<&str>,
    content_type: Option<&HeaderValue>,
    content_length: Option<&HeaderValue>,
    request_body: &[u8],
) -> Result<(StatusCode, HeaderMap, Vec<u8>), BridgeError> {
    // Prepare env for git http-backend
    let mut cmd = Command::new("git");
    cmd.arg("http-backend")
        .env("GIT_PROJECT_ROOT", &cfg.git_root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", format!("/{}", path_tail))
        .env("REQUEST_METHOD", method.as_str())
        .env("QUERY_STRING", query.unwrap_or(""))
        .env(
            "CONTENT_TYPE",
            content_type.and_then(|v| v.to_str().ok()).unwrap_or(""),
        )
        .env(
            "CONTENT_LENGTH",
            content_length.and_then(|v| v.to_str().ok()).unwrap_or(""),
        )
        .env("REMOTE_USER", "gitbridge-ro")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(BridgeError::Io)?;

    // write request body to stdin of child
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(request_body).map_err(BridgeError::Io)?;
    }
    drop(child.stdin.take());

    // Read all stdout
    let mut stdout_buf: Vec<u8> = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        stdout
            .read_to_end(&mut stdout_buf)
            .map_err(BridgeError::Io)?;
    }

    // Capture stderr for logging
    let mut stderr_buf: Vec<u8> = Vec::new();
    if let Some(mut stderr) = child.stderr.take() {
        stderr
            .read_to_end(&mut stderr_buf)
            .map_err(BridgeError::Io)?;
    }
    if !stderr_buf.is_empty() {
        warn!(
            "git http-backend stderr: {}",
            String::from_utf8_lossy(&stderr_buf)
        );
    }

    // make sure process exited "successfully"
    let status = child.wait().map_err(BridgeError::Io)?;
    if !status.success() {
        error!("git http-backend exited with {status:?}");
        return Err(BridgeError::Other(format!(
            "git http-backend failed with {status:?}"
        )));
    }

    // Parse CGI-style output: headers \r\n\r\n body
    parse_cgi_response(stdout_buf)
}

fn parse_cgi_response(mut all: Vec<u8>) -> Result<(StatusCode, HeaderMap, Vec<u8>), BridgeError> {
    // find header/body split
    let split_seq = b"\r\n\r\n";
    let split_alt = b"\n\n";

    let (header_bytes, body_start_idx) = if let Some(idx) = find_subslice(&all, split_seq) {
        let (h, _rest) = all.split_at(idx);
        (h.to_vec(), idx + split_seq.len())
    } else if let Some(idx) = find_subslice(&all, split_alt) {
        let (h, _rest) = all.split_at(idx);
        (h.to_vec(), idx + split_alt.len())
    } else {
        return Err(BridgeError::Other(
            "git http-backend output missing header delimiter".into(),
        ));
    };

    let body_bytes = all.split_off(body_start_idx);

    // parse headers line by line
    let header_text = String::from_utf8(header_bytes)?;
    let mut status_code = StatusCode::OK;
    let mut headers = HeaderMap::new();

    for line in header_text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim();
            let value = value.trim();

            if name.eq_ignore_ascii_case("Status") {
                // "Status: 200 OK"
                let mut parts = value.split_whitespace();
                if let Some(code_str) = parts.next() {
                    if let Ok(code_u16) = code_str.parse::<u16>() {
                        if let Ok(sc) = StatusCode::from_u16(code_u16) {
                            status_code = sc;
                        }
                    }
                }
                continue;
            }

            let header_name = axum::http::HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| BridgeError::HeaderName(e.to_string()))?;
            let header_val = axum::http::HeaderValue::from_str(value)
                .map_err(|e| BridgeError::HeaderValue(e.to_string()))?;
            headers.append(header_name, header_val);
        } else {
            // line without ':', ignore quietly
        }
    }

    Ok((status_code, headers, body_bytes))
}

/// Find first occurrence of needle in haystack, return start index.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
