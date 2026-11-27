//! Open an allowed workspace in the file manager on the backend host.
use std::path::{Path, PathBuf};
use std::process::Command;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::{AppState, path_access};

fn platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    }
}

fn host_is_loopback(host: Option<&str>) -> bool {
    let Some(host) = host else { return false };
    let host = host.trim().to_ascii_lowercase();
    let name = if host.starts_with('[') {
        host.split_once(']').map(|(inside, _)| inside.trim_start_matches('[')).unwrap_or("")
    } else {
        host.rsplit_once(':')
            .filter(|(name, port)| !name.contains(':') && port.parse::<u16>().is_ok())
            .map_or(host.as_str(), |(name, _)| name)
    };
    name == "localhost"
        || name.ends_with(".localhost")
        || name == "::1"
        || name.strip_prefix("127.").is_some_and(|rest| {
            let octets: Vec<_> = rest.split('.').collect();
            octets.len() == 3 && octets.iter().all(|octet| octet.parse::<u8>().is_ok())
        })
}

fn availability(host: Option<&str>) -> (bool, Option<&'static str>) {
    if !host_is_loopback(host) {
        return (false, Some("remote"));
    }
    if !matches!(platform(), "win32" | "darwin" | "linux") {
        return (false, Some("unsupported-platform"));
    }
    (true, None)
}

pub(super) async fn get(request: Request) -> Json<Value> {
    let (supported, reason) =
        availability(request.headers().get("x-pi-web-host").and_then(|value| value.to_str().ok()));
    Json(json!({"supported":supported,"reason":reason,"platform":platform()}))
}

fn denied(status: StatusCode, error: impl Into<String>) -> Response {
    (status, Json(json!({"error":error.into()}))).into_response()
}

fn is_within(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

fn launch(path: &Path) -> std::io::Result<()> {
    let mut command = if cfg!(target_os = "windows") {
        let mut command = Command::new("explorer.exe");
        command.arg(path);
        command
    } else if cfg!(target_os = "macos") {
        let mut command = Command::new("open");
        command.arg(path);
        command
    } else if cfg!(target_os = "linux") {
        let mut command = Command::new("xdg-open");
        command.arg(path);
        command
    } else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("Unsupported platform: {}", platform()),
        ));
    };
    command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    command.spawn().map(|_| ())
}

pub(super) async fn post(State(state): State<AppState>, request: Request) -> Response {
    let (supported, reason) =
        availability(request.headers().get("x-pi-web-host").and_then(|value| value.to_str().ok()));
    if !supported {
        return denied(StatusCode::FORBIDDEN, reason.unwrap_or("unsupported-platform"));
    }
    let body = match axum::body::to_bytes(request.into_body(), 1024 * 1024).await {
        Ok(body) => body,
        Err(error) => return denied(StatusCode::BAD_REQUEST, error.to_string()),
    };
    let body: Value = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(error) => return denied(StatusCode::BAD_REQUEST, error.to_string()),
    };
    let Some(cwd) = body.get("cwd").and_then(Value::as_str).map(str::trim).filter(|cwd| !cwd.is_empty()) else {
        return denied(StatusCode::BAD_REQUEST, "cwd-required");
    };
    let target = PathBuf::from(cwd);
    // Resolve and authorize before checking the directory type to avoid
    // exposing whether a path outside the configured roots exists.
    let canonical = match target.canonicalize() {
        Ok(path) => path,
        Err(_) => return denied(StatusCode::FORBIDDEN, "access-denied"),
    };
    if !is_within(&canonical, &path_access::allowed_roots(&state)) {
        return denied(StatusCode::FORBIDDEN, "access-denied");
    }
    if !canonical.is_dir() {
        return denied(StatusCode::BAD_REQUEST, "not-a-directory");
    }
    match launch(&canonical) {
        Ok(()) => Json(json!({"opened":true})).into_response(),
        Err(error) => denied(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}
