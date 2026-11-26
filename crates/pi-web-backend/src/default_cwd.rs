use std::path::PathBuf;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use pi_coding_agent::resources::paths::home_dir;
use serde_json::{Value, json};

use crate::web_backend::{AppState, failure};

pub(super) async fn create(State(_state): State<AppState>, Json(body): Json<Value>) -> Response {
    let Some(date) = body.get("date").and_then(Value::as_str) else {
        return failure(StatusCode::BAD_REQUEST, "date must be YYYYMMDD");
    };
    if date.len() != 8 || !date.bytes().all(|byte| byte.is_ascii_digit()) {
        return failure(StatusCode::BAD_REQUEST, "date must be YYYYMMDD");
    }
    let cwd: PathBuf = home_dir().join("pi-cwd").join(date);
    if let Err(error) = std::fs::create_dir_all(&cwd) {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
    }
    Json(json!({"cwd":cwd})).into_response()
}
