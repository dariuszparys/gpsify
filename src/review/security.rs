//! Loopback Host, per-launch token, CSRF, JSON Content-Type, and no-store middleware.

use axum::extract::{Request, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, COOKIE, HOST, LOCATION, ORIGIN, SET_COOKIE};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use subtle::ConstantTimeEq;
use url::Url;

use crate::review::ReviewState;

pub const SESSION_COOKIE: &str = "gpsify_session";
pub const TOKEN_HEADER: &str = "x-gpsify-token";
pub const TOKEN_QUERY: &str = "token";

fn is_loopback_host(raw: &str) -> bool {
    let host = raw.trim().to_ascii_lowercase();
    if host.is_empty() || host.chars().any(char::is_whitespace) {
        return false;
    }
    let hostname = if host.starts_with('[') {
        let Some(end) = host.find(']') else {
            return false;
        };
        let rest = &host[end + 1..];
        if !rest.is_empty() {
            let Some(port_str) = rest.strip_prefix(':') else {
                return false;
            };
            if port_str.parse::<u16>().ok().filter(|&p| p >= 1).is_none() {
                return false;
            }
        }
        &host[..=end]
    } else if let Some((name, port_str)) = host.rsplit_once(':') {
        if port_str.parse::<u16>().ok().filter(|&p| p >= 1).is_none() {
            return false;
        }
        name
    } else {
        host.as_str()
    };

    matches!(hostname, "127.0.0.1" | "localhost" | "[::1]")
}

fn origin_matches_host(origin: &str, host_header: &str) -> bool {
    let Ok(parsed) = Url::parse(origin.trim()) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    let Some(host_str) = parsed.host_str() else {
        return false;
    };
    let netloc = match parsed.port() {
        Some(port) => format!("{host_str}:{port}"),
        None => host_str.to_string(),
    };
    if !is_loopback_host(&netloc) {
        return false;
    }
    netloc.eq_ignore_ascii_case(host_header.trim())
}

fn ct_eq_str(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

fn extract_cookie<'a>(cookie_header: &'a str, name: &str) -> Option<&'a str> {
    for part in cookie_header.split(';') {
        let trimmed = part.trim();
        if let Some((k, v)) = trimmed.split_once('=') {
            if k.trim() == name {
                return Some(v.trim());
            }
        }
    }
    None
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

pub async fn security_middleware(
    State(state): State<ReviewState>,
    request: Request,
    next: Next,
) -> Response {
    let headers = request.headers();
    let host_header = headers
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !is_loopback_host(host_header) {
        return error_response(StatusCode::BAD_REQUEST, "invalid host header");
    }

    let method = request.method().clone();
    if !matches!(method, Method::GET | Method::HEAD | Method::OPTIONS) {
        if let Some(sec_fetch) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
            let clean = sec_fetch.trim().to_ascii_lowercase();
            if clean != "same-origin" && clean != "none" {
                return error_response(StatusCode::FORBIDDEN, "cross-origin request blocked");
            }
        }
        if let Some(origin) = headers.get(ORIGIN).and_then(|v| v.to_str().ok()) {
            if !origin_matches_host(origin, host_header) {
                return error_response(StatusCode::FORBIDDEN, "cross-origin request blocked");
            }
        }
    }

    let cookie_match = headers
        .get(COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|raw| extract_cookie(raw, SESSION_COOKIE))
        .map(|val| ct_eq_str(val, &state.auth_token))
        .unwrap_or(false);

    let header_match = headers
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|val| ct_eq_str(val, &state.auth_token))
        .unwrap_or(false);

    let has_session = cookie_match || header_match;

    let path = request.uri().path().to_string();
    if path == "/" && matches!(method, Method::GET | Method::HEAD) {
        if let Some(query) = request.uri().query() {
            let mut found_token: Option<String> = None;
            for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
                if k == TOKEN_QUERY {
                    found_token = Some(v.into_owned());
                    break;
                }
            }
            if let Some(query_token) = found_token {
                if !ct_eq_str(&query_token, &state.auth_token) {
                    return error_response(StatusCode::UNAUTHORIZED, "unauthorized");
                }
                let cookie_val = format!(
                    "{SESSION_COOKIE}={}; HttpOnly; SameSite=Strict; Path=/",
                    state.auth_token
                );
                let mut resp = StatusCode::SEE_OTHER.into_response();
                resp.headers_mut()
                    .insert(LOCATION, HeaderValue::from_static("/"));
                if let Ok(hv) = HeaderValue::from_str(&cookie_val) {
                    resp.headers_mut().insert(SET_COOKIE, hv);
                }
                resp.headers_mut()
                    .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
                return resp;
            }
        }
    }

    if !has_session {
        return error_response(StatusCode::UNAUTHORIZED, "unauthorized");
    }

    if path.starts_with("/api/") && method == Method::POST {
        let ct = headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if ct != "application/json" {
            return error_response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Content-Type must be application/json",
            );
        }
    }

    let mut response = next.run(request).await;
    if (path == "/" || path.starts_with("/api/")) && !response.headers().contains_key(CACHE_CONTROL)
    {
        response
            .headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}
