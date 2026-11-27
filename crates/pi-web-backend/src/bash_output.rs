//! Read bash execution output files referenced by a persisted session.
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::{Path as RoutePath, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;

use crate::{AppState, sessions::get_session_entries};

const MAX_INLINE_BYTES: u64 = 5 * 1024 * 1024;

fn failure(status: StatusCode, message: impl Into<String>) -> Response {
    (status, axum::Json(json!({"error":message.into()}))).into_response()
}

fn output_path(value: &str) -> Option<PathBuf> {
    let requested = Path::new(value);
    if !requested.is_absolute() {
        return None;
    }
    let parent = requested.parent()?;
    let file_name = requested.file_name()?.to_str()?;
    if !file_name.starts_with("pi-bash-") || !file_name.ends_with(".log") {
        return None;
    }
    let id = &file_name[8..file_name.len() - 4];
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')) {
        return None;
    }
    let temp_root = std::env::temp_dir().canonicalize().ok()?;
    let resolved_parent = parent.canonicalize().ok()?;
    if resolved_parent != temp_root {
        return None;
    }
    Some(resolved_parent.join(file_name))
}

fn open_regular_file(path: &Path) -> io::Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
    }
    Ok(file)
}

pub(super) async fn get(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(requested_path) = query.get("path") else {
        return failure(StatusCode::BAD_REQUEST, "path required");
    };
    let Some(path) = output_path(requested_path) else {
        return failure(StatusCode::BAD_REQUEST, "invalid path");
    };

    let (entries, _) = match get_session_entries(&state, &id).await {
        Ok(entries) => entries,
        Err(_) => return failure(StatusCode::NOT_FOUND, "Session not found"),
    };
    if !crate::session_file_reference::is_bash_output_path_referenced(&entries, &path.to_string_lossy()) {
        return failure(StatusCode::FORBIDDEN, "forbidden");
    }

    let file = match open_regular_file(&path) {
        Ok(file) => file,
        Err(_) => return failure(StatusCode::NOT_FOUND, "full output unavailable"),
    };
    let size = match file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(_) => return failure(StatusCode::NOT_FOUND, "full output unavailable"),
    };
    if query.get("download").is_some_and(|value| value == "1") {
        let file = tokio::fs::File::from_std(file);
        let stream = ReaderStream::new(file);
        let mut response = Response::new(Body::from_stream(stream));
        *response.status_mut() = StatusCode::OK;
        response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, HeaderValue::from_static("attachment; filename=\"bash-output.log\""));
        response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return response;
    }
    if size > MAX_INLINE_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            axum::Json(json!({
                "error": format!("Full output is too large to display (limit {MAX_INLINE_BYTES} bytes)"),
                "data": {"size":size,"maxBytes":MAX_INLINE_BYTES}
            })),
        )
            .into_response();
    }
    let mut limited = tokio::fs::File::from_std(file).take(MAX_INLINE_BYTES + 1);
    let mut bytes = Vec::with_capacity(size as usize);
    if limited.read_to_end(&mut bytes).await.is_err() {
        return failure(StatusCode::NOT_FOUND, "full output unavailable");
    }
    if bytes.len() as u64 > MAX_INLINE_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            axum::Json(json!({
                "error": format!("Full output is too large to display (limit {MAX_INLINE_BYTES} bytes)"),
                "data": {"size":bytes.len(),"maxBytes":MAX_INLINE_BYTES}
            })),
        )
            .into_response();
    }
    axum::Json(json!({"success":true,"data":{"output":String::from_utf8_lossy(&bytes)}})).into_response()
}
