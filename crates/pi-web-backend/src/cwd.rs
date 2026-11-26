//! Workspace picker and path validation endpoints.
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::web_backend::{AppState, path_access};

fn home_dir() -> PathBuf {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    home.map(PathBuf::from).unwrap_or_default()
}

pub(crate) fn normalize(path: &str) -> PathBuf {
    // Browser clients may submit Windows long-path values. Convert them at
    // the Web boundary so the core receives the ordinary path representation.
    #[cfg(windows)]
    let path = if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        path.to_string()
    };
    #[cfg(not(windows))]
    let path = path.to_string();

    let home = home_dir();
    if path == "~" {
        return home;
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    let path = PathBuf::from(path);
    if path.is_absolute() { path } else { std::env::current_dir().unwrap_or_default().join(path) }
}

fn response(status: StatusCode, error: impl Into<String>) -> Response {
    (status, Json(json!({"error":error.into()}))).into_response()
}

pub(super) async fn home() -> Json<Value> {
    Json(json!({"home":home_dir().to_string_lossy()}))
}

pub(super) async fn allowed_roots(State(state): State<AppState>) -> Json<Value> {
    Json(
        json!({"roots":path_access::allowed_roots(&state).iter().map(|path|path.to_string_lossy().to_string()).collect::<Vec<_>>()}),
    )
}

pub(super) async fn validate(body: Bytes) -> Response {
    let body = match serde_json::from_slice::<Value>(&body) {
        Ok(body) => body,
        Err(_) => return response(StatusCode::BAD_REQUEST, "Invalid JSON body"),
    };
    let cwd = body.get("cwd").and_then(Value::as_str).unwrap_or_default().trim();
    if cwd.is_empty() {
        return response(StatusCode::BAD_REQUEST, "Path is required");
    }
    let path = normalize(cwd);
    let canonical = match path.canonicalize() {
        Ok(path) => path,
        Err(_) => return response(StatusCode::BAD_REQUEST, format!("Directory does not exist: {cwd}")),
    };
    if !canonical.is_dir() {
        return response(StatusCode::BAD_REQUEST, format!("Path is not a directory: {cwd}"));
    }
    if let Err(error) = path_access::allow_root(&canonical) {
        return response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
    }
    let cwd = canonical.to_string_lossy().to_string();
    let project = crate::web_backend::sessions::session_project_fields(&cwd);
    let mut value = json!({"success":true,"cwd":cwd});
    value["projectRoot"] = project.get("projectRoot").cloned().unwrap_or(json!(cwd));
    value["projectKey"] = project.get("projectKey").cloned().unwrap_or(json!(cwd));
    Json(value).into_response()
}

fn browse_start(path: Option<&String>) -> PathBuf {
    match path.map(|path| path.trim()).filter(|path| !path.is_empty()) {
        Some(path) => normalize(path),
        None => home_dir(),
    }
}

fn is_windows_drive_picker(path: Option<&String>) -> bool {
    cfg!(windows) && path.is_none_or(|path| path.trim().is_empty())
}

fn list_drives() -> Vec<Value> {
    (b'A'..=b'Z')
        .map(|letter| PathBuf::from(format!("{}:\\", char::from(letter))))
        .filter(|path| path.is_dir())
        .map(|path| json!({"name":path.to_string_lossy().trim_end_matches(['\\','/']),"path":path.to_string_lossy()}))
        .collect()
}

fn parent(path: &Path) -> Option<String> {
    let normalized = path.to_path_buf();
    normalized.parent().filter(|parent| *parent != normalized).map(|parent| parent.to_string_lossy().to_string())
}

fn directories(path: &Path) -> Result<Vec<Value>, std::io::Error> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let entry_path = path.join(entry.file_name());
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        let is_directory = if file_type.is_dir() {
            true
        } else if file_type.is_symlink() {
            fs::metadata(&entry_path).is_ok_and(|metadata| metadata.is_dir())
        } else {
            false
        };
        if is_directory {
            entries.push(json!({"name":entry.file_name().to_string_lossy(),"path":entry_path.to_string_lossy()}));
        }
    }
    entries.sort_by(|a, b| a["name"].as_str().unwrap_or_default().cmp(b["name"].as_str().unwrap_or_default()));
    Ok(entries)
}

pub(super) async fn browse_get(Query(query): Query<HashMap<String, String>>) -> Response {
    let requested = query.get("path").map(|path| path.trim().to_string());
    if is_windows_drive_picker(requested.as_ref()) {
        return Json(json!({"path":"","parentPath":null,"drives":list_drives(),"directories":[]})).into_response();
    }
    let candidate = browse_start(requested.as_ref());
    let resolved = match candidate.canonicalize() {
        Ok(path) => path,
        Err(_) => return response(StatusCode::NOT_FOUND, "Directory does not exist"),
    };
    if !resolved.is_dir() {
        return response(StatusCode::BAD_REQUEST, "Path is not a directory");
    }
    match directories(&resolved) {
        Ok(directories) => {
            Json(json!({"path":resolved.to_string_lossy(),"parentPath":parent(&resolved),"directories":directories}))
                .into_response()
        }
        Err(error) => response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

fn create_error(error: &std::io::Error) -> Response {
    let (status, message) = match error.kind() {
        std::io::ErrorKind::AlreadyExists => {
            (StatusCode::CONFLICT, "A file or directory with this name already exists")
        }
        std::io::ErrorKind::NotFound => (StatusCode::NOT_FOUND, "Parent directory does not exist"),
        std::io::ErrorKind::PermissionDenied => (StatusCode::FORBIDDEN, "Permission denied"),
        std::io::ErrorKind::InvalidInput => (StatusCode::BAD_REQUEST, "Directory name is invalid"),
        _ => return response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    response(status, message)
}

pub(super) async fn browse_post(body: Bytes) -> Response {
    let body = match serde_json::from_slice::<Value>(&body) {
        Ok(body) => body,
        Err(_) => return response(StatusCode::BAD_REQUEST, "Invalid JSON body"),
    };
    let parent_path = body.get("path").and_then(Value::as_str).unwrap_or_default().trim();
    let name = body.get("name").and_then(Value::as_str).unwrap_or_default().trim();
    if parent_path.is_empty() {
        return response(StatusCode::BAD_REQUEST, "Parent path is required");
    }
    if name.is_empty() {
        return response(StatusCode::BAD_REQUEST, "Directory name is required");
    }
    if name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return response(StatusCode::BAD_REQUEST, "Directory name must be a single folder name");
    }
    let parent = match normalize(parent_path).canonicalize() {
        Ok(path) => path,
        Err(error) => return create_error(&error),
    };
    if !parent.is_dir() {
        return response(StatusCode::BAD_REQUEST, "Parent path is not a directory");
    }
    let created = parent.join(name);
    match fs::create_dir(&created) {
        Ok(()) => match created.canonicalize() {
            Ok(path) => Json(json!({"success":true,"path":path.to_string_lossy()})).into_response(),
            Err(error) => response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        },
        Err(error) => create_error(&error),
    }
}
