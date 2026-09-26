//! End-to-end tests of the HTTP contract.
//!
//! By default each test runs against a private in-memory SQLite store. Set
//! `MARKSYNC_TEST_MONGO_URI` (e.g. `mongodb://127.0.0.1:27017`) to run the same suite
//! against MongoDB instead, using a throwaway database per test.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use marksync_server::config::Config;
use marksync_server::http::{self, App};

struct TestApp {
    app: Arc<App>,
}

struct TestResponse {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Value,
}

impl TestApp {
    async fn new(overrides: Value) -> Self {
        let db = match std::env::var("MARKSYNC_TEST_MONGO_URI") {
            Ok(uri) if !overrides["db"]["path"].is_string() => json!({
                "type": "mongodb",
                "uri": uri,
                "name": format!("marksync_test_{}", uuid::Uuid::new_v4().simple()),
            }),
            _ => json!({ "type": "sqlite", "path": ":memory:" }),
        };
        let mut layer = json!({ "db": db, "log": { "stdout": { "enabled": false } } });
        merge(&mut layer, overrides);
        let config = Config::with_overrides(layer).unwrap();
        Self { app: marksync_server::build_app(config).await.unwrap() }
    }

    async fn default() -> Self {
        Self::new(json!({})).await
    }

    async fn send(&self, request: Request<Body>) -> TestResponse {
        let mut request = request;
        if request.extensions().get::<ConnectInfo<SocketAddr>>().is_none() {
            request.extensions_mut().insert(ConnectInfo(SocketAddr::from(([10, 0, 0, 1], 5000))));
        }
        let response = http::router(self.app.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
        };
        TestResponse { status, headers, body }
    }

    async fn get(&self, uri: &str) -> TestResponse {
        self.send(Request::get(uri).body(Body::empty()).unwrap()).await
    }

    async fn json(&self, method: Method, uri: &str, body: Value) -> TestResponse {
        self.send(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }

    async fn create(&self) -> Value {
        let res = self.json(Method::POST, "/bookmarks", json!({ "version": "1.1.13" })).await;
        assert_eq!(res.status, StatusCode::OK, "{:?}", res.body);
        res.body
    }
}

fn merge(target: &mut Value, source: Value) {
    match (target, source) {
        (Value::Object(t), Value::Object(s)) => {
            for (k, v) in s {
                match t.get_mut(&k) {
                    Some(existing) => merge(existing, v),
                    None => {
                        t.insert(k, v);
                    }
                }
            }
        }
        (t, s) => *t = s,
    }
}

fn assert_error(res: &TestResponse, status: StatusCode, code: &str) {
    assert_eq!(res.status, status, "body: {:?}", res.body);
    assert_eq!(res.body["code"], code);
    assert!(res.body["message"].as_str().is_some_and(|m| !m.is_empty()));
    assert_eq!(res.body.as_object().unwrap().len(), 2);
}

fn is_iso_millis(value: &Value) -> bool {
    let s = value.as_str().unwrap_or_default();
    s.len() == 24 && s.ends_with('Z') && chrono::DateTime::parse_from_rfc3339(s).is_ok() && &s[19..20] == "."
}

#[tokio::test]
async fn info_reports_service_details() {
    let app = TestApp::new(
        json!({ "location": "gb", "maxSyncSize": 1048576, "status": { "message": "hi<script>x()</script>!" } }),
    )
    .await;
    let res = app.get("/info").await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.body,
        json!({ "location": "GB", "maxSyncSize": 1048576, "message": "hi!", "status": 1, "version": "1.1.13" })
    );
    assert_eq!(res.headers["content-type"], "application/json; charset=utf-8");
    assert_eq!(res.headers["cache-control"], "no-store, no-cache, must-revalidate, proxy-revalidate");
    assert_eq!(res.headers["access-control-allow-origin"], "*");
    assert_eq!(res.headers["x-ratelimit-limit"], "1000");
}

#[tokio::test]
async fn full_sync_lifecycle() {
    let app = TestApp::default().await;
    let created = app.create().await;
    let id = created["id"].as_str().unwrap().to_owned();
    assert!(id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    assert_eq!(created["version"], "1.1.13");
    assert!(is_iso_millis(&created["lastUpdated"]));
    assert_eq!(created.as_object().unwrap().len(), 3);

    // A fresh sync has no bookmarks yet.
    let res = app.get(&format!("/bookmarks/{id}")).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body, json!({ "bookmarks": "", "version": "1.1.13", "lastUpdated": created["lastUpdated"] }));

    let res = app
        .json(
            Method::PUT,
            &format!("/bookmarks/{id}"),
            json!({ "bookmarks": "ciphertext", "lastUpdated": created["lastUpdated"], "version": "1.1.14" }),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK);
    let updated = res.body["lastUpdated"].clone();
    assert!(is_iso_millis(&updated));
    assert_ne!(updated, created["lastUpdated"]);
    assert_eq!(res.body.as_object().unwrap().len(), 1);

    let res = app.get(&format!("/bookmarks/{id}")).await;
    assert_eq!(res.body, json!({ "bookmarks": "ciphertext", "version": "1.1.14", "lastUpdated": updated }));

    let res = app.get(&format!("/bookmarks/{id}/lastUpdated")).await;
    assert_eq!(res.body, json!({ "lastUpdated": updated }));

    let res = app.get(&format!("/bookmarks/{id}/version")).await;
    assert_eq!(res.body, json!({ "version": "1.1.14" }));

    // Update without lastUpdated or version: unconditional, version unchanged.
    let res = app.json(Method::PUT, &format!("/bookmarks/{id}"), json!({ "bookmarks": "second" })).await;
    assert_eq!(res.status, StatusCode::OK);
    let res = app.get(&format!("/bookmarks/{id}")).await;
    assert_eq!(res.body["bookmarks"], "second");
    assert_eq!(res.body["version"], "1.1.14");
}

#[tokio::test]
async fn stale_last_updated_is_a_conflict() {
    let app = TestApp::default().await;
    let created = app.create().await;
    let uri = format!("/bookmarks/{}", created["id"].as_str().unwrap());
    let body = json!({ "bookmarks": "a", "lastUpdated": created["lastUpdated"] });
    assert_eq!(app.json(Method::PUT, &uri, body.clone()).await.status, StatusCode::OK);
    assert_error(&app.json(Method::PUT, &uri, body).await, StatusCode::CONFLICT, "SyncConflictException");
    let res = app.json(Method::PUT, &uri, json!({ "bookmarks": "a", "lastUpdated": "garbage" })).await;
    assert_error(&res, StatusCode::CONFLICT, "SyncConflictException");
}

#[tokio::test]
async fn invalid_and_unknown_sync_ids() {
    let app = TestApp::default().await;
    for uri in ["/bookmarks/nope", "/bookmarks/52758CB942814FAA9AB255208025AE65", "/bookmarks/abc/lastUpdated"] {
        assert_error(&app.get(uri).await, StatusCode::UNAUTHORIZED, "InvalidSyncIdException");
    }
    let unknown = "52758cb942814faa9ab255208025ae65";
    for uri in [
        format!("/bookmarks/{unknown}"),
        format!("/bookmarks/{unknown}/lastUpdated"),
        format!("/bookmarks/{unknown}/version"),
    ] {
        assert_error(&app.get(&uri).await, StatusCode::UNAUTHORIZED, "SyncNotFoundException");
    }
    let res = app.json(Method::PUT, &format!("/bookmarks/{unknown}"), json!({ "bookmarks": "x" })).await;
    assert_error(&res, StatusCode::UNAUTHORIZED, "SyncNotFoundException");
}

#[tokio::test]
async fn hyphenated_ids_and_case_insensitive_paths_are_accepted() {
    let app = TestApp::default().await;
    let id = app.create().await["id"].as_str().unwrap().to_owned();
    let hyphenated = format!("{}-{}-{}-{}-{}", &id[..8], &id[8..12], &id[12..16], &id[16..20], &id[20..]);
    assert_eq!(app.get(&format!("/bookmarks/{hyphenated}")).await.status, StatusCode::OK);
    assert_eq!(app.get(&format!("/Bookmarks/{id}/LASTUPDATED/")).await.status, StatusCode::OK);
}

#[tokio::test]
async fn missing_required_data() {
    let app = TestApp::default().await;
    assert_error(
        &app.json(Method::POST, "/bookmarks", json!({})).await,
        StatusCode::BAD_REQUEST,
        "RequiredDataNotFoundException",
    );
    assert_error(
        &app.json(Method::POST, "/bookmarks", json!({ "version": "" })).await,
        StatusCode::BAD_REQUEST,
        "RequiredDataNotFoundException",
    );
    // Non-JSON bodies are ignored, as by express.json().
    let res = app
        .send(
            Request::post("/bookmarks")
                .header("content-type", "text/plain")
                .body(Body::from(r#"{"version":"1"}"#))
                .unwrap(),
        )
        .await;
    assert_error(&res, StatusCode::BAD_REQUEST, "RequiredDataNotFoundException");

    let id = app.create().await["id"].as_str().unwrap().to_owned();
    let res = app.json(Method::PUT, &format!("/bookmarks/{id}"), json!({ "version": "1" })).await;
    assert_error(&res, StatusCode::BAD_REQUEST, "RequiredDataNotFoundException");
}

#[tokio::test]
async fn malformed_json_is_unspecified_error() {
    let app = TestApp::default().await;
    let res = app
        .send(Request::post("/bookmarks").header("content-type", "application/json").body(Body::from("{nope")).unwrap())
        .await;
    assert_error(&res, StatusCode::INTERNAL_SERVER_ERROR, "UnspecifiedException");
}

#[tokio::test]
async fn unknown_routes_and_methods_are_not_implemented() {
    let app = TestApp::default().await;
    assert_error(&app.get("/nope").await, StatusCode::NOT_FOUND, "NotImplementedException");
    assert_error(&app.get("/bookmarks").await, StatusCode::NOT_FOUND, "NotImplementedException");
    let res =
        app.send(Request::delete("/bookmarks/52758cb942814faa9ab255208025ae65").body(Body::empty()).unwrap()).await;
    assert_error(&res, StatusCode::NOT_FOUND, "NotImplementedException");
    let res = app.json(Method::POST, "/info", json!({})).await;
    assert_error(&res, StatusCode::NOT_FOUND, "NotImplementedException");
}

#[tokio::test]
async fn docs_page_is_served_at_root() {
    let app = TestApp::default().await;
    let res = app.get("/").await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.headers["content-type"].to_str().unwrap().starts_with("text/html"));
}

#[tokio::test]
async fn accept_version_selects_route_behaviour() {
    let app = TestApp::default().await;
    let with_version = |method: Method, uri: &str, version: &str, body: Value| {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .header("accept-version", version)
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    let res = app.send(with_version(Method::GET, "/info", "2.0.0", json!({}))).await;
    assert_error(&res, StatusCode::PRECONDITION_FAILED, "UnsupportedVersionException");

    // Legacy create stores bookmarks directly and returns no version.
    let res = app.send(with_version(Method::POST, "/bookmarks", "1.0.0", json!({ "bookmarks": "legacy" }))).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body.as_object().unwrap().len(), 2);
    let id = res.body["id"].as_str().unwrap().to_owned();
    let res = app.get(&format!("/bookmarks/{id}")).await;
    assert_eq!(res.body["bookmarks"], "legacy");
    assert!(res.body.get("version").is_none());

    // Legacy update ignores lastUpdated (no conflict detection).
    let res = app
        .send(with_version(
            Method::PUT,
            &format!("/bookmarks/{id}"),
            "1.0.0",
            json!({ "bookmarks": "legacy2", "lastUpdated": "stale" }),
        ))
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(is_iso_millis(&res.body["lastUpdated"]));

    // Legacy update of an unknown sync answers an empty object.
    let res = app
        .send(with_version(
            Method::PUT,
            "/bookmarks/52758cb942814faa9ab255208025ae65",
            "1.0.0",
            json!({ "bookmarks": "x" }),
        ))
        .await;
    assert_eq!((res.status, res.body), (StatusCode::OK, json!({})));

    let res = app.send(with_version(Method::POST, "/bookmarks", "1.1.13", json!({ "bookmarks": "x" }))).await;
    assert_error(&res, StatusCode::BAD_REQUEST, "RequiredDataNotFoundException");
}

#[tokio::test]
async fn oversize_payload_is_rejected() {
    let app = TestApp::new(json!({ "maxSyncSize": 100 })).await;
    let id = app.create().await["id"].as_str().unwrap().to_owned();
    let res = app.json(Method::PUT, &format!("/bookmarks/{id}"), json!({ "bookmarks": "x".repeat(200) })).await;
    assert_error(&res, StatusCode::PAYLOAD_TOO_LARGE, "SyncDataLimitExceededException");
    // Also without a Content-Length header (streamed body).
    let stream = futures_stream(vec!["{\"bookmarks\":\"".into(), "x".repeat(200), "\"}".into()]);
    let res = app
        .send(Request::put(format!("/bookmarks/{id}")).header("content-type", "application/json").body(stream).unwrap())
        .await;
    assert_error(&res, StatusCode::PAYLOAD_TOO_LARGE, "SyncDataLimitExceededException");
}

fn futures_stream(chunks: Vec<String>) -> Body {
    Body::from_stream(futures_util::stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>)))
}

#[tokio::test]
async fn offline_service() {
    let app = TestApp::new(json!({ "status": { "online": false } })).await;
    assert_eq!(app.get("/info").await.body["status"], 2);
    assert_error(
        &app.json(Method::POST, "/bookmarks", json!({ "version": "1" })).await,
        StatusCode::SERVICE_UNAVAILABLE,
        "ServiceNotAvailableException",
    );
    assert_error(
        &app.get("/bookmarks/52758cb942814faa9ab255208025ae65").await,
        StatusCode::SERVICE_UNAVAILABLE,
        "ServiceNotAvailableException",
    );
}

#[tokio::test]
async fn new_syncs_disabled_or_capped() {
    let app = TestApp::new(json!({ "status": { "allowNewSyncs": false } })).await;
    assert_eq!(app.get("/info").await.body["status"], 3);
    assert_error(
        &app.json(Method::POST, "/bookmarks", json!({ "version": "1" })).await,
        StatusCode::METHOD_NOT_ALLOWED,
        "NewSyncsForbiddenException",
    );

    let app = TestApp::new(json!({ "maxSyncs": 1 })).await;
    assert_eq!(app.get("/info").await.body["status"], 1);
    app.create().await;
    assert_eq!(app.get("/info").await.body["status"], 3);
    assert_error(
        &app.json(Method::POST, "/bookmarks", json!({ "version": "1" })).await,
        StatusCode::METHOD_NOT_ALLOWED,
        "NewSyncsForbiddenException",
    );
}

#[tokio::test]
async fn daily_new_sync_limit_is_per_client_ip() {
    let app = TestApp::new(json!({ "dailyNewSyncsLimit": 2 })).await;
    app.create().await;
    app.create().await;
    assert_error(
        &app.json(Method::POST, "/bookmarks", json!({ "version": "1" })).await,
        StatusCode::NOT_ACCEPTABLE,
        "NewSyncsLimitExceededException",
    );
    let mut other = Request::post("/bookmarks")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"version":"1"}"#))
        .unwrap();
    other.extensions_mut().insert(ConnectInfo(SocketAddr::from(([10, 0, 0, 2], 5000))));
    assert_eq!(app.send(other).await.status, StatusCode::OK);
}

#[tokio::test]
async fn forwarded_client_ip_is_used_behind_proxy() {
    let app = TestApp::new(json!({ "dailyNewSyncsLimit": 1, "server": { "behindProxy": true } })).await;
    let request = |ip: &str| {
        Request::post("/bookmarks")
            .header("content-type", "application/json")
            .header("x-forwarded-for", format!("{ip}, 172.16.0.1"))
            .body(Body::from(r#"{"version":"1"}"#))
            .unwrap()
    };
    assert_eq!(app.send(request("1.1.1.1")).await.status, StatusCode::OK);
    assert_eq!(app.send(request("1.1.1.1")).await.status, StatusCode::NOT_ACCEPTABLE);
    assert_eq!(app.send(request("2.2.2.2")).await.status, StatusCode::OK);
}

#[tokio::test]
async fn requests_are_throttled() {
    let app = TestApp::new(json!({ "throttle": { "maxRequests": 2, "timeWindow": 60000 } })).await;
    assert_eq!(app.get("/info").await.status, StatusCode::OK);
    let res = app.get("/info").await;
    assert_eq!(res.headers["x-ratelimit-remaining"], "0");
    let res = app.get("/info").await;
    assert_error(&res, StatusCode::TOO_MANY_REQUESTS, "RequestThrottledException");
    assert_eq!(res.headers["retry-after"], "60");
}

#[tokio::test]
async fn cors_preflight_and_allow_list() {
    let app = TestApp::default().await;
    let res = app
        .send(
            Request::options("/bookmarks")
                .header("origin", "chrome-extension://abc")
                .header("access-control-request-method", "POST")
                .header("access-control-request-headers", "content-type,accept-version")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert_eq!(res.headers["access-control-allow-origin"], "*");
    assert_eq!(res.headers["access-control-allow-headers"], "content-type,accept-version");
    assert!(res.headers["access-control-allow-methods"].to_str().unwrap().contains("PUT"));

    let app = TestApp::new(json!({ "allowedOrigins": ["https://app.marksync.org"] })).await;
    let with_origin = |origin: &str| Request::get("/info").header("origin", origin).body(Body::empty()).unwrap();
    let res = app.send(with_origin("https://app.marksync.org")).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.headers["access-control-allow-origin"], "https://app.marksync.org");
    let res = app.send(with_origin("https://evil.example")).await;
    assert_error(&res, StatusCode::INTERNAL_SERVER_ERROR, "OriginNotPermittedException");
    assert!(res.headers.get("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn relative_path_prefixes_all_routes() {
    let app = TestApp::new(json!({ "server": { "relativePath": "/api/" } })).await;
    assert_eq!(app.get("/api/info").await.status, StatusCode::OK);
    assert_error(&app.get("/info").await, StatusCode::NOT_FOUND, "NotImplementedException");
    let res = app.json(Method::POST, "/api/bookmarks", json!({ "version": "1.1.13" })).await;
    assert_eq!(res.status, StatusCode::OK);
}

#[tokio::test]
async fn sqlite_file_store_persists_and_purges() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/marksync.db");
    let overrides = json!({ "db": { "path": path.to_str().unwrap() }, "syncExpiryDays": 0 });
    let app = TestApp::new(overrides.clone()).await;
    let id = app.create().await["id"].as_str().unwrap().to_owned();
    drop(app);

    let app = TestApp::new(overrides).await;
    assert_eq!(app.get(&format!("/bookmarks/{id}")).await.status, StatusCode::OK);
    assert_eq!(app.app.service.purge().await.unwrap(), (0, 0));
}

#[tokio::test]
async fn purge_removes_stale_syncs_and_expired_logs() {
    let app = TestApp::new(json!({ "dailyNewSyncsLimit": 5 })).await;
    let id = app.create().await["id"].as_str().unwrap().to_owned();
    let store = app.app.service.store();
    let now = marksync_server::service::now_millis();

    // Nothing is stale or expired yet.
    assert_eq!(store.purge(now, Some(now - 1000)).await.unwrap(), (0, 0));
    assert_eq!(store.count_new_sync_logs("10.0.0.1", now).await.unwrap(), 1);

    // Two days on, the log has expired and the sync is older than a one-day cutoff.
    let later = now + 2 * 86_400_000;
    assert_eq!(store.count_new_sync_logs("10.0.0.1", later).await.unwrap(), 0);
    assert_eq!(store.purge(later, Some(later - 86_400_000)).await.unwrap(), (1, 1));
    assert_error(&app.get(&format!("/bookmarks/{id}")).await, StatusCode::UNAUTHORIZED, "SyncNotFoundException");
}
