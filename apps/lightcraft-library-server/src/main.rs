#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::collections::HashMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;
use uuid::Uuid;

const DEFAULT_BIND: &str = "127.0.0.1:8090";
const DEFAULT_ORIGIN: &str = "http://127.0.0.1:8080";
const DEFAULT_LEASE_SECONDS: u64 = 300;
const MAX_BODY_BYTES: usize = 512 * 1024 * 1024;
const LEASE_HEADER: HeaderName = HeaderName::from_static("x-lightcraft-lease");
const OFFSET_HEADER: HeaderName = HeaderName::from_static("x-lightcraft-offset");

#[derive(Clone)]
struct AppState {
    root: Arc<PathBuf>,
    api_token: Arc<str>,
    lease_seconds: u64,
    lease: Arc<Mutex<Option<Lease>>>,
    files_gate: Arc<tokio::sync::Mutex<()>>,
    handoffs: Arc<Mutex<HashMap<String, Handoff>>>,
}

struct Lease {
    token: String,
    expires_at: Instant,
}

struct Handoff {
    old_lease: String,
    new_lease: Option<String>,
    expires_at: Instant,
}

#[derive(Serialize)]
struct HandoffId {
    id: String,
}

#[derive(Serialize)]
struct HandoffStatus {
    state: &'static str,
    lease: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LeaseReply {
    lease: String,
    expires_in_seconds: u64,
}

#[derive(Serialize)]
struct HealthReply {
    status: &'static str,
    version: &'static str,
}

#[derive(Deserialize)]
struct ListQuery {
    path: String,
}

#[derive(Debug)]
struct ApiError(StatusCode, String);

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, message.into())
    }

    fn internal(message: impl Into<String>) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, message.into())
    }

    fn unauthorized() -> Self {
        Self(StatusCode::UNAUTHORIZED, "invalid or missing authorization".into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn env_value(name: &str, default: &str) -> String {
    env::var(name).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| default.to_string())
}

fn required_env(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    env::var(name)
        .ok()
        .filter(|v| v.len() >= 32)
        .ok_or_else(|| format!("{name} must be set to a secret of at least 32 characters; generate one with a password manager").into())
}

fn parse_origin(raw: &str) -> Result<HeaderValue, Box<dyn std::error::Error>> {
    let origin: HeaderValue = raw.parse().map_err(|e| format!("invalid LIGHTCRAFT_ALLOWED_ORIGIN: {e}"))?;
    if origin == HeaderValue::from_static("*") {
        return Err("LIGHTCRAFT_ALLOWED_ORIGIN cannot be *".into());
    }
    Ok(origin)
}

fn authorized(headers: &HeaderMap, state: &AppState) -> bool {
    let Some(value) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) else { return false };
    let Some(provided) = value.strip_prefix("Bearer ") else { return false };
    provided.as_bytes() == state.api_token.as_bytes()
}

fn lease_token(headers: &HeaderMap) -> Option<&str> {
    headers.get(&LEASE_HEADER).and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty())
}

fn check_auth(headers: &HeaderMap, state: &AppState) -> Result<(), ApiError> {
    if authorized(headers, state) { Ok(()) } else { Err(ApiError::unauthorized()) }
}

fn check_lease(headers: &HeaderMap, state: &AppState) -> Result<(), ApiError> {
    check_auth(headers, state)?;
    let Some(token) = lease_token(headers) else { return Err(ApiError(StatusCode::CONFLICT, "an active library lease is required".into())) };
    let mut held = lock(&state.lease);
    if held.as_ref().is_some_and(|lease| lease.expires_at <= Instant::now()) {
        *held = None;
    }
    if held.as_ref().is_some_and(|lease| lease.token == token) {
        Ok(())
    } else {
        Err(ApiError(StatusCode::CONFLICT, "the library lease expired or belongs to another editor".into()))
    }
}

async fn health() -> Json<HealthReply> {
    Json(HealthReply { status: "ok", version: env!("CARGO_PKG_VERSION") })
}

async fn acquire_lease(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<LeaseReply>, ApiError> {
    check_auth(&headers, &state)?;
    let _files_guard = state.files_gate.lock().await;
    let mut held = lock(&state.lease);
    if held.as_ref().is_some_and(|lease| lease.expires_at > Instant::now()) {
        return Err(ApiError(StatusCode::CONFLICT, "the library is already open in another editor".into()));
    }
    let token = Uuid::new_v4().to_string();
    *held = Some(Lease { token: token.clone(), expires_at: Instant::now() + Duration::from_secs(state.lease_seconds) });
    Ok(Json(LeaseReply { lease: token, expires_in_seconds: state.lease_seconds }))
}

/// Start a graceful handoff request. The current lease holder receives it through polling.
async fn request_handoff(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<HandoffId>, ApiError> {
    check_auth(&headers, &state)?;
    let held = lock(&state.lease);
    let Some(lease) = held.as_ref().filter(|lease| lease.expires_at > Instant::now()) else {
        return Err(ApiError(StatusCode::CONFLICT, "there is no active editor to hand off from".into()));
    };
    let old_lease = lease.token.clone();
    drop(held);
    let mut handoffs = lock(&state.handoffs);
    handoffs.retain(|_, handoff| handoff.new_lease.is_some() || handoff.expires_at > Instant::now());
    if handoffs.values().any(|handoff| handoff.new_lease.is_none()) {
        return Err(ApiError(StatusCode::CONFLICT, "a handoff request is already pending".into()));
    }
    let id = Uuid::new_v4().to_string();
    handoffs.insert(id.clone(), Handoff { old_lease, new_lease: None, expires_at: Instant::now() + Duration::from_secs(30) });
    Ok(Json(HandoffId { id }))
}

/// The active session polls this endpoint for a pending handoff event.
async fn pending_handoff(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Option<HandoffId>>, ApiError> {
    check_auth(&headers, &state)?;
    let Some(token) = lease_token(&headers) else { return Err(ApiError(StatusCode::CONFLICT, "an active library lease is required".into())) };
    let held = lock(&state.lease);
    if !held.as_ref().is_some_and(|lease| lease.token == token && lease.expires_at > Instant::now()) {
        return Err(ApiError(StatusCode::CONFLICT, "the library lease expired or belongs to another editor".into()));
    }
    let handoffs = lock(&state.handoffs);
    let id = handoffs
        .iter()
        .find(|(_, handoff)| handoff.old_lease == token && handoff.new_lease.is_none() && handoff.expires_at > Instant::now())
        .map(|(id, _)| id.clone());
    Ok(Json(id.map(|id| HandoffId { id })))
}

/// The requester polls until the old session flushes and acknowledges the transfer.
async fn handoff_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<HandoffStatus>, ApiError> {
    check_auth(&headers, &state)?;
    let handoffs = lock(&state.handoffs);
    let Some(handoff) = handoffs.get(&id) else { return Err(ApiError(StatusCode::NOT_FOUND, "handoff request not found".into())) };
    Ok(Json(match &handoff.new_lease {
        Some(lease) => HandoffStatus { state: "complete", lease: Some(lease.clone()) },
        None if handoff.expires_at > Instant::now() => HandoffStatus { state: "pending", lease: None },
        None => HandoffStatus { state: "expired", lease: None },
    }))
}

async fn cancel_handoff(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<StatusCode, ApiError> {
    check_auth(&headers, &state)?;
    lock(&state.handoffs).remove(&id);
    Ok(StatusCode::NO_CONTENT)
}

async fn acknowledge_handoff(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<LeaseReply>, ApiError> {
    check_auth(&headers, &state)?;
    let Some(token) = lease_token(&headers).map(str::to_string) else {
        return Err(ApiError(StatusCode::CONFLICT, "an active library lease is required".into()));
    };
    let _files_guard = state.files_gate.lock().await;
    let mut held = lock(&state.lease);
    if !held.as_ref().is_some_and(|lease| lease.token == token && lease.expires_at > Instant::now()) {
        return Err(ApiError(StatusCode::CONFLICT, "the library lease expired or belongs to another editor".into()));
    }
    let mut handoffs = lock(&state.handoffs);
    let Some(handoff) =
        handoffs.get_mut(&id).filter(|handoff| handoff.old_lease == token && handoff.new_lease.is_none() && handoff.expires_at > Instant::now())
    else {
        return Err(ApiError(StatusCode::NOT_FOUND, "handoff request is no longer pending".into()));
    };
    let new_lease = Uuid::new_v4().to_string();
    let expires_in_seconds = state.lease_seconds;
    *held = Some(Lease { token: new_lease.clone(), expires_at: Instant::now() + Duration::from_secs(expires_in_seconds) });
    handoff.new_lease = Some(new_lease.clone());
    Ok(Json(LeaseReply { lease: new_lease, expires_in_seconds }))
}

/// Explicit timeout fallback: replaces the current lease after the new editor warns the user.
async fn takeover_lease(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<LeaseReply>, ApiError> {
    check_auth(&headers, &state)?;
    let _files_guard = state.files_gate.lock().await;
    let token = Uuid::new_v4().to_string();
    let expires_in_seconds = state.lease_seconds;
    *lock(&state.lease) = Some(Lease { token: token.clone(), expires_at: Instant::now() + Duration::from_secs(expires_in_seconds) });
    lock(&state.handoffs).clear();
    Ok(Json(LeaseReply { lease: token, expires_in_seconds }))
}

async fn renew_lease(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<LeaseReply>, ApiError> {
    check_auth(&headers, &state)?;
    let Some(token) = lease_token(&headers) else { return Err(ApiError(StatusCode::CONFLICT, "an active library lease is required".into())) };
    let mut held = lock(&state.lease);
    let now = Instant::now();
    match held.as_mut() {
        Some(lease) if lease.token == token && lease.expires_at > now => {
            lease.expires_at = now + Duration::from_secs(state.lease_seconds);
            Ok(Json(LeaseReply { lease: lease.token.clone(), expires_in_seconds: state.lease_seconds }))
        }
        Some(lease) if lease.expires_at <= now => {
            *held = None;
            Err(ApiError(StatusCode::CONFLICT, "the library lease expired; reopen the library".into()))
        }
        _ => Err(ApiError(StatusCode::CONFLICT, "the library lease belongs to another editor".into())),
    }
}

async fn release_lease(State(state): State<AppState>, headers: HeaderMap) -> Result<StatusCode, ApiError> {
    check_auth(&headers, &state)?;
    let Some(token) = lease_token(&headers) else { return Err(ApiError(StatusCode::CONFLICT, "an active library lease is required".into())) };
    let _files_guard = state.files_gate.lock().await;
    let mut held = lock(&state.lease);
    if held.as_ref().is_some_and(|lease| lease.token == token) {
        *held = None;
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(StatusCode::CONFLICT, "the library lease expired or belongs to another editor".into()))
    }
}

fn safe_relative(path: &str) -> Result<PathBuf, ApiError> {
    if path.is_empty() || path.contains('\\') || path.contains('\0') {
        return Err(ApiError::bad_request("invalid storage path"));
    }
    let mut rel = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) if !part.is_empty() => rel.push(part),
            _ => return Err(ApiError::bad_request("invalid storage path")),
        }
    }
    if rel.as_os_str().is_empty() {
        return Err(ApiError::bad_request("invalid storage path"));
    }
    Ok(rel)
}

fn target_path(root: &Path, rel: &Path, create_parents: bool) -> Result<PathBuf, ApiError> {
    let mut target = root.to_path_buf();
    let components: Vec<_> = rel.components().collect();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else { return Err(ApiError::bad_request("invalid storage path")) };
        target.push(name);
        if index + 1 == components.len() {
            match fs::symlink_metadata(&target) {
                Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
                    return Err(ApiError(StatusCode::CONFLICT, "storage path is not a regular file".into()));
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(ApiError::internal(e.to_string())),
            }
        } else {
            match fs::symlink_metadata(&target) {
                Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                    return Err(ApiError(StatusCode::CONFLICT, "storage path contains a non-directory entry".into()));
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && create_parents => {
                    fs::create_dir(&target).map_err(|e| ApiError::internal(e.to_string()))?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ApiError(StatusCode::NOT_FOUND, "not found".into())),
                Err(e) => return Err(ApiError::internal(e.to_string())),
            }
        }
    }
    Ok(target)
}

fn list_directory(root: &Path, path: &str) -> Result<Vec<String>, ApiError> {
    let rel = safe_relative(path)?;
    let mut target = root.to_path_buf();
    for component in rel.components() {
        let Component::Normal(name) = component else { return Err(ApiError::bad_request("invalid storage path")) };
        target.push(name);
        let meta = match fs::symlink_metadata(&target) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(ApiError::internal(e.to_string())),
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(ApiError(StatusCode::CONFLICT, "storage path is not a directory".into()));
        }
    }
    let entries = fs::read_dir(target).map_err(|e| ApiError::internal(e.to_string()))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| ApiError::internal(e.to_string()))?;
        let file_type = entry.file_type().map_err(|e| ApiError::internal(e.to_string()))?;
        if file_type.is_symlink() {
            continue;
        }
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

fn atomic_write(root: &Path, rel: &Path, bytes: &[u8]) -> Result<(), ApiError> {
    let target = target_path(root, rel, true)?;
    let parent = target.parent().ok_or_else(|| ApiError::bad_request("invalid storage path"))?;
    let name = target.file_name().ok_or_else(|| ApiError::bad_request("invalid storage path"))?.to_string_lossy();
    let tmp = parent.join(format!(".{name}.{}.tmp", Uuid::new_v4()));
    let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp).map_err(|e| ApiError::internal(e.to_string()))?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &target)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(ApiError::internal(e.to_string()));
    }
    Ok(())
}

fn read_file(root: &Path, rel: &Path) -> Result<Vec<u8>, ApiError> {
    let target = target_path(root, rel, false)?;
    let mut file = File::open(target).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound { ApiError(StatusCode::NOT_FOUND, "not found".into()) } else { ApiError::internal(e.to_string()) }
    })?;
    if file.metadata().map_err(|e| ApiError::internal(e.to_string()))?.len() > MAX_BODY_BYTES as u64 {
        return Err(ApiError(StatusCode::PAYLOAD_TOO_LARGE, "stored file exceeds the service limit".into()));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(bytes)
}

fn validate_original(path: &str, bytes: &[u8]) -> Result<(), ApiError> {
    let Some(hash) = path.strip_prefix("originals/") else { return Ok(()) };
    if hash.contains('/') || hash.len() != 32 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError::bad_request("invalid original content hash"));
    }
    let actual = lightcraft_preview::hash_bytes(bytes).to_string();
    if actual != hash {
        return Err(ApiError::bad_request("original bytes do not match their content hash"));
    }
    Ok(())
}

async fn get_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
) -> Result<Response, ApiError> {
    check_lease(&headers, &state)?;
    let rel = safe_relative(&path)?;
    let _guard = state.files_gate.lock().await;
    check_lease(&headers, &state)?;
    let root = state.root.clone();
    let bytes = tokio::task::spawn_blocking(move || read_file(&root, &rel)).await.map_err(|e| ApiError::internal(e.to_string()))??;
    Ok(([(CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

async fn head_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
) -> Result<StatusCode, ApiError> {
    check_lease(&headers, &state)?;
    let rel = safe_relative(&path)?;
    let _guard = state.files_gate.lock().await;
    check_lease(&headers, &state)?;
    let root = state.root.clone();
    tokio::task::spawn_blocking(move || {
        let target = target_path(&root, &rel, false)?;
        match fs::symlink_metadata(target) {
            Ok(meta) if meta.file_type().is_symlink() => Err(ApiError(StatusCode::CONFLICT, "storage path is a symbolic link".into())),
            Ok(meta) if meta.is_file() => Ok(StatusCode::OK),
            Ok(_) => Err(ApiError(StatusCode::CONFLICT, "storage path is not a regular file".into())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ApiError(StatusCode::NOT_FOUND, "not found".into())),
            Err(e) => Err(ApiError::internal(e.to_string())),
        }
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
}

async fn put_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    check_lease(&headers, &state)?;
    validate_original(&path, &body)?;
    let rel = safe_relative(&path)?;
    let _guard = state.files_gate.lock().await;
    check_lease(&headers, &state)?;
    let root = state.root.clone();
    tokio::task::spawn_blocking(move || atomic_write(&root, &rel, &body)).await.map_err(|e| ApiError::internal(e.to_string()))??;
    Ok(StatusCode::NO_CONTENT)
}

async fn append_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    check_lease(&headers, &state)?;
    if path != "library/catalog.log" {
        return Err(ApiError::bad_request("offset writes are only supported for library/catalog.log"));
    }
    let offset = headers
        .get(&OFFSET_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or_else(|| ApiError::bad_request("a valid x-lightcraft-offset header is required"))?;
    let rel = safe_relative(&path)?;
    let _guard = state.files_gate.lock().await;
    check_lease(&headers, &state)?;
    let root = state.root.clone();
    let bytes = body.to_vec();
    tokio::task::spawn_blocking(move || {
        let old = match read_file(&root, &rel) {
            Ok(old) => old,
            Err(ApiError(StatusCode::NOT_FOUND, _)) if offset == 0 => Vec::new(),
            Err(e) => return Err(e),
        };
        if old.len() as u64 != offset {
            return Err(ApiError(StatusCode::CONFLICT, format!("catalog log offset mismatch: expected {}, received {offset}", old.len())));
        }
        let total = old.len().checked_add(bytes.len()).ok_or_else(|| ApiError(StatusCode::PAYLOAD_TOO_LARGE, "catalog log is too large".into()))?;
        if total > MAX_BODY_BYTES {
            return Err(ApiError(StatusCode::PAYLOAD_TOO_LARGE, "catalog log is too large".into()));
        }
        let mut joined = old;
        joined.extend_from_slice(&bytes);
        atomic_write(&root, &rel, &joined)
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))??;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
) -> Result<StatusCode, ApiError> {
    check_lease(&headers, &state)?;
    let rel = safe_relative(&path)?;
    let _guard = state.files_gate.lock().await;
    check_lease(&headers, &state)?;
    let root = state.root.clone();
    tokio::task::spawn_blocking(move || {
        let target = target_path(&root, &rel, false)?;
        match fs::remove_file(target) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ApiError::internal(e.to_string())),
        }
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))??;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_files(State(state): State<AppState>, headers: HeaderMap, Query(query): Query<ListQuery>) -> Result<Json<Vec<String>>, ApiError> {
    check_lease(&headers, &state)?;
    let _guard = state.files_gate.lock().await;
    check_lease(&headers, &state)?;
    let root = state.root.clone();
    let names = tokio::task::spawn_blocking(move || list_directory(&root, &query.path)).await.map_err(|e| ApiError::internal(e.to_string()))??;
    Ok(Json(names))
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let api_token = required_env("LIGHTCRAFT_API_TOKEN")?;
    let origin = parse_origin(&env_value("LIGHTCRAFT_ALLOWED_ORIGIN", DEFAULT_ORIGIN))?;
    let root = PathBuf::from(env_value("LIGHTCRAFT_DATA_DIR", "./lightcraft-library-data"));
    fs::create_dir_all(&root)?;
    let root = fs::canonicalize(root)?;
    let lease_seconds = env_value("LIGHTCRAFT_LEASE_SECONDS", &DEFAULT_LEASE_SECONDS.to_string()).parse::<u64>()?;
    if !(45..=300).contains(&lease_seconds) {
        return Err("LIGHTCRAFT_LEASE_SECONDS must be between 45 and 300".into());
    }
    let bind: SocketAddr = env_value("LIGHTCRAFT_BIND", DEFAULT_BIND).parse()?;
    let state = AppState {
        root: Arc::new(root),
        api_token: Arc::from(api_token),
        lease_seconds,
        lease: Arc::new(Mutex::new(None)),
        files_gate: Arc::new(tokio::sync::Mutex::new(())),
        handoffs: Arc::new(Mutex::new(HashMap::new())),
    };
    let cors = CorsLayer::new()
        .allow_origin(origin)
        .allow_methods([Method::GET, Method::HEAD, Method::POST, Method::PUT, Method::PATCH, Method::DELETE])
        .allow_headers([AUTHORIZATION, CONTENT_TYPE, LEASE_HEADER, OFFSET_HEADER]);
    let app = Router::new()
        .route("/healthz", get(health))
        .route("/v1/lease", post(acquire_lease).delete(release_lease))
        .route("/v1/lease/handoff", post(request_handoff))
        .route("/v1/lease/handoff/pending", get(pending_handoff))
        .route("/v1/lease/handoff/{id}", get(handoff_status))
        .route("/v1/lease/handoff/{id}/cancel", post(cancel_handoff))
        .route("/v1/lease/handoff/{id}/ack", post(acknowledge_handoff))
        .route("/v1/lease/takeover", post(takeover_lease))
        .route("/v1/lease/renew", post(renew_lease))
        .route("/v1/files/{*path}", get(get_file).head(head_file).put(put_file).patch(append_file).delete(delete_file))
        .route("/v1/list", get(list_files))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(cors)
        .with_state(state.clone());
    let listener = TcpListener::bind(bind).await?;
    println!("LightCraft library service listening on {bind}; data directory: {}", state.root.display());
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("lightcraft-library-server: {error}");
        std::process::exit(1);
    }
}
