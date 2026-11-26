//! Git status and per-file diff endpoints for Pi Web.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tokio::process::Command;

use crate::web_backend::{AppState, path_access};

const TEXT_PREVIEW_MAX_BYTES: u64 = 256 * 1024;

fn absolute(path: &str) -> bool {
    Path::new(path).is_absolute()
}

async fn git(cwd: &Path, args: &[&str], max_bytes: usize) -> anyhow::Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if output.stdout.len() > max_bytes {
        anyhow::bail!("Git output exceeded the size limit");
    }
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(output.stdout)
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error":message.into()}))).into_response()
}

fn contained(root: &Path, target: &Path) -> bool {
    target.starts_with(root)
}

/// Session working directories and their Git project roots are the backend's
/// allowed roots, matching the roots exposed by the session catalog to the UI.
fn authorized(path: &Path, roots: &[PathBuf]) -> bool {
    let Ok(real) = path.canonicalize() else { return false };
    roots.iter().any(|root| contained(root, &real))
}

fn classify(index: char, worktree: char) -> (&'static str, &'static str) {
    let pair = format!("{index}{worktree}");
    if pair == "??" {
        return ("untracked", "U");
    }
    if ["DD", "AU", "UD", "UA", "DU", "AA", "UU"].contains(&pair.as_str()) || pair.contains('U') {
        return ("conflict", "C");
    }
    if pair.contains('D') {
        return ("deleted", "D");
    }
    if pair.contains('R') || pair.contains('C') {
        return ("renamed", "R");
    }
    if pair.contains('A') {
        return ("added", "A");
    }
    ("modified", "M")
}

#[derive(Debug)]
struct Entry {
    path: String,
    original: Option<String>,
    index: char,
    worktree: char,
}

fn parse_status(bytes: &[u8]) -> Vec<Entry> {
    let records = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut out = Vec::new();
    let mut i = 0;
    while i < records.len() {
        let row = records[i];
        i += 1;
        if row.len() < 4 || row[2] != b' ' {
            continue;
        }
        let (Ok(index), Ok(worktree), Ok(path)) =
            (std::str::from_utf8(&row[0..1]), std::str::from_utf8(&row[1..2]), std::str::from_utf8(&row[3..]))
        else {
            continue;
        };
        let index = index.chars().next().unwrap_or(' ');
        let worktree = worktree.chars().next().unwrap_or(' ');
        let original = if matches!(index, 'R' | 'C') || matches!(worktree, 'R' | 'C') {
            let value = records.get(i).and_then(|v| std::str::from_utf8(v).ok()).map(str::to_owned);
            i += 1;
            value
        } else {
            None
        };
        out.push(Entry { path: path.into(), original, index, worktree });
    }
    out
}

pub(super) async fn status(State(state): State<AppState>, Query(query): Query<HashMap<String, String>>) -> Response {
    let cwd = query.get("cwd").map(String::as_str).unwrap_or_default();
    if !absolute(cwd) {
        return error(StatusCode::BAD_REQUEST, "cwd must be an absolute path");
    }
    let cwd = Path::new(cwd);
    let roots = path_access::allowed_roots(&state);
    if !cwd.exists() {
        return error(StatusCode::NOT_FOUND, "Directory not found");
    }
    if !cwd.is_dir() {
        return error(StatusCode::BAD_REQUEST, "Not a directory");
    }
    if !authorized(cwd, &roots) {
        return error(StatusCode::FORBIDDEN, "Access denied");
    }
    let root = match git(cwd, &["rev-parse", "--show-toplevel"], 16 * 1024).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).trim_end().to_owned(),
        Err(_) => {
            return Json(json!({"isGitRepository":false,"repositoryRoot":null,"files":[],"additions":0,"deletions":0}))
                .into_response();
        }
    };
    let repo = PathBuf::from(&root);
    let Ok(real_repo) = repo.canonicalize() else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Git repository path could not be resolved");
    };
    let Ok(real_cwd) = cwd.canonicalize() else {
        return error(StatusCode::FORBIDDEN, "Access denied");
    };
    let bytes = match git(&repo, &["status", "--porcelain=v1", "-z", "--untracked-files=all"], 8 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let mut files = Vec::new();
    let mut untracked_lines: u64 = 0;
    for entry in parse_status(&bytes) {
        let relative = PathBuf::from(&entry.path);
        if relative.is_absolute() || relative.components().any(|part| matches!(part, std::path::Component::ParentDir)) {
            continue;
        }
        let absolute_path = real_repo.join(relative);
        let Ok(relative_cwd) = absolute_path.strip_prefix(&real_cwd) else { continue };
        if entry.index == '?' && entry.worktree == '?' {
            if let Ok(meta) = std::fs::symlink_metadata(&absolute_path) {
                if meta.is_file() && meta.len() <= TEXT_PREVIEW_MAX_BYTES {
                    if let Ok(content) = std::fs::read(&absolute_path) {
                        if !content.contains(&0) {
                            untracked_lines += content
                                .split(|b| *b == b'\n')
                                .count()
                                .saturating_sub(usize::from(content.last() == Some(&b'\n')))
                                as u64;
                        }
                    }
                }
            }
        }
        let (kind, code) = classify(entry.index, entry.worktree);
        let display = cwd.join(relative_cwd).to_string_lossy().into_owned();
        files.push(json!({"filePath":display,"status":kind,"code":code,"indexStatus":entry.index.to_string(),"worktreeStatus":entry.worktree.to_string()}));
    }
    let numstat = git(
        &real_repo,
        &["--literal-pathspecs", "diff", "--no-color", "--no-ext-diff", "--numstat", "HEAD", "--", "."],
        8 * 1024 * 1024,
    )
    .await
    .unwrap_or_default();
    let (mut additions, mut deletions) = (0u64, 0u64);
    for line in String::from_utf8_lossy(&numstat).lines() {
        let mut cols = line.split('\t');
        if let Some(n) = cols.next().and_then(|s| s.parse::<u64>().ok()) {
            additions += n;
        }
        if let Some(n) = cols.next().and_then(|s| s.parse::<u64>().ok()) {
            deletions += n;
        }
    }
    additions += untracked_lines;
    Json(
        json!({"isGitRepository":true,"repositoryRoot":root,"files":files,"additions":additions,"deletions":deletions}),
    )
    .into_response()
}

pub(super) async fn diff(State(state): State<AppState>, Query(query): Query<HashMap<String, String>>) -> Response {
    let cwd = query.get("cwd").map(String::as_str).unwrap_or_default();
    let file = query.get("path").map(String::as_str).unwrap_or_default();
    if !absolute(cwd) {
        return error(StatusCode::BAD_REQUEST, "cwd must be an absolute path");
    }
    if !absolute(file) {
        return error(StatusCode::BAD_REQUEST, "path must be an absolute path");
    }
    let cwd = Path::new(cwd);
    let file_path = Path::new(file);
    let roots = path_access::allowed_roots(&state);
    if !authorized(cwd, &roots) || !authorized_missing(file_path, &roots) {
        return error(StatusCode::FORBIDDEN, "Access denied");
    }
    let Some(repo) = find_repo(file_path).or_else(|| find_repo(cwd)) else {
        return Json(json!({"supported":false})).into_response();
    };
    let Ok(real_repo) = repo.canonicalize() else {
        return Json(json!({"supported":false})).into_response();
    };
    let resolved_file = file_path.canonicalize().unwrap_or_else(|_| file_path.to_path_buf());
    let Ok(relative) = resolved_file.strip_prefix(&real_repo) else {
        return Json(json!({"supported":false})).into_response();
    };
    let relative = relative.to_string_lossy().replace('\\', "/");
    let status = match git(&repo, &["status", "--porcelain=v1", "-z", "--untracked-files=all"], 8 * 1024 * 1024).await {
        Ok(bytes) => parse_status(&bytes).into_iter().find(|entry| entry.path == relative),
        Err(_) => None,
    };
    let Some(entry) = status else {
        return Json(json!({"supported":false})).into_response();
    };
    let (kind, _) = classify(entry.index, entry.worktree);
    let mut args = vec!["--literal-pathspecs", "diff", "--no-color", "--no-ext-diff", "--unified=3", "HEAD", "--"];
    let original = entry.original.as_deref();
    let mut owned_args = Vec::new();
    if let Some(original) = original {
        owned_args.push(original.to_owned());
    }
    owned_args.push(relative.clone());
    args.extend(owned_args.iter().map(String::as_str));
    let patch = if kind == "untracked" {
        let Ok(bytes) = std::fs::read(file_path) else {
            return Json(json!({"supported":false})).into_response();
        };
        if bytes.len() as u64 > TEXT_PREVIEW_MAX_BYTES || bytes.contains(&0) {
            return Json(json!({"supported":false})).into_response();
        }
        let content = String::from_utf8_lossy(&bytes);
        if content.is_empty() {
            String::new()
        } else {
            let lines = content.split_terminator('\n').collect::<Vec<_>>();
            let count = lines.len();
            let body = lines.iter().map(|line| format!("+{line}")).collect::<Vec<_>>().join("\n");
            format!(
                "diff --git a/{relative} b/{relative}\nnew file mode 100644\n--- /dev/null\n+++ b/{relative}\n@@ -0,0 +1,{count} @@\n{body}{}",
                if content.ends_with('\n') { "" } else { "\n\\ No newline at end of file" }
            )
        }
    } else {
        match git(&repo, &args, TEXT_PREVIEW_MAX_BYTES as usize * 4).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(_) => return Json(json!({"supported":false})).into_response(),
        }
    };
    if !patch.contains("\n@@ ") {
        return Json(json!({"supported":false})).into_response();
    }
    Json(json!({"supported":true,"status":kind,"patch":patch})).into_response()
}

fn authorized_missing(path: &Path, roots: &[PathBuf]) -> bool {
    if path.components().any(|part| matches!(part, std::path::Component::ParentDir)) {
        return false;
    }
    let mut candidate = path;
    loop {
        if let Ok(real) = candidate.canonicalize() {
            return roots.iter().any(|root| contained(root, &real));
        }
        let Some(parent) = candidate.parent() else { return false };
        if parent == candidate {
            return false;
        }
        candidate = parent;
    }
}

fn find_repo(path: &Path) -> Option<PathBuf> {
    let mut directory = if path.is_dir() { path.to_path_buf() } else { path.parent()?.to_path_buf() };
    while !directory.exists() {
        let parent = directory.parent()?.to_path_buf();
        if parent == directory {
            return None;
        }
        directory = parent;
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&directory)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    output.status.success().then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_owned()))
}
