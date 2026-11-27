use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::to_bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::{
    hmac,
    rand::{SecureRandom, SystemRandom},
};
use serde_json::{Value, json};

use super::config_edit;
use crate::AppState;

const MAX_STATUS_RECORDS: usize = 500;
const MAX_UNDO_RECORDS: usize = 100;
const UNDO_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct StatusKey {
    scope: String,
    source_path: String,
    name: String,
    config_key: String,
}

#[derive(Clone)]
struct StatusRecord {
    key: StatusKey,
    status: Value,
}

static STATUSES: OnceLock<Mutex<VecDeque<StatusRecord>>> = OnceLock::new();
static CONFIG_KEY_SECRET: OnceLock<Vec<u8>> = OnceLock::new();

fn canonical_json(value: &Value, depth: usize, output: &mut String) {
    if (value.is_array() || value.is_object()) && depth >= 64 {
        output.push_str("\"<too deep>\"");
        return;
    }
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => output.push_str(&value.to_string()),
        Value::String(value) => output.push_str(&serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())),
        Value::Array(items) => {
            output.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                canonical_json(item, depth + 1, output);
            }
            output.push(']');
        }
        Value::Object(entries) => {
            let mut keys = entries.keys().collect::<Vec<_>>();
            keys.sort();
            output.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(key).unwrap_or_else(|_| "\"\"".into()));
                output.push(':');
                if let Some(value) = entries.get(key) {
                    canonical_json(value, depth + 1, output);
                }
            }
            output.push('}');
        }
    }
}

/// Stable only for this backend process, so a status never exposes a plain hash of secret config.
pub fn config_key(value: &Value) -> String {
    let secret = CONFIG_KEY_SECRET.get_or_init(|| {
        let mut secret = vec![0u8; 32];
        SystemRandom::new().fill(&mut secret).expect("operating system entropy is required for MCP config keys");
        secret
    });
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    let mut canonical = String::new();
    canonical_json(value, 0, &mut canonical);
    URL_SAFE_NO_PAD.encode(hmac::sign(&key, canonical.as_bytes()).as_ref())
}

pub async fn get_config_key(Json(body): Json<Value>) -> Response {
    let Some(value) = body.get("value") else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"value is required"}))).into_response();
    };
    Json(json!({"configKey":config_key(value)})).into_response()
}

fn statuses() -> &'static Mutex<VecDeque<StatusRecord>> {
    STATUSES.get_or_init(|| Mutex::new(VecDeque::new()))
}

pub fn lookup_status(scope: &str, source_path: &str, name: &str, config_key: &str) -> Option<Value> {
    let key = StatusKey {
        scope: scope.to_owned(),
        source_path: source_path.to_owned(),
        name: name.to_owned(),
        config_key: config_key.to_owned(),
    };
    statuses()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .find(|record| record.key == key)
        .map(|record| record.status.clone())
}

fn status_key(value: &Value) -> Option<StatusKey> {
    Some(StatusKey {
        scope: value.get("scope")?.as_str()?.to_owned(),
        source_path: value.get("sourcePath")?.as_str()?.to_owned(),
        name: value.get("name")?.as_str()?.to_owned(),
        config_key: value.get("configKey")?.as_str()?.to_owned(),
    })
}

pub async fn record_status(Json(body): Json<Value>) -> Response {
    let Some(key) = status_key(&body) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"scope, sourcePath, name, and configKey are required"})))
            .into_response();
    };
    let Some(status) = body.get("status").filter(|status| status.is_object()).cloned() else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"status must be an object"}))).into_response();
    };
    let mut held = statuses().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(index) = held.iter().position(|record| record.key == key) {
        held.remove(index);
    }
    held.push_back(StatusRecord { key, status });
    while held.len() > MAX_STATUS_RECORDS {
        held.pop_front();
    }
    Json(json!({"ok":true})).into_response()
}

pub async fn query_status(Json(body): Json<Value>) -> Response {
    let Some(servers) = body.get("servers").and_then(Value::as_array) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"servers must be an array"}))).into_response();
    };
    if servers.len() > MAX_STATUS_RECORDS {
        return (StatusCode::PAYLOAD_TOO_LARGE, Json(json!({"error":"Too many MCP status entries"}))).into_response();
    }
    let keys = servers.iter().map(status_key).collect::<Vec<_>>();
    let listed = keys.iter().flatten().cloned().collect::<std::collections::HashSet<_>>();
    let read_files = body
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|file| Some((file.get("scope")?.as_str()?.to_owned(), file.get("path")?.as_str()?.to_owned())))
        .collect::<std::collections::HashSet<_>>();
    let mut held = statuses().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    held.retain(|record| {
        !read_files.contains(&(record.key.scope.clone(), record.key.source_path.clone()))
            || listed.contains(&record.key)
    });
    let results = keys
        .iter()
        .map(|key| {
            key.as_ref()
                .and_then(|key| held.iter().find(|record| &record.key == key).map(|record| record.status.clone()))
        })
        .collect::<Vec<_>>();
    Json(json!({"statuses":results})).into_response()
}

pub async fn forget_status_entry(Json(body): Json<Value>) -> Response {
    let (Some(scope), Some(source_path), Some(name)) = (
        body.get("scope").and_then(Value::as_str),
        body.get("sourcePath").and_then(Value::as_str),
        body.get("name").and_then(Value::as_str),
    ) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"scope, sourcePath, and name are required"})))
            .into_response();
    };
    let mut held = statuses().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    held.retain(|record| {
        !(record.key.scope == scope && record.key.source_path == source_path && record.key.name == name)
    });
    Json(json!({"ok":true})).into_response()
}

#[derive(Clone)]
struct HeldRemoval {
    scope: String,
    name: String,
    cwd: Option<String>,
    path: String,
    entry: Value,
    index: usize,
    expires_at: Instant,
}

struct UndoState {
    records: HashMap<String, HeldRemoval>,
    order: VecDeque<String>,
}

static UNDOS: OnceLock<Mutex<UndoState>> = OnceLock::new();

fn undos() -> &'static Mutex<UndoState> {
    UNDOS.get_or_init(|| Mutex::new(UndoState { records: HashMap::new(), order: VecDeque::new() }))
}

fn new_token() -> Result<String, String> {
    let mut bytes = [0u8; 24];
    SystemRandom::new().fill(&mut bytes).map_err(|_| "Could not create an undo token".to_owned())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn insert_undo(token: String, record: HeldRemoval) {
    let mut state = undos().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    state.records.retain(|_, held| held.expires_at > Instant::now());
    let live_tokens = state.records.keys().cloned().collect::<std::collections::HashSet<_>>();
    state.order.retain(|held| live_tokens.contains(held));
    while state.records.len() >= MAX_UNDO_RECORDS {
        if let Some(oldest) = state.order.pop_front() {
            state.records.remove(&oldest);
        } else {
            break;
        }
    }
    state.order.push_back(token.clone());
    state.records.insert(token, record);
}

fn take_undo(token: &str) -> Option<HeldRemoval> {
    let mut state = undos().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let record = state.records.remove(token)?;
    state.order.retain(|held| held != token);
    (record.expires_at > Instant::now()).then_some(record)
}

async fn edit_json(state: AppState, request: Value) -> Result<(StatusCode, Value), Response> {
    let response = config_edit::edit(State(state), Json(request)).await;
    let status = response.status();
    let body = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, Json(json!({"error":error.to_string()}))).into_response())?;
    let value = serde_json::from_slice(&body).map_err(|error| {
        (StatusCode::BAD_GATEWAY, Json(json!({"error":format!("Invalid Rust MCP edit response: {error}")})))
            .into_response()
    })?;
    Ok((status, value))
}

pub async fn remove_with_undo(State(state): State<AppState>, Json(mut body): Json<Value>) -> Response {
    let Some(scope) = body
        .get("scope")
        .and_then(Value::as_str)
        .filter(|scope| *scope == "global" || *scope == "project")
        .map(str::to_owned)
    else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"scope must be global or project"}))).into_response();
    };
    let Some(name) = body.get("name").and_then(Value::as_str).filter(|name| !name.is_empty()).map(str::to_owned) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"name must be a server name"}))).into_response();
    };
    body["remove"] = Value::Bool(true);
    let (status, result) = match edit_json(state, body.clone()).await {
        Ok(result) => result,
        Err(response) => return response,
    };
    if !status.is_success() {
        return (status, Json(result)).into_response();
    }
    let (Some(path), Some(entry), Some(index)) =
        (result.get("path").and_then(Value::as_str), result.get("entry"), result.get("index").and_then(Value::as_u64))
    else {
        return (StatusCode::BAD_GATEWAY, Json(json!({"error":"Rust MCP edit returned an incomplete removal"})))
            .into_response();
    };
    let token = match new_token() {
        Ok(token) => token,
        Err(error) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":error}))).into_response(),
    };
    let record = HeldRemoval {
        scope,
        name,
        cwd: body.get("cwd").and_then(Value::as_str).map(str::to_owned),
        path: path.to_owned(),
        entry: entry.clone(),
        index: index as usize,
        expires_at: Instant::now() + UNDO_TTL,
    };
    insert_undo(token.clone(), record);
    Json(json!({"ok":true,"path":path,"token":token,"expiresInMs":UNDO_TTL.as_millis()})).into_response()
}

pub async fn restore_undo(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let Some(token) = body.get("token").and_then(Value::as_str).filter(|token| !token.is_empty() && token.len() <= 100)
    else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"token must be an undo token"}))).into_response();
    };
    let Some(record) = take_undo(token) else {
        return (StatusCode::GONE, Json(json!({"error":"Nothing to undo: the removal is unknown, already undone, or older than 60 seconds","reason":"undo-unavailable"}))).into_response();
    };
    let mut request = json!({
        "scope":record.scope,
        "name":record.name,
        "operation":"restore",
        "entry":record.entry,
        "index":record.index,
    });
    if let Some(cwd) = record.cwd.as_deref() {
        request["cwd"] = json!(cwd);
    }
    request["allowedRoots"] = json!(crate::path_access::allowed_roots(&state));
    let (status, mut result) = match edit_json(state, request).await {
        Ok(result) => result,
        Err(response) => {
            insert_undo(token.to_owned(), record);
            return response;
        }
    };
    if status.is_success() {
        return Json(json!({"ok":true,"restored":{"scope":record.scope,"name":record.name}})).into_response();
    }
    if result.get("reason").and_then(Value::as_str) == Some("name-taken") {
        result["reason"] = json!("undo-name-taken");
        result["error"] =
            json!(format!("{} defines \"{}\" again, so the removal is not undone", record.path, record.name));
    }
    insert_undo(token.to_owned(), record);
    (status, Json(result)).into_response()
}
