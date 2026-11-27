use std::fs;
use std::path::{Path, PathBuf};

use axum::Json;
use axum::body::to_bytes;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use pi_coding_agent::resources::paths::home_dir;
use pi_coding_agent::resources::trust::{ProjectTrustStore, has_trust_requiring_project_resources};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{AppState, failure};

const TRUST_ENTRIES: &[&str] =
    &["settings.json", "mcp.json", "extensions", "skills", "prompts", "themes", "SYSTEM.md", "APPEND_SYSTEM.md"];
const SCAN_DEPTH: usize = 4;
const SCAN_MAX_FOLDERS: usize = 2000;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustBreadth {
    pub kind: &'static str,
    pub path: PathBuf,
}

fn real_path_or(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn same_path(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy().eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

fn has_trust_relevant_entry(folder: &Path) -> bool {
    let config_dir = folder.join(".pi");
    if fs::symlink_metadata(&config_dir).is_ok_and(|metadata| metadata.file_type().is_symlink())
        && config_dir.canonicalize().is_err()
    {
        return true;
    }
    if TRUST_ENTRIES.iter().any(|entry| fs::symlink_metadata(config_dir.join(entry)).is_ok()) {
        return true;
    }
    let home_skills = real_path_or(&home_dir()).join(".agents").join("skills");
    let mut current = folder.to_path_buf();
    loop {
        let skills = current.join(".agents").join("skills");
        if !same_path(&skills, &home_skills) && fs::symlink_metadata(&skills).is_ok() {
            return true;
        }
        let Some(parent) = current.parent() else { break };
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    false
}

fn nested_trust_project(agent_dir: &Path, root: &Path) -> Result<Option<TrustBreadth>, String> {
    let store = ProjectTrustStore::new(agent_dir);
    let mut count = 0usize;
    let mut level = vec![root.to_path_buf()];
    for depth in 0..=SCAN_DEPTH {
        if level.is_empty() {
            break;
        }
        let mut next = Vec::new();
        for dir in level {
            if count >= SCAN_MAX_FOLDERS {
                return Ok(Some(TrustBreadth { kind: "too-many-folders", path: root.to_path_buf() }));
            }
            count += 1;
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            let entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
            if depth > 0 && has_trust_relevant_entry_shallow(&dir) && store.get_entry(&dir).ok().flatten().is_none() {
                return Ok(Some(TrustBreadth { kind: "contains-project", path: dir }));
            }
            if depth < SCAN_DEPTH {
                for entry in entries {
                    if matches!(entry.file_name().to_str(), Some("node_modules" | ".git" | ".pi" | ".agents")) {
                        continue;
                    }
                    if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                        next.push(entry.path());
                    }
                }
            }
        }
        level = next;
    }
    Ok(None)
}

fn has_trust_relevant_entry_shallow(folder: &Path) -> bool {
    let config_dir = folder.join(".pi");
    if fs::symlink_metadata(&config_dir).is_ok_and(|metadata| metadata.file_type().is_symlink())
        && config_dir.canonicalize().is_err()
    {
        return true;
    }
    TRUST_ENTRIES.iter().any(|entry| fs::symlink_metadata(config_dir.join(entry)).is_ok())
        || fs::symlink_metadata(folder.join(".agents").join("skills")).is_ok()
}

pub fn fresh_folder_breadth(
    agent_dir: &Path,
    cwd: &Path,
    known_folders: &[PathBuf],
) -> Result<Option<TrustBreadth>, String> {
    let folder = real_path_or(cwd);
    if folder.parent().is_none() {
        return Ok(Some(TrustBreadth { kind: "root", path: folder }));
    }
    let home = real_path_or(&home_dir());
    if same_path(&folder, &home) {
        return Ok(Some(TrustBreadth { kind: "home", path: folder }));
    }
    if home.starts_with(&folder) {
        return Ok(Some(TrustBreadth { kind: "contains-home", path: home }));
    }
    let agent = real_path_or(agent_dir);
    if agent.starts_with(&folder) {
        return Ok(Some(TrustBreadth { kind: "contains-agent-dir", path: agent }));
    }
    for known in known_folders {
        let known = real_path_or(known);
        if !same_path(&known, &folder) && known.starts_with(&folder) {
            return Ok(Some(TrustBreadth { kind: "contains-folder", path: known }));
        }
    }
    nested_trust_project(agent_dir, &folder)
}

pub fn folder_has_trust_relevant_entries(cwd: &Path) -> bool {
    has_trust_relevant_entry(&real_path_or(cwd))
}

pub(crate) fn resources_trusted(cwd: &Path, agent_dir: &Path) -> Result<bool, String> {
    let requires_trust = has_trust_requiring_project_resources(cwd, &home_dir());
    let decision = match ProjectTrustStore::new(agent_dir).get(cwd) {
        Ok(decision) => decision,
        Err(error) if requires_trust => return Err(error.to_string()),
        Err(_) => return Ok(true),
    };
    Ok(if requires_trust { decision == Some(true) } else { decision.unwrap_or(true) })
}

pub(crate) fn is_trusted_for_resources(cwd: &Path, agent_dir: &Path) -> bool {
    resources_trusted(cwd, agent_dir).unwrap_or(false)
}

pub async fn fresh_check(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let Some(raw) = body.get("cwd").and_then(Value::as_str).filter(|value| !value.is_empty()) else {
        return failure(StatusCode::BAD_REQUEST, "cwd required");
    };
    let cwd = match project_dir(&state, raw) {
        Ok(cwd) => cwd,
        Err(response) => return response,
    };
    let roots = crate::path_access::allowed_roots(&state);
    let breadth = match fresh_folder_breadth(&state.agent_dir, &cwd, &roots) {
        Ok(breadth) => breadth,
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":error,"reason":"trust-unreadable"})))
                .into_response();
        }
    };
    if let Some(breadth) = breadth {
        return Json(json!({"allowed":false,"reason":"trust-too-broad","breadth":breadth})).into_response();
    }
    let entry = match ProjectTrustStore::new(&state.agent_dir).get_entry(&cwd) {
        Ok(entry) => entry,
        Err(error) => {
            return (StatusCode::CONFLICT, Json(json!({"error":error.to_string(),"reason":"trust-unreadable"})))
                .into_response();
        }
    };
    if entry.is_some()
        || has_trust_requiring_project_resources(&cwd, &home_dir())
        || folder_has_trust_relevant_entries(&cwd)
    {
        return Json(json!({"allowed":false,"reason":"folder-not-fresh"})).into_response();
    }
    Json(json!({"allowed":true})).into_response()
}

pub(crate) fn status(state: &AppState, cwd: &Path) -> Result<Value, String> {
    let cwd = cwd.canonicalize().map_err(|error| error.to_string())?;
    let requires = has_trust_requiring_project_resources(&cwd, &home_dir());
    let entry = ProjectTrustStore::new(&state.agent_dir).get_entry(&cwd).map_err(|error| error.to_string());
    let entry = match entry {
        Ok(entry) => entry,
        Err(error) if requires => return Err(error),
        Err(error) => {
            return Ok(
                json!({"requiresTrust":false,"trusted":true,"decision":null,"inherited":false,"decisionError":error}),
            );
        }
    };
    let decision = entry.as_ref().map(|entry| entry.decision);
    let path = entry.as_ref().map(|entry| entry.path.clone());
    let inherited = path.as_ref().is_some_and(|path| path != &cwd);
    Ok(json!({
        "requiresTrust":requires,
        "trusted":if requires { decision == Some(true) } else { decision.unwrap_or(true) },
        "decision":decision,
        "decisionPath":path,
        "inherited":inherited,
    }))
}

fn project_dir(state: &AppState, raw: &str) -> Result<PathBuf, Response> {
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(failure(StatusCode::BAD_REQUEST, "cwd must be an absolute path"));
    }
    let canonical = path
        .canonicalize()
        .map_err(|_| failure(StatusCode::BAD_REQUEST, format!("Directory does not exist: {}", path.display())))?;
    if !canonical.is_dir() {
        return Err(failure(StatusCode::BAD_REQUEST, format!("Not a directory: {}", canonical.display())));
    }
    crate::path_access::allowed_existing_path(state, &canonical)
        .ok_or_else(|| failure(StatusCode::FORBIDDEN, "Access denied"))
}

async fn project_listing(state: &AppState, cwd: &Path) -> (Option<Value>, Vec<Value>, Option<String>) {
    let roots =
        crate::path_access::allowed_roots(state).into_iter().map(|root| root.to_string_lossy().into_owned()).collect();
    let cwd = cwd.to_string_lossy().into_owned();
    let response = crate::mcp::config_read::overview(
        State(state.clone()),
        Query(crate::mcp::config_read::SourceQuery { cwd: Some(cwd), allowed_roots: roots }),
    )
    .await;
    let status = response.status();
    let bytes = match to_bytes(response.into_body(), 2 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(error) => return (None, Vec::new(), Some(error.to_string())),
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => return (None, Vec::new(), Some(error.to_string())),
    };
    if !status.is_success() {
        return (
            None,
            Vec::new(),
            Some(value.get("error").and_then(Value::as_str).unwrap_or("Could not list project MCP servers").to_owned()),
        );
    }
    let file = value
        .get("files")
        .and_then(Value::as_array)
        .and_then(|files| files.iter().find(|file| file.get("scope").and_then(Value::as_str) == Some("project")))
        .cloned();
    let servers = value
        .get("servers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|server| server.get("scope").and_then(Value::as_str) == Some("project"))
        .cloned()
        .collect();
    (file, servers, None)
}

fn with_project_listing(
    mut value: Value,
    file: Option<Value>,
    servers: Vec<Value>,
    mcp_error: Option<String>,
) -> Value {
    if let Some(file) = file {
        value["mcpFile"] = file;
    }
    value["mcpServers"] = json!(servers);
    if let Some(error) = mcp_error {
        value["mcpError"] = json!(error);
    }
    value
}

pub async fn get(
    State(state): State<AppState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(raw) = query.get("cwd").filter(|value| !value.is_empty()) else {
        return failure(StatusCode::BAD_REQUEST, "cwd required");
    };
    let cwd = match project_dir(&state, raw) {
        Ok(cwd) => cwd,
        Err(response) => return response,
    };
    let (file, servers, mcp_error) = project_listing(&state, &cwd).await;
    match status(&state, &cwd) {
        Ok(value) => Json(with_project_listing(value, file, servers, mcp_error)).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(with_project_listing(json!({"error":error,"reason":"trust-unreadable"}), file, servers, mcp_error)),
        )
            .into_response(),
    }
}

pub async fn post(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let Some(raw) = body.get("cwd").and_then(Value::as_str).filter(|value| !value.is_empty()) else {
        return failure(StatusCode::BAD_REQUEST, "cwd required");
    };
    let cwd = match project_dir(&state, raw) {
        Ok(cwd) => cwd,
        Err(response) => return response,
    };
    if !has_trust_requiring_project_resources(&cwd, &home_dir()) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"This project has no resources that require trust","reason":"trust-not-required"})),
        )
            .into_response();
    }
    let sessions = crate::lock(&state.sessions).values().cloned().collect::<Vec<_>>();
    let mut busy = false;
    for session in &sessions {
        if crate::sessions::same_session_cwd(&session.cwd().await, &cwd) {
            match session.is_busy().await {
                Ok(true) => {
                    busy = true;
                    break;
                }
                Ok(false) => {}
                Err(error) => {
                    return failure(StatusCode::BAD_GATEWAY, format!("Could not query session state: {error:#}"));
                }
            }
        }
    }
    if busy {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"Wait for the active session to finish before trusting this project","reason":"session-busy"})),
        )
            .into_response();
    }
    if let Err(error) = ProjectTrustStore::new(&state.agent_dir).set(&cwd, Some(true)) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":error.to_string(),"reason":"trust-unreadable"})),
        )
            .into_response();
    }
    crate::models::cache::invalidate();
    let candidates =
        crate::lock(&state.sessions).iter().map(|(id, session)| (id.clone(), session.clone())).collect::<Vec<_>>();
    let mut ids = Vec::new();
    for (id, session) in candidates {
        if crate::sessions::same_session_cwd(&session.cwd().await, &cwd) {
            ids.push(id);
        }
    }
    let disposed = {
        let mut sessions = crate::lock(&state.sessions);
        ids.into_iter().filter_map(|id| sessions.remove(&id)).collect::<Vec<_>>()
    };
    for session in disposed {
        session.dispose().await;
    }
    Json(json!({"requiresTrust":true,"trusted":true,"decision":true,"decisionPath":cwd,"inherited":false}))
        .into_response()
}
