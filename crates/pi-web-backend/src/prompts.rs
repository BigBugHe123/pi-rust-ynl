use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::app_state::{AppState, failure};

#[derive(Deserialize)]
pub(crate) struct QueryOptions {
    scope: String,
    cwd: Option<String>,
    kind: String,
}

fn target(state: &AppState, query: &QueryOptions) -> Result<std::path::PathBuf, Response> {
    let name = match query.kind.as_str() {
        "system" => "SYSTEM.md",
        "append_system" => "APPEND_SYSTEM.md",
        "agents" => "AGENTS.md",
        _ => return Err(failure(StatusCode::BAD_REQUEST, "Unknown prompt document")),
    };
    let root = match query.scope.as_str() {
        "global" => state.agent_dir.clone(),
        "project" => {
            let Some(cwd) = query.cwd.as_deref() else {
                return Err(failure(StatusCode::BAD_REQUEST, "Project cwd required"));
            };
            let candidate = std::path::Path::new(cwd);
            if !candidate.is_dir() {
                return Err(failure(StatusCode::BAD_REQUEST, "Project cwd must be a directory"));
            }
            let authorized = crate::path_access::allowed_existing_path(state, candidate).or_else(|| {
                let parent = candidate.parent()?;
                let allowed_parent = crate::path_access::allowed_existing_path(state, parent)?;
                let name = candidate.file_name()?;
                Some(allowed_parent.join(name))
            });
            let Some(cwd) = authorized else {
                return Err(failure(StatusCode::FORBIDDEN, "Project directory is not allowed"));
            };
            cwd.join(".pi")
        }
        _ => return Err(failure(StatusCode::BAD_REQUEST, "Unknown prompt scope")),
    };
    Ok(root.join(name))
}

pub(crate) async fn get(State(state): State<AppState>, Query(query): Query<QueryOptions>) -> Response {
    let path = match target(&state, &query) {
        Ok(path) => path,
        Err(response) => return response,
    };
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => Json(json!({"content": content, "exists": true})).into_response(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Json(json!({"content":"", "exists":false})).into_response()
        }
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

pub(crate) async fn put(State(state): State<AppState>, Query(query): Query<QueryOptions>, body: Bytes) -> Response {
    let path = match target(&state, &query) {
        Ok(path) => path,
        Err(response) => return response,
    };
    if body.len() > 1024 * 1024 {
        return failure(StatusCode::PAYLOAD_TOO_LARGE, "Prompt document exceeds 1 MiB");
    }
    let Some(parent) = path.parent() else { return failure(StatusCode::BAD_REQUEST, "Invalid prompt path") };
    if let Err(error) = tokio::fs::create_dir_all(parent).await {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
    }
    match tokio::fs::write(&path, &body).await {
        Ok(()) => Json(json!({"success":true})).into_response(),
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

pub(crate) async fn delete(State(state): State<AppState>, Query(query): Query<QueryOptions>) -> Response {
    let path = match target(&state, &query) {
        Ok(path) => path,
        Err(response) => return response,
    };
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Json(json!({"success":true})).into_response(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Json(json!({"success":true})).into_response(),
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}
