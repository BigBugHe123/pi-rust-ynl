//! Global Pi Web subagent settings stored in `$PI_CODING_AGENT_DIR/agents/settings.json`.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use crate::AppState;

const DEFAULT_MAX_CONCURRENT: u64 = 10;
const MAX_MAX_CONCURRENT: u64 = 32;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);
static WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn path(state: &AppState) -> PathBuf {
    state.agent_dir.join("agents").join("settings.json")
}

fn read(path: &Path) -> Result<Map<String, Value>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => return Err(error.to_string()),
    };
    serde_json::from_str::<Value>(&text)
        .map_err(|error| format!("Invalid subagent settings: {error}"))?
        .as_object()
        .cloned()
        .ok_or_else(|| "Invalid subagent settings: expected an object".to_owned())
}

fn max_concurrent(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_u64).filter(|n| (1..=MAX_MAX_CONCURRENT).contains(n)).unwrap_or(DEFAULT_MAX_CONCURRENT)
}

fn response(stored: &Map<String, Value>) -> Value {
    json!({
        "enabled": stored.get("builtInEnabled").and_then(Value::as_bool).unwrap_or(false),
        "maxConcurrent": max_concurrent(stored.get("maxConcurrent")),
    })
}

fn atomic_write(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let destination = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            fs::canonicalize(path).map_err(|_| format!("Failed to write {}: dangling symbolic link", path.display()))?
        }
        _ => path.to_path_buf(),
    };
    let parent = destination.parent().ok_or_else(|| format!("Invalid path: {}", destination.display()))?;
    let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".subagent-settings.{}.{}.tmp", std::process::id(), id));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp).map_err(|error| error.to_string())?;
        file.write_all(serde_json::to_string_pretty(value).map_err(|error| error.to_string())?.as_bytes())
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        #[cfg(windows)]
        if destination.exists() {
            fs::remove_file(&destination).map_err(|error| error.to_string())?;
        }
        fs::rename(&temp, &destination).map_err(|error| error.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(|error: String| format!("Failed to write {}: {error}", path.display()))
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error":message.into()}))).into_response()
}

pub async fn get(State(state): State<AppState>) -> Response {
    match read(&path(&state)) {
        Ok(stored) => Json(response(&stored)).into_response(),
        Err(message) => error(StatusCode::INTERNAL_SERVER_ERROR, message),
    }
}

pub async fn put(State(state): State<AppState>, Json(input): Json<Value>) -> Response {
    let Some(input) = input.as_object() else {
        return error(StatusCode::BAD_REQUEST, "Expected a JSON object");
    };
    let enabled = input.get("enabled");
    let max = input.get("maxConcurrent");
    if enabled.is_none() && max.is_none() {
        return error(StatusCode::BAD_REQUEST, "enabled or maxConcurrent is required");
    }
    if enabled.is_some_and(|value| !value.is_boolean()) {
        return error(StatusCode::BAD_REQUEST, "enabled must be a boolean");
    }
    if let Some(value) = max {
        if !value.as_u64().is_some_and(|n| (1..=MAX_MAX_CONCURRENT).contains(&n)) {
            return error(StatusCode::BAD_REQUEST, "maxConcurrent must be an integer between 1 and 32");
        }
    }
    let _guard = WRITE_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|error| error.into_inner());
    let path = path(&state);
    let mut stored = match read(&path) {
        Ok(stored) => stored,
        Err(message) => return error(StatusCode::INTERNAL_SERVER_ERROR, message),
    };
    if let Some(value) = enabled {
        stored.insert("version".into(), json!(1));
        stored.insert("builtInEnabled".into(), value.clone());
    }
    if let Some(value) = max {
        stored.insert("version".into(), json!(1));
        stored.insert("maxConcurrent".into(), value.clone());
    }
    if let Err(message) = atomic_write(&path, &Value::Object(stored.clone())) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, message);
    }
    Json(response(&stored)).into_response()
}
