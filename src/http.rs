//! HTTP API: auth middleware, path guard usage, SSE wiring, static assets.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Query, Request, State};
use axum::http::{StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use crate::config::Config;
use crate::error::AppError;
use crate::grep::{self, Direction};
use crate::pathguard;

#[derive(Clone)]
pub struct AppState {
    pub root: Arc<std::path::Path>,
    pub token: Option<String>,
    pub search_semaphore: Arc<tokio::sync::Semaphore>,
    pub stream_semaphore: Arc<tokio::sync::Semaphore>,
    pub chunk_size: usize,
}

impl AppState {
    pub fn from_config(config: &Config, canonical_root: PathBuf) -> Self {
        AppState {
            root: Arc::from(canonical_root),
            token: config.token.clone(),
            search_semaphore: Arc::new(tokio::sync::Semaphore::new(config.max_searches)),
            stream_semaphore: Arc::new(tokio::sync::Semaphore::new(config.max_streams)),
            chunk_size: config.chunk_size,
        }
    }
}

#[derive(RustEmbed)]
#[folder = "ui/dist/"]
struct Assets;

pub fn build_router(state: AppState) -> Router {
    let api = Router::new()
        .route("/files", get(handle_files))
        .route("/tail", get(handle_tail))
        .route("/grep", get(handle_grep))
        .route("/stream", get(handle_stream))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth));

    Router::new()
        .nest("/api", api)
        .fallback(get(handle_static))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

async fn auth(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, AppError> {
    match &state.token {
        None => Ok(next.run(req).await),
        Some(token) => {
            if authorized(&req, token) {
                Ok(next.run(req).await)
            } else {
                Err(AppError::Unauthorized)
            }
        }
    }
}

fn authorized(req: &Request, token: &str) -> bool {
    let header_ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|v| v == token);
    if header_ok {
        return true;
    }
    // Query variant: EventSource cannot set headers. Config validation limits
    // the token to unreserved URL characters, so no percent-decoding here can
    // ever change the comparison.
    req.uri().query().is_some_and(|q| {
        q.split('&').any(|pair| {
            pair.split_once('=')
                .is_some_and(|(k, v)| k == "token" && v == token)
        })
    })
}

// ---------------------------------------------------------------------------
// Static assets (embedded in release, read from disk in debug).
// One fallback handler serves the whole ui/dist tree by path; the embedded
// key set is fixed at compile time, so traversal is impossible by construction.
// ---------------------------------------------------------------------------

async fn handle_static(uri: Uri) -> Response {
    let path = uri.path();
    if path == "/api" || path.starts_with("/api/") {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not found" })),
        )
            .into_response();
    }
    let name = path.trim_start_matches('/');
    let name = if name.is_empty() { "index.html" } else { name };
    serve_asset(name)
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("json") | Some("map") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn serve_asset(name: &str) -> Response {
    match Assets::get(name) {
        Some(asset) => {
            // Borrowed embedded assets live in .rodata: serve them zero-copy.
            let body = match asset.data {
                std::borrow::Cow::Borrowed(b) => Body::from(Bytes::from_static(b)),
                std::borrow::Cow::Owned(v) => Body::from(Bytes::from(v)),
            };
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, content_type(name))
                .header(header::CACHE_CONTROL, "no-cache")
                .body(body)
                .unwrap()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

// ---------------------------------------------------------------------------
// /api/files
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct FileEntry {
    name: String,
    size: u64,
    modified_unix: u64,
    inode: u64,
}

async fn handle_files(State(state): State<AppState>) -> Result<Json<serde_json::Value>, AppError> {
    // Directory walking is blocking I/O: keep it off the async workers.
    let root = state.root.clone();
    let files: std::io::Result<Vec<FileEntry>> = tokio::task::spawn_blocking(move || {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&*root)? {
            let entry = entry?;
            let md = entry.metadata()?;
            if !md.is_file() {
                continue;
            }
            use std::os::unix::fs::MetadataExt;
            files.push(FileEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                size: md.len(),
                modified_unix: md.mtime().max(0) as u64,
                inode: md.ino(),
            });
        }
        Ok(files)
    })
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;
    let mut files = files.map_err(AppError::from)?;
    files.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(serde_json::json!({ "files": files })))
}

// ---------------------------------------------------------------------------
// /api/tail
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TailParams {
    file: String,
    lines: Option<usize>,
    around_offset: Option<u64>,
}

#[derive(Serialize)]
struct TailResponse {
    file: String,
    start_offset: u64,
    end_offset: u64,
    lines: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    anchor_line: Option<usize>,
}

async fn handle_tail(
    State(state): State<AppState>,
    Query(p): Query<TailParams>,
) -> Result<Json<TailResponse>, AppError> {
    let path = pathguard::resolve_under_root(&state.root, &p.file)?;
    let lines = p.lines.unwrap_or(500).clamp(1, 5000);
    let chunk = state.chunk_size;

    if let Some(anchor_offset) = p.around_offset {
        let w = tokio::task::spawn_blocking(move || {
            crate::logfile::read_around(&path, anchor_offset, lines, chunk)
        })
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?
        .map_err(AppError::from)?;
        // `read_around` already computes the anchor index from raw byte
        // offsets. Re-deriving it here drifted on lossy-UTF-8 windows and
        // reported `Some(0)` for empty files.
        let anchor_line = if w.lines.is_empty() {
            None
        } else {
            w.anchor_line
        };
        return Ok(Json(TailResponse {
            file: p.file,
            start_offset: w.start_offset,
            end_offset: w.end_offset,
            lines: w.lines,
            anchor_line,
        }));
    }

    // Backward scan + window read is blocking I/O: keep it off the workers.
    let w = tokio::task::spawn_blocking(move || crate::logfile::read_tail(&path, lines, chunk))
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?
        .map_err(AppError::from)?;
    Ok(Json(TailResponse {
        file: p.file,
        start_offset: w.start_offset,
        end_offset: w.end_offset,
        lines: w.lines,
        anchor_line: None,
    }))
}

// ---------------------------------------------------------------------------
// /api/grep (SSE)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GrepParams {
    file: Option<String>,
    files: Option<String>,
    query: String,
    is_regex: Option<bool>,
    direction: Option<String>,
    from_offset: Option<u64>,
    limit: Option<usize>,
}

async fn handle_grep(
    State(state): State<AppState>,
    Query(p): Query<GrepParams>,
) -> Result<Response, AppError> {
    let is_regex = p.is_regex.unwrap_or(false);
    let matcher = grep::compile(&p.query, is_regex).map_err(AppError::from)?;
    let direction = match p.direction.as_deref() {
        None | Some("reverse") => Direction::Reverse,
        Some("forward") => Direction::Forward,
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "invalid direction {other:?}: expected \"forward\" or \"reverse\""
            )));
        }
    };
    let limit = p.limit.unwrap_or(1000).clamp(1, 10000);

    if p.file.is_some() && p.files.is_some() {
        return Err(AppError::BadRequest(
            "specify either 'file' or 'files', not both".into(),
        ));
    }

    let mut resolved_files: Vec<(String, PathBuf)> = Vec::new();
    if let Some(files_str) = &p.files {
        let mut tokens = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for raw in files_str.split(',') {
            let name = raw.trim();
            if name.is_empty() || !seen.insert(name.to_string()) {
                continue;
            }
            tokens.push(name);
        }
        if tokens.is_empty() {
            return Err(AppError::BadRequest("no files specified".into()));
        }
        if tokens.len() > 50 {
            return Err(AppError::BadRequest("too many files (maximum 50)".into()));
        }
        for name in tokens {
            let path = pathguard::resolve_under_root(&state.root, name)?;
            resolved_files.push((name.to_string(), path));
        }
    } else if let Some(file_str) = &p.file {
        let path = pathguard::resolve_under_root(&state.root, file_str)?;
        resolved_files.push((file_str.clone(), path));
    } else {
        return Err(AppError::BadRequest(
            "missing 'file' or 'files' query parameter".into(),
        ));
    }

    if resolved_files.len() > 1 && p.from_offset.is_some() {
        return Err(AppError::BadRequest(
            "from_offset is only supported for single-file search".into(),
        ));
    }

    // Gate concurrent searches; permit is released when the stream ends.
    let permit = state
        .search_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| AppError::TooManyRequests)?;

    let chunk = state.chunk_size;
    let from_offset = p.from_offset;
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(100);

    tokio::spawn(async move {
        let _permit = permit;
        // Probe one hit past the limit so the `truncated` flag is exact.
        let scan_limit = limit.saturating_add(1);
        let (hit_tx, mut hit_rx) = mpsc::channel::<grep::GrepHit>(100);
        let join = tokio::task::spawn_blocking(move || {
            // Sort files by modified timestamp in the blocking thread.
            // Cache mtime once per file to avoid O(N log N) stat syscalls.
            let mut entries: Vec<(String, PathBuf, Option<std::time::SystemTime>)> = resolved_files
                .into_iter()
                .map(|(name, path)| {
                    let mtime = path.metadata().and_then(|m| m.modified()).ok();
                    (name, path, mtime)
                })
                .collect();

            entries.sort_by(|(n1, _p1, t1), (n2, _p2, t2)| match direction {
                Direction::Reverse => t2.cmp(t1).then_with(|| n2.cmp(n1)),
                Direction::Forward => t1.cmp(t2).then_with(|| n1.cmp(n2)),
            });

            let sorted_files: Vec<(String, PathBuf)> = entries
                .into_iter()
                .map(|(name, path, _)| (name, path))
                .collect();

            grep::grep_multi(
                sorted_files,
                matcher,
                direction,
                from_offset,
                scan_limit,
                chunk,
                hit_tx,
            )
        });

        let mut matches = 0u64;
        let mut truncated = false;
        while let Some(hit) = hit_rx.recv().await {
            if matches >= limit as u64 {
                truncated = true;
                break;
            }
            matches += 1;
            let ev = Event::default()
                .event("match")
                .json_data(&hit)
                .expect("GrepHit serializes");
            if tx.send(Ok(ev)).await.is_err() {
                return; // client disconnected
            }
        }
        // Release the worker (it may be parked on a full channel send).
        drop(hit_rx);

        match join.await {
            Ok(Ok((scanned, files_scanned))) => {
                let ev = Event::default()
                    .event("done")
                    .json_data(serde_json::json!({
                        "matches": matches,
                        "scanned_bytes": scanned,
                        "truncated": truncated,
                        "files_scanned": files_scanned,
                    }))
                    .expect("done event serializes");
                let _ = tx.send(Ok(ev)).await;
            }
            Ok(Err(e)) => {
                let _ = tx
                    .send(Ok(Event::default().event("error").data(e.to_string())))
                    .await;
            }
            Err(_) => {
                let _ = tx
                    .send(Ok(Event::default().event("error").data("worker panicked")))
                    .await;
            }
        }
    });

    let sse = Sse::new(ReceiverStream::new(rx)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    );
    Ok((
        [("cache-control", "no-cache"), ("x-accel-buffering", "no")],
        sse,
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// /api/stream (SSE) — implemented in watcher.rs
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct StreamParams {
    file: String,
    from_offset: Option<u64>,
}

async fn handle_stream(
    State(state): State<AppState>,
    Query(p): Query<StreamParams>,
) -> Result<Response, AppError> {
    let path = pathguard::resolve_under_root(&state.root, &p.file)?;
    // Gate concurrent streams; each holds a watcher task and buffers.
    let permit = state
        .stream_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| AppError::TooManyRequests)?;

    let rx = crate::watcher::follow(path, p.from_offset, state.chunk_size);
    // The permit rides inside the stream adapter: it is dropped when the
    // response body is dropped (client disconnect) or the stream completes.
    let stream = ReceiverStream::new(rx).map(move |ev| {
        let _permit = &permit;
        crate::watcher::to_sse(ev)
    });
    let sse = Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    );
    Ok((
        [("cache-control", "no-cache"), ("x-accel-buffering", "no")],
        sse,
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// Integration tests (tower oneshot against the Router)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Method;
    use tower::ServiceExt;

    fn test_state(root: &std::path::Path, token: Option<&str>, max_searches: usize) -> AppState {
        test_state_with(root, token, max_searches, 8)
    }

    fn test_state_with(
        root: &std::path::Path,
        token: Option<&str>,
        max_searches: usize,
        max_streams: usize,
    ) -> AppState {
        AppState {
            // pathguard requires a canonical root (the server canonicalizes
            // at startup; on macOS tempdirs resolve through /private/var).
            root: Arc::from(root.canonicalize().unwrap()),
            token: token.map(str::to_string),
            search_semaphore: Arc::new(tokio::sync::Semaphore::new(max_searches)),
            stream_semaphore: Arc::new(tokio::sync::Semaphore::new(max_streams)),
            chunk_size: 65536,
        }
    }

    async fn request(state: &AppState, uri: &str, auth_header: Option<&str>) -> Response {
        let app = build_router(state.clone());
        let mut builder = Request::builder().method(Method::GET).uri(uri);
        if let Some(h) = auth_header {
            builder = builder.header(header::AUTHORIZATION, h);
        }
        let req = builder.body(Body::empty()).unwrap();
        app.oneshot(req).await.unwrap()
    }

    fn setup_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let mut content = String::new();
        for i in 1..=100 {
            content.push_str(&format!("line {i:03}\n"));
        }
        content.push_str("ERROR boom\n");
        std::fs::write(dir.path().join("catalina.out"), content).unwrap();
        dir
    }

    async fn read_body(resp: Response) -> String {
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        String::from_utf8_lossy(&body).into_owned()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn files_list_shape() {
        let dir = setup_root();
        let state = test_state(dir.path(), None, 2);
        let resp = request(&state, "/api/files", None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let files = v["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["name"], "catalina.out");
        assert!(files[0]["size"].as_u64().unwrap() > 0);
        assert!(files[0]["inode"].as_u64().unwrap() > 0);
        assert!(files[0]["modified_unix"].as_u64().is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn files_list_is_sorted_by_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.log"), "b").unwrap();
        std::fs::write(dir.path().join("a.log"), "a").unwrap();
        std::fs::write(dir.path().join("c.log"), "c").unwrap();
        let state = test_state(dir.path(), None, 2);
        let resp = request(&state, "/api/files", None).await;
        let v: serde_json::Value = serde_json::from_str(&read_body(resp).await).unwrap();
        let names: Vec<&str> = v["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| f["name"].as_str())
            .collect();
        assert_eq!(names, vec!["a.log", "b.log", "c.log"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tail_returns_last_lines_with_offsets() {
        let dir = setup_root();
        let state = test_state(dir.path(), None, 2);
        let resp = request(&state, "/api/tail?file=catalina.out&lines=2", None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let lines = v["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "line 100");
        assert_eq!(lines[1], "ERROR boom");
        assert!(v["start_offset"].as_u64().unwrap() < v["end_offset"].as_u64().unwrap());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tail_around_offset_returns_centred_lines() {
        let dir = setup_root();
        let state = test_state(dir.path(), None, 2);
        // "line 050" in catalina.out starts at 49 * 9 = 441.
        let resp = request(
            &state,
            "/api/tail?file=catalina.out&lines=4&around_offset=441",
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let lines = v["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 4);
        let anchor_idx = v["anchor_line"].as_u64().unwrap() as usize;
        assert_eq!(lines[anchor_idx], "line 050");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tail_around_offset_on_empty_file_omits_anchor() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("empty.log"), "").unwrap();
        let state = test_state(dir.path(), None, 2);
        let resp = request(
            &state,
            "/api/tail?file=empty.log&lines=4&around_offset=0",
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&read_body(resp).await).unwrap();
        assert!(v["lines"].as_array().unwrap().is_empty());
        // No lines → no valid index: `anchor_line` must be absent, not 0.
        assert!(v.get("anchor_line").is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_sse_has_match_and_done() {
        let dir = setup_root();
        let state = test_state(dir.path(), None, 2);
        let resp = request(
            &state,
            "/api/grep?file=catalina.out&query=ERROR&limit=5",
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let text = read_body(resp).await;
        assert!(text.contains("event: match"), "body: {text}");
        assert!(text.contains("event: done"), "body: {text}");
        assert!(text.contains("\"matches\":1"), "body: {text}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_truncated_flag_is_exact() {
        // catalina.out has exactly one ERROR line.
        let dir = setup_root();
        let state = test_state(dir.path(), None, 2);

        // Limit above the true count: not truncated.
        let resp = request(
            &state,
            "/api/grep?file=catalina.out&query=ERROR&limit=5",
            None,
        )
        .await;
        let text = read_body(resp).await;
        assert!(text.contains("\"truncated\":false"), "body: {text}");

        // Limit below the true count: truncated.
        let resp = request(
            &state,
            "/api/grep?file=catalina.out&query=line&limit=5",
            None,
        )
        .await;
        let text = read_body(resp).await;
        assert!(text.contains("\"truncated\":true"), "body: {text}");
        assert!(text.contains("\"matches\":5"), "body: {text}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_regex_returns_400() {
        let dir = setup_root();
        let state = test_state(dir.path(), None, 2);
        let resp = request(
            &state,
            "/api/grep?file=catalina.out&query=%28&is_regex=true",
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_direction_returns_400() {
        let dir = setup_root();
        let state = test_state(dir.path(), None, 2);
        let resp = request(
            &state,
            "/api/grep?file=catalina.out&query=ERROR&direction=upwards",
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp = request(
            &state,
            "/api/grep?file=catalina.out&query=ERROR&direction=forward",
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_multi_files_returns_hits_and_files_scanned() {
        let dir = setup_root();
        // Add a second log file with an ERROR
        std::fs::write(dir.path().join("localhost.log"), "localhost ERROR line\n").unwrap();
        let state = test_state(dir.path(), None, 2);
        let resp = request(
            &state,
            "/api/grep?files=catalina.out,localhost.log&query=ERROR",
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let text = read_body(resp).await;
        assert!(text.contains("event: match"), "body: {text}");
        assert!(text.contains("\"file\":\"catalina.out\""), "body: {text}");
        assert!(text.contains("\"file\":\"localhost.log\""), "body: {text}");
        assert!(text.contains("event: done"), "body: {text}");
        assert!(text.contains("\"matches\":2"), "body: {text}");
        assert!(text.contains("\"files_scanned\":2"), "body: {text}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_missing_files_param_returns_400() {
        let dir = setup_root();
        let state = test_state(dir.path(), None, 2);
        let resp = request(&state, "/api/grep?query=ERROR", None).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_both_file_and_files_returns_400() {
        let dir = setup_root();
        let state = test_state(dir.path(), None, 2);
        let resp = request(
            &state,
            "/api/grep?file=catalina.out&files=catalina.out&query=ERROR",
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_from_offset_with_multi_files_returns_400() {
        let dir = setup_root();
        std::fs::write(dir.path().join("localhost.log"), "line\n").unwrap();
        let state = test_state(dir.path(), None, 2);
        let resp = request(
            &state,
            "/api/grep?files=catalina.out,localhost.log&from_offset=10&query=ERROR",
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_multi_files_deduplicates_names() {
        let dir = setup_root();
        let state = test_state(dir.path(), None, 2);
        let resp = request(
            &state,
            "/api/grep?files=catalina.out,catalina.out&query=ERROR",
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let text = read_body(resp).await;
        assert!(text.contains("\"files_scanned\":1"), "body: {text}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn path_traversal_rejected() {
        let dir = setup_root();
        let state = test_state(dir.path(), None, 2);
        assert_eq!(
            request(&state, "/api/tail?file=../devenv.yaml", None)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(&state, "/api/tail?file=%2Fetc%2Fpasswd", None)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn symlink_escape_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "x").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("link")).unwrap();
        let state = test_state(dir.path(), None, 2);
        assert_eq!(
            request(&state, "/api/tail?file=link", None).await.status(),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_file_returns_404() {
        let dir = setup_root();
        let state = test_state(dir.path(), None, 2);
        assert_eq!(
            request(&state, "/api/tail?file=nope.log", None)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn token_required_and_accepted() {
        let dir = setup_root();
        let state = test_state(dir.path(), Some("s3cret"), 2);

        // No auth → 401.
        assert_eq!(
            request(&state, "/api/files", None).await.status(),
            StatusCode::UNAUTHORIZED
        );

        // Header auth → 200.
        assert_eq!(
            request(&state, "/api/files", Some("Bearer s3cret"))
                .await
                .status(),
            StatusCode::OK
        );

        // Query auth → 200.
        assert_eq!(
            request(&state, "/api/files?token=s3cret", None)
                .await
                .status(),
            StatusCode::OK
        );

        // Static assets exempt from auth.
        assert_eq!(request(&state, "/", None).await.status(), StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_grep_limit_returns_429() {
        let dir = setup_root();
        // A large matching file so the worker cannot finish while undrained.
        let mut content = String::new();
        for i in 0..50_000 {
            content.push_str(&format!("ERROR line {i}\n"));
        }
        std::fs::write(dir.path().join("big.out"), content).unwrap();

        let state = test_state(dir.path(), None, 2);
        let app = build_router(state.clone());
        let mk = |uri: &str| {
            Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(Body::empty())
                .unwrap()
        };

        // Hold two searches open (undrained bodies keep the permits held).
        let r1 = app
            .clone()
            .oneshot(mk("/api/grep?file=big.out&query=ERROR&limit=10000"))
            .await
            .unwrap();
        let r2 = app
            .clone()
            .oneshot(mk("/api/grep?file=big.out&query=ERROR&limit=10000"))
            .await
            .unwrap();
        assert_eq!(r1.status(), StatusCode::OK);
        assert_eq!(r2.status(), StatusCode::OK);

        let r3 = app
            .clone()
            .oneshot(mk("/api/grep?file=big.out&query=ERROR&limit=10000"))
            .await
            .unwrap();
        assert_eq!(r3.status(), StatusCode::TOO_MANY_REQUESTS);

        // Keep the in-flight responses alive to the end of the test.
        drop((r1, r2, r3));
    }

    // ---------------------------------------------------------------------------
    // /api/stream
    // ---------------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn stream_replays_from_offset() {
        let dir = setup_root();
        let state = test_state(dir.path(), None, 2);
        let resp = request(&state, "/api/stream?file=catalina.out&from_offset=0", None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers()
                .get("x-accel-buffering")
                .is_some_and(|v| v == "no")
        );

        // Read streamed chunks until a replayed line arrives.
        let mut body = resp.into_body().into_data_stream();
        let mut text = String::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !text.contains("event: line") {
            match tokio::time::timeout(Duration::from_millis(500), body.next()).await {
                Ok(Some(Ok(chunk))) => text.push_str(&String::from_utf8_lossy(&chunk)),
                Ok(Some(Err(_))) => panic!("stream body error"),
                Ok(None) => break,
                Err(_) => continue,
            }
        }
        assert!(text.contains("event: line"), "body: {text}");
        assert!(text.contains("line 001"), "body: {text}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stream_limit_returns_429() {
        let dir = setup_root();
        let state = test_state_with(dir.path(), None, 2, 1);
        let app = build_router(state.clone());
        let mk = |uri: &str| {
            Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(Body::empty())
                .unwrap()
        };

        // Hold one stream open (undrained body keeps the permit held).
        let r1 = app
            .clone()
            .oneshot(mk("/api/stream?file=catalina.out&from_offset=0"))
            .await
            .unwrap();
        assert_eq!(r1.status(), StatusCode::OK);

        let r2 = app
            .clone()
            .oneshot(mk("/api/stream?file=catalina.out&from_offset=0"))
            .await
            .unwrap();
        assert_eq!(r2.status(), StatusCode::TOO_MANY_REQUESTS);

        drop((r1, r2));
    }

    // ---------------------------------------------------------------------------
    // Static assets
    // ---------------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn static_assets_serve_and_cache_control() {
        let dir = setup_root();
        let state = test_state(dir.path(), None, 2);
        let resp = request(&state, "/", None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .is_some_and(|v| v.as_bytes().starts_with(b"text/html"))
        );
        assert!(
            resp.headers()
                .get(header::CACHE_CONTROL)
                .is_some_and(|v| v == "no-cache")
        );
        let resp = request(&state, "/app.js", None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = request(&state, "/missing.js", None).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_api_route_returns_json_404() {
        let dir = setup_root();
        let state = test_state(dir.path(), None, 2);
        let resp = request(&state, "/api/nope", None).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = read_body(resp).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["error"], "not found");
    }
}
