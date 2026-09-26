//! HTTP layer. Routing is done by hand rather than with axum's router so the observable
//! behaviour matches the reference Express app exactly: unknown paths *and* unsupported
//! methods answer `404 NotImplementedException` (never `405`, which clients read as
//! "not accepting new syncs"), paths are case-insensitive, a trailing slash is ignored,
//! and the handler for each route is chosen from the `Accept-Version` header.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use http_body_util::{BodyExt, LengthLimitError, Limited};
use serde::Serialize;
use serde_json::Value;

use crate::config::API_VERSION;
use crate::error::ApiError;
use crate::service::{Service, cast_string, now_millis, parse_sync_id};
use crate::throttle::Throttle;
use crate::version;

const DOCS_HTML: &str = include_str!("docs.html");

pub struct App {
    pub service: Service,
    throttle: Option<Throttle>,
    base_path: String,
}

impl App {
    pub fn new(service: Service) -> Self {
        let throttle = Throttle::new(service.config.throttle.max_requests, service.config.throttle.time_window);
        let base_path = service.config.base_path().to_ascii_lowercase();
        Self { service, throttle, base_path }
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new().fallback(handle).with_state(app)
}

enum Cors {
    AnyOrigin,
    Origin(HeaderValue),
    /// Origin allow-list configured but the request carries no `Origin` (not a browser
    /// cross-origin request): serve it without CORS headers.
    None,
}

async fn handle(State(app): State<Arc<App>>, request: Request) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let (cors, mut response) = match cors_policy(&app, request.headers()) {
        Ok(cors) => {
            let response = if method == Method::OPTIONS {
                preflight(request.headers())
            } else {
                dispatch(&app, request).await.unwrap_or_else(error_response)
            };
            (cors, response)
        }
        Err(err) => (Cors::None, error_response(err)),
    };
    let headers = response.headers_mut();
    apply_security_headers(headers);
    match cors {
        Cors::AnyOrigin => {
            headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
        }
        Cors::Origin(origin) => {
            headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
            headers.append(header::VARY, HeaderValue::from_static("Origin"));
        }
        Cors::None => {}
    }
    tracing::debug!(%method, %path, status = response.status().as_u16(), "request");
    response
}

fn cors_policy(app: &App, headers: &HeaderMap) -> Result<Cors, ApiError> {
    let allowed = &app.service.config.allowed_origins;
    if allowed.is_empty() {
        return Ok(Cors::AnyOrigin);
    }
    match headers.get(header::ORIGIN) {
        None => Ok(Cors::None),
        Some(origin) if allowed.iter().any(|a| a.as_bytes() == origin.as_bytes()) => Ok(Cors::Origin(origin.clone())),
        Some(_) => Err(ApiError::OriginNotPermitted),
    }
}

/// CORS preflight, as answered by the `cors` middleware's defaults.
fn preflight(request_headers: &HeaderMap) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let headers = response.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET,HEAD,PUT,PATCH,POST,DELETE"));
    if let Some(requested) = request_headers.get(header::ACCESS_CONTROL_REQUEST_HEADERS) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, requested.clone());
        headers.append(header::VARY, HeaderValue::from_static("Access-Control-Request-Headers"));
    }
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    response
}

async fn dispatch(app: &App, request: Request) -> Result<Response, ApiError> {
    let client_ip = client_ip(app, &request);

    let mut rate_headers = None;
    if let Some(throttle) = &app.throttle {
        let hit = throttle.hit(client_ip.as_deref().unwrap_or(""), now_millis());
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-limit", hit.limit.into());
        headers.insert("x-ratelimit-remaining", hit.remaining.into());
        headers.insert("x-ratelimit-reset", hit.reset_secs.into());
        if hit.exceeded {
            headers.insert(header::RETRY_AFTER, hit.retry_after_secs.into());
            let mut response = error_response(ApiError::RequestThrottled);
            response.headers_mut().extend(headers);
            return Ok(response);
        }
        rate_headers = Some(headers);
    }

    let mut response = route(app, request, client_ip.as_deref()).await?;
    if let Some(headers) = rate_headers {
        response.headers_mut().extend(headers);
    }
    Ok(response)
}

async fn route(app: &App, request: Request, client_ip: Option<&str>) -> Result<Response, ApiError> {
    let (parts, body) = request.into_parts();
    let method = parts.method;
    let path = parts.uri.path();

    if path == "/favicon.ico" {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    // Like `express.json()`, the body is parsed (and size-limited) before routing.
    let body = read_json_body(&parts.headers, body, app.service.config.max_sync_size).await?;
    let version = parts
        .headers
        .get("accept-version")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .unwrap_or(API_VERSION)
        .to_owned();

    let Some(relative) = strip_base(path, &app.base_path) else {
        return Err(ApiError::NotImplemented);
    };
    let segments: Vec<&str> = relative.split('/').collect();
    let literal = |i: usize, name: &str| segments.get(i).is_some_and(|s| s.eq_ignore_ascii_case(name));
    let is_get = method == Method::GET || method == Method::HEAD;
    let service = &app.service;

    match segments.len() {
        1 if segments[0].is_empty() || segments[0].eq_ignore_ascii_case("index.html") => {
            if !is_get {
                return Err(ApiError::NotImplemented);
            }
            Ok(([(header::CONTENT_TYPE, "text/html; charset=utf-8")], DOCS_HTML).into_response())
        }
        1 if literal(0, "info") && is_get => {
            select(&version, &["^1.0.0"])?;
            Ok(json(&service.info().await))
        }
        1 if literal(0, "bookmarks") && method == Method::POST => match select(&version, &["~1.0.0", "^1.1.3"])? {
            0 => {
                let bookmarks = required_string(&body, "bookmarks")?;
                Ok(json(&service.create_v1(bookmarks, client_ip).await?))
            }
            _ => {
                let sync_version = required_string(&body, "version")?;
                Ok(json(&service.create_v2(sync_version, client_ip).await?))
            }
        },
        2 if literal(0, "bookmarks") && is_get => {
            select(&version, &["^1.0.0"])?;
            let id = sync_id(segments[1])?;
            Ok(json(&service.get_bookmarks(&id).await?))
        }
        2 if literal(0, "bookmarks") && method == Method::PUT => {
            let mapping = select(&version, &["~1.0.0", "^1.1.3"])?;
            let id = sync_id(segments[1])?;
            let bookmarks = required_string(&body, "bookmarks")?;
            match mapping {
                0 => Ok(json(&service.update_v1(&id, &bookmarks).await?)),
                _ => {
                    let sync_version = cast_string(field(&body, "version"))?;
                    let last_updated = field(&body, "lastUpdated");
                    Ok(json(&service.update_v2(&id, &bookmarks, last_updated, sync_version.as_deref()).await?))
                }
            }
        }
        3 if literal(0, "bookmarks") && literal(2, "lastUpdated") && is_get => {
            select(&version, &["^1.0.0"])?;
            let id = sync_id(segments[1])?;
            Ok(json(&service.get_last_updated(&id).await?))
        }
        3 if literal(0, "bookmarks") && literal(2, "version") && is_get => {
            select(&version, &["^1.1.3"])?;
            let id = sync_id(segments[1])?;
            Ok(json(&service.get_version(&id).await?))
        }
        _ => Err(ApiError::NotImplemented),
    }
}

/// Returns the path relative to the base (without leading or trailing slash), or `None`
/// if it lies outside the base path.
fn strip_base<'a>(path: &'a str, base: &str) -> Option<&'a str> {
    let rest = if path.len() + 1 == base.len() && base.eq_ignore_ascii_case(&format!("{path}/")) {
        ""
    } else {
        let prefix = path.get(..base.len())?;
        if !prefix.eq_ignore_ascii_case(base) {
            return None;
        }
        &path[base.len()..]
    };
    Some(rest.strip_suffix('/').unwrap_or(rest))
}

fn select(version: &str, mappings: &[&str]) -> Result<usize, ApiError> {
    version::select(version, mappings).ok_or(ApiError::UnsupportedVersion)
}

fn sync_id(segment: &str) -> Result<String, ApiError> {
    let decoded = percent_encoding::percent_decode_str(segment).decode_utf8().map_err(|_| ApiError::InvalidSyncId)?;
    parse_sync_id(&decoded)
}

fn field<'a>(body: &'a Value, name: &str) -> &'a Value {
    body.get(name).unwrap_or(&Value::Null)
}

fn required_string(body: &Value, name: &str) -> Result<String, ApiError> {
    cast_string(field(body, name))?.ok_or(ApiError::RequiredDataNotFound)
}

fn client_ip(app: &App, request: &Request) -> Option<String> {
    if app.service.config.server.behind_proxy {
        let forwarded = request
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|v| !v.is_empty());
        if let Some(ip) = forwarded {
            return Some(ip.to_owned());
        }
    }
    request.extensions().get::<ConnectInfo<SocketAddr>>().map(|ConnectInfo(addr)| addr.ip().to_canonical().to_string())
}

/// Reads a JSON request body the way `express.json({ limit })` does: only
/// `application/json` bodies are parsed (anything else yields `{}`), oversize bodies are
/// a `SyncDataLimitExceededException`, and malformed JSON or a top-level value other than
/// an object or array is an `UnspecifiedException`.
async fn read_json_body(headers: &HeaderMap, body: Body, limit: usize) -> Result<Value, ApiError> {
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"));
    if !is_json {
        return Ok(Value::Object(Default::default()));
    }
    let declared =
        headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|len| len > limit as u64) {
        return Err(ApiError::SyncDataLimitExceeded);
    }
    let bytes: Bytes = match Limited::new(body, limit).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(err) if err.downcast_ref::<LengthLimitError>().is_some() => {
            return Err(ApiError::SyncDataLimitExceeded);
        }
        Err(_) => return Err(ApiError::Unspecified),
    };
    let text = std::str::from_utf8(&bytes).map_err(|_| ApiError::Unspecified)?;
    let trimmed = text.trim_start_matches([' ', '\t', '\n', '\r']);
    if trimmed.is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    if !trimmed.starts_with(['{', '[']) {
        return Err(ApiError::Unspecified);
    }
    serde_json::from_str(trimmed).map_err(|_| ApiError::Unspecified)
}

fn json<T: Serialize>(value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => ([(header::CONTENT_TYPE, "application/json; charset=utf-8")], body).into_response(),
        Err(_) => error_response(ApiError::Unspecified),
    }
}

fn error_response(err: ApiError) -> Response {
    let mut response = json(&err.body());
    *response.status_mut() = err.status();
    response
}

/// Headers set by `helmet@4` defaults and `nocache` in the reference API.
fn apply_security_headers(headers: &mut HeaderMap) {
    const HEADERS: &[(&str, &str)] = &[
        (
            "content-security-policy",
            "default-src 'self';base-uri 'self';block-all-mixed-content;font-src 'self' https: data:;\
             frame-ancestors 'self';img-src 'self' data:;object-src 'none';script-src 'self';\
             script-src-attr 'none';style-src 'self' https: 'unsafe-inline';upgrade-insecure-requests",
        ),
        ("x-dns-prefetch-control", "off"),
        ("expect-ct", "max-age=0"),
        ("x-frame-options", "SAMEORIGIN"),
        ("strict-transport-security", "max-age=15552000; includeSubDomains"),
        ("x-download-options", "noopen"),
        ("x-content-type-options", "nosniff"),
        ("x-permitted-cross-domain-policies", "none"),
        ("referrer-policy", "no-referrer"),
        ("x-xss-protection", "0"),
        ("surrogate-control", "no-store"),
        ("cache-control", "no-store, no-cache, must-revalidate, proxy-revalidate"),
        ("pragma", "no-cache"),
        ("expires", "0"),
    ];
    for (name, value) in HEADERS {
        headers.insert(HeaderName::from_static(name), HeaderValue::from_static(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_base_path() {
        assert_eq!(strip_base("/info", "/"), Some("info"));
        assert_eq!(strip_base("/info/", "/"), Some("info"));
        assert_eq!(strip_base("/", "/"), Some(""));
        assert_eq!(strip_base("/api", "/api/"), Some(""));
        assert_eq!(strip_base("/API/info", "/api/"), Some("info"));
        assert_eq!(strip_base("/other/info", "/api/"), None);
        assert_eq!(strip_base("/ap", "/api/"), None);
    }
}
