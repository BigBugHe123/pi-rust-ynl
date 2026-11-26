//! Git worktree operations exposed by the Pi Web API.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::web_backend::{AppState, path_access};

fn response(status: StatusCode, error: impl Into<String>) -> Response {
    (status, Json(json!({"error": error.into()}))).into_response()
}

fn git(cwd: &Path, args: &[&str]) -> Result<Output, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(output)
    } else {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if message.is_empty() { format!("git exited with {}", output.status) } else { message })
    }
}

fn output_text(output: Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn is_allowed_existing(state: &AppState, candidate: &str) -> bool {
    let path = Path::new(candidate);
    let Ok(real) = path.canonicalize() else { return false };
    path_access::allowed_roots(state).iter().any(|root| real.starts_with(root))
}

fn repo_root_from_common(common: &Path) -> PathBuf {
    if common.file_name().is_some_and(|name| name == ".git") {
        return common.parent().unwrap_or(common).to_path_buf();
    }
    let parent = common.parent().unwrap_or(common);
    if let Ok(contents) = fs::read_to_string(parent.join(".git")) {
        if let Some(raw) = contents.lines().find_map(|line| line.trim().strip_prefix("gitdir:")) {
            let linked_common = canonical(&parent.join(raw.trim()));
            if linked_common == canonical(common) {
                return parent.to_path_buf();
            }
        }
    }
    common.to_path_buf()
}

#[derive(Clone)]
struct RegisteredWorktree {
    path: PathBuf,
    branch: Option<String>,
    missing: bool,
}

fn registered(cwd: &Path) -> Result<Vec<RegisteredWorktree>, String> {
    let text = output_text(git(cwd, &["worktree", "list", "--porcelain"])?);
    let mut result = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    let mut prunable = false;
    let flush = |result: &mut Vec<RegisteredWorktree>,
                 path: &mut Option<PathBuf>,
                 branch: &mut Option<String>,
                 prunable: &mut bool| {
        if let Some(path) = path.take() {
            let missing = *prunable || !path.exists();
            result.push(RegisteredWorktree { path, branch: branch.take(), missing });
        }
        *branch = None;
        *prunable = false;
    };
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("worktree ") {
            flush(&mut result, &mut path, &mut branch, &mut prunable);
            path = Some(PathBuf::from(value.trim()));
        } else if let Some(value) = line.strip_prefix("branch ") {
            branch = Some(value.trim().strip_prefix("refs/heads/").unwrap_or(value.trim()).to_string());
        } else if line.starts_with("prunable") {
            prunable = true;
        } else if line.trim().is_empty() {
            flush(&mut result, &mut path, &mut branch, &mut prunable);
        }
    }
    flush(&mut result, &mut path, &mut branch, &mut prunable);
    Ok(result)
}

fn repo_root(cwd: &Path) -> Result<PathBuf, String> {
    let common = output_text(git(cwd, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?);
    Ok(canonical(&repo_root_from_common(Path::new(&common))))
}

fn resolve_project(cwd: &Path) -> (String, String, bool) {
    let cwd_string = cwd.to_string_lossy().to_string();
    let fields = crate::web_backend::sessions::session_project_fields(&cwd_string);
    let project_root = fields.get("projectRoot").and_then(Value::as_str).unwrap_or(&cwd_string).to_string();
    let project_key = fields.get("projectKey").and_then(Value::as_str).unwrap_or(&project_root).to_string();
    let top = git(cwd, &["rev-parse", "--path-format=absolute", "--show-toplevel"])
        .ok()
        .map(|output| PathBuf::from(output_text(output)));
    let is_top_level = top.is_some_and(|top| canonical(&top) == canonical(cwd));
    (project_root, project_key, is_top_level)
}

pub(super) async fn get(
    State(state): State<AppState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(cwd) = query.get("cwd").filter(|cwd| !cwd.is_empty()) else {
        return response(StatusCode::BAD_REQUEST, "cwd is required");
    };
    if !is_allowed_existing(&state, cwd) {
        return response(StatusCode::FORBIDDEN, "Access denied");
    }
    let cwd_path = Path::new(cwd);
    let (project_root, project_key, is_top_level) = resolve_project(cwd_path);
    let mut worktrees: Vec<Value> = Vec::new();
    let mut current_path: Option<String> = None;
    let mut is_git = true;
    let lookup = if cwd_path.exists() { cwd_path.to_path_buf() } else { PathBuf::from(&project_root) };
    match registered(&lookup) {
        Ok(entries) => {
            let mut index = 0;
            for entry in entries.into_iter().filter(|entry| !entry.missing) {
                let path = entry.path.to_string_lossy().to_string();
                let real_path = canonical(&entry.path);
                if real_path == canonical(cwd_path) {
                    current_path = Some(path.clone());
                }
                let _ = path_access::allow_root(&entry.path);
                worktrees.push(json!({"path":path,"branch":entry.branch,"isMain":index == 0}));
                index += 1;
            }
        }
        Err(_) => is_git = false,
    }
    Json(json!({
        "projectRoot": project_root,
        "projectKey": project_key,
        "isGit": is_git,
        "isTopLevel": is_top_level,
        "currentWorktreePath": current_path,
        "worktrees": worktrees,
    }))
    .into_response()
}

fn sanitize_branch(branch: &str) -> String {
    let mut out = String::new();
    let mut replacing = false;
    for ch in branch.chars() {
        let invalid = matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || ch.is_whitespace();
        if invalid {
            if !replacing {
                out.push('-');
            }
            replacing = true;
        } else {
            out.push(ch);
            replacing = false;
        }
    }
    out.trim_matches('-').to_string()
}

fn git_ref_exists(repo: &Path, name: &str) -> bool {
    git(repo, &["rev-parse", "--verify", "--quiet", name]).is_ok()
}

fn branch_in_use(branch: &str, path: &Path, missing: bool) -> Response {
    let path = path.to_string_lossy();
    let message = if missing {
        format!("Branch '{branch}' belongs to a worktree whose folder is gone: {path}; git worktree prune clears it")
    } else {
        format!("Branch '{branch}' is already checked out at {path}")
    };
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error":message,
            "code":if missing {"branch_worktree_missing"} else {"branch_in_use"},
            "branch":branch,
            "worktreePath":path,
        })),
    )
        .into_response()
}

pub(super) async fn post(State(state): State<AppState>, body: Bytes) -> Response {
    let body = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    let cwd = body.get("cwd").and_then(Value::as_str).unwrap_or_default();
    let branch = body.get("branch").and_then(Value::as_str).unwrap_or_default();
    if cwd.is_empty() {
        return response(StatusCode::BAD_REQUEST, "cwd is required");
    }
    if branch.is_empty() {
        return response(StatusCode::BAD_REQUEST, "branch is required");
    }
    if !is_allowed_existing(&state, cwd) {
        return response(StatusCode::FORBIDDEN, "Access denied");
    }
    let cwd = Path::new(cwd);
    if !cwd.exists() {
        return response(StatusCode::BAD_REQUEST, format!("Directory does not exist: {}", cwd.display()));
    }
    let branch = branch.trim();
    if branch.is_empty() {
        return response(StatusCode::BAD_REQUEST, "Branch name is required");
    }
    let dir_name = sanitize_branch(branch);
    if dir_name.is_empty() {
        return response(StatusCode::BAD_REQUEST, format!("Invalid branch name: {branch}"));
    }
    let repo = match repo_root(cwd) {
        Ok(repo) => repo,
        Err(error) => return response(StatusCode::BAD_REQUEST, error),
    };
    let entries = match registered(&repo) {
        Ok(entries) => entries,
        Err(error) => return response(StatusCode::BAD_REQUEST, error),
    };
    if let Some(holder) = entries.iter().find(|entry| entry.branch.as_deref() == Some(branch)) {
        return branch_in_use(branch, &holder.path, holder.missing);
    }
    let base = PathBuf::from(format!("{}-worktrees", repo.display()));
    let worktree_path = base.join(dir_name);
    if worktree_path.exists() {
        return response(StatusCode::BAD_REQUEST, format!("Directory already exists: {}", worktree_path.display()));
    }
    if let Err(error) = fs::create_dir_all(&base) {
        return response(StatusCode::BAD_REQUEST, error.to_string());
    }

    let local_ref = format!("refs/heads/{branch}");
    let add_result = if git_ref_exists(&repo, &local_ref) {
        let path = worktree_path.to_string_lossy().to_string();
        git(&repo, &["worktree", "add", "--", &path, branch])
    } else {
        let remote_ref = format!("refs/remotes/origin/{branch}");
        let path = worktree_path.to_string_lossy().to_string();
        let mut args = vec!["worktree", "add", "-b", branch, "--", path.as_str()];
        let remote = git_ref_exists(&repo, &remote_ref);
        if remote {
            args.push(remote_ref.as_str());
        }
        git(&repo, &args)
    };
    if let Err(error) = add_result {
        return response(StatusCode::BAD_REQUEST, error);
    }
    let _ = path_access::allow_root(&worktree_path);
    Json(json!({"path":worktree_path,"branch":branch})).into_response()
}

fn needs_force(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("contains modified or untracked files")
        || lower.contains("is dirty")
        || lower.contains("containing submodules")
}

pub(super) async fn delete(State(state): State<AppState>, body: Bytes) -> Response {
    let body = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    let cwd = body.get("cwd").and_then(Value::as_str).unwrap_or_default();
    let path = body.get("path").and_then(Value::as_str).unwrap_or_default();
    if cwd.is_empty() {
        return response(StatusCode::BAD_REQUEST, "cwd is required");
    }
    if path.is_empty() {
        return response(StatusCode::BAD_REQUEST, "path is required");
    }
    if !is_allowed_existing(&state, cwd) {
        return response(StatusCode::FORBIDDEN, "Access denied");
    }
    let entries = match registered(Path::new(cwd)) {
        Ok(entries) => entries,
        Err(error) => return response(StatusCode::BAD_REQUEST, error),
    };
    let target = canonical(Path::new(path));
    let mut found: Option<(PathBuf, bool)> = None;
    for (index, entry) in entries.iter().filter(|entry| !entry.missing).enumerate() {
        if canonical(&entry.path) == target {
            if index == 0 {
                return response(StatusCode::BAD_REQUEST, "Cannot remove the main worktree");
            }
            found = Some((entry.path.clone(), false));
            break;
        }
    }
    let Some((worktree, _)) = found else {
        return response(StatusCode::BAD_REQUEST, format!("Not a worktree of this repository: {path}"));
    };
    let force = body.get("force").and_then(Value::as_bool) == Some(true);
    let target_string = worktree.to_string_lossy().to_string();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&target_string);
    match git(Path::new(cwd), &args) {
        Ok(_) => Json(json!({"success":true})).into_response(),
        Err(error) => {
            let dirty = needs_force(&error);
            let status = if dirty { StatusCode::CONFLICT } else { StatusCode::BAD_REQUEST };
            (status, Json(json!({"error":error,"dirty":dirty}))).into_response()
        }
    }
}
