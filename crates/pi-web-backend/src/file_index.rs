//! Workspace file index used by the browser's `@` file completion.
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::web_backend::AppState;

const MAX_FILES: usize = 5_000;
const GIT_HARD_CAP: usize = 200_000;
const WALK_HARD_CAP: usize = 50_000;
const MAX_WALK_DEPTH: usize = 8;
const MAX_QUERY_LENGTH: usize = 500;
const CACHE_TTL: Duration = Duration::from_secs(10);
const CACHE_MAX_ENTRIES: usize = 20;
const AT_RESULT_LIMIT: usize = 20;

#[derive(Clone)]
struct Listing {
    files: Vec<String>,
    hard_truncated: bool,
}

struct CacheEntry {
    listing: Listing,
    entries: Option<Vec<Entry>>,
    expires_at: Instant,
}

#[derive(Clone)]
struct Entry {
    path: String,
    is_dir: bool,
}

static CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();

#[derive(Deserialize)]
pub(super) struct Params {
    cwd: Option<String>,
    q: Option<String>,
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

async fn list_with_git(cwd: &Path) -> Option<Listing> {
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["-c", "core.fsmonitor=false", "ls-files", "--cached", "--others", "--exclude-standard", "-z"])
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() || output.stdout.len() > 64 * 1024 * 1024 {
        return None;
    }
    let all = output.stdout.split(|byte| *byte == 0).filter(|item| !item.is_empty()).collect::<Vec<_>>();
    let hard_truncated = all.len() > GIT_HARD_CAP;
    let files =
        all.into_iter().take(GIT_HARD_CAP).map(|item| String::from_utf8_lossy(item).replace('\\', "/")).collect();
    Some(Listing { files, hard_truncated })
}

fn hidden_outside_git(name: &str) -> bool {
    matches!(
        name,
        "node_modules"
            | ".git"
            | ".next"
            | "dist"
            | "build"
            | "__pycache__"
            | ".turbo"
            | ".cache"
            | "coverage"
            | ".pytest_cache"
            | ".mypy_cache"
            | "target"
            | "vendor"
            | ".DS_Store"
    ) || name.ends_with(".pyc")
}

async fn list_with_walk(cwd: &Path) -> Listing {
    let mut files = Vec::new();
    let mut queue = VecDeque::from([(cwd.to_path_buf(), String::new(), 0usize)]);
    while let Some((abs, rel, depth)) = queue.pop_front() {
        let Ok(mut dir) = tokio::fs::read_dir(&abs).await else { continue };
        loop {
            let Ok(Some(entry)) = dir.next_entry().await else { break };
            let name = entry.file_name().to_string_lossy().into_owned();
            if hidden_outside_git(&name) {
                continue;
            }
            let child_rel = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            let Ok(kind) = entry.file_type().await else { continue };
            if kind.is_dir() {
                if depth + 1 <= MAX_WALK_DEPTH {
                    queue.push_back((entry.path(), child_rel, depth + 1));
                }
            } else if kind.is_file() {
                if files.len() >= WALK_HARD_CAP {
                    return Listing { files, hard_truncated: true };
                }
                files.push(child_rel);
            }
        }
    }
    Listing { files, hard_truncated: false }
}

fn path_depth(path: &str) -> usize {
    path.bytes().filter(|byte| *byte == b'/').count()
}

fn build_entries(files: &[String]) -> Vec<Entry> {
    let mut dirs = std::collections::HashSet::new();
    for file in files {
        for (idx, byte) in file.bytes().enumerate() {
            if byte == b'/' {
                dirs.insert(file[..idx].to_string());
            }
        }
    }
    let mut entries = dirs.into_iter().map(|path| Entry { path, is_dir: true }).collect::<Vec<_>>();
    entries
        .extend(files.iter().filter(|file| !file.is_empty()).map(|path| Entry { path: path.clone(), is_dir: false }));
    entries.sort_by(|a, b| path_depth(&a.path).cmp(&path_depth(&b.path)).then_with(|| a.path.cmp(&b.path)));
    entries
}

fn subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|wanted| chars.find(|ch| *ch == wanted).is_some())
}

fn score(entry: &Entry, query: &str) -> i32 {
    let path = entry.path.to_lowercase();
    let mut value = if query.contains('/') {
        if path == query {
            100
        } else if path.starts_with(query) {
            80
        } else if path.contains(query) {
            50
        } else if subsequence(query, &path) {
            10
        } else {
            0
        }
    } else {
        let name = path.rsplit('/').next().unwrap_or(&path);
        if name == query {
            100
        } else if name.starts_with(query) {
            80
        } else if name.contains(query) {
            50
        } else if path.contains(query) {
            30
        } else if subsequence(query, &path) {
            10
        } else {
            0
        }
    };
    if entry.is_dir && value > 0 {
        value += 10;
    }
    value
}

fn matching_entries(entries: &[Entry], query: &str) -> Vec<Value> {
    let query = query.to_lowercase();
    let mut scored = entries
        .iter()
        .filter_map(|entry| {
            let score = score(entry, &query);
            (score > 0).then_some((entry, score))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|(a, sa), (b, sb)| {
        sb.cmp(sa).then_with(|| path_depth(&a.path).cmp(&path_depth(&b.path))).then_with(|| a.path.cmp(&b.path))
    });
    if query.is_empty() {
        return entries.iter().take(AT_RESULT_LIMIT).map(to_json).collect();
    }
    scored.into_iter().take(AT_RESULT_LIMIT).map(|(entry, _)| to_json(entry)).collect()
}

fn to_json(entry: &Entry) -> Value {
    json!({"path":entry.path,"isDir":entry.is_dir})
}

fn get_cached(cwd: &str) -> Option<(Listing, Option<Vec<Entry>>)> {
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = Instant::now();
    cache.retain(|_, entry| entry.expires_at > now);
    cache.get(cwd).map(|entry| (entry.listing.clone(), entry.entries.clone()))
}

fn put_cached(cwd: &str, listing: Listing, entries: Option<Vec<Entry>>) {
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = Instant::now();
    cache.retain(|_, entry| entry.expires_at > now);
    if cache.len() >= CACHE_MAX_ENTRIES {
        cache.clear();
    }
    cache.insert(cwd.to_owned(), CacheEntry { listing, entries, expires_at: now + CACHE_TTL });
}

pub(super) async fn get(State(state): State<AppState>, Query(params): Query<Params>) -> Response {
    let Some(cwd) = params.cwd.filter(|cwd| !cwd.is_empty()) else {
        return error(StatusCode::BAD_REQUEST, "cwd must be an absolute path");
    };
    let path = PathBuf::from(&cwd);
    if !path.is_absolute() {
        return error(StatusCode::BAD_REQUEST, "cwd must be an absolute path");
    }
    let path = match tokio::fs::canonicalize(path).await {
        Ok(path) => path,
        Err(_) => return error(StatusCode::NOT_FOUND, "Directory not found"),
    };
    let roots = crate::web_backend::path_access::allowed_roots(&state);
    if !roots.iter().any(|root| path.starts_with(root)) {
        return error(StatusCode::FORBIDDEN, "Access denied");
    }
    let metadata = match tokio::fs::metadata(&path).await {
        Ok(metadata) => metadata,
        Err(_) => return error(StatusCode::NOT_FOUND, "Directory not found"),
    };
    if !metadata.is_dir() {
        return error(StatusCode::BAD_REQUEST, "Not a directory");
    }
    let query = params.q.unwrap_or_default().chars().take(MAX_QUERY_LENGTH).collect::<String>();
    let (listing, mut entries) = if let Some(cached) = get_cached(&cwd) {
        cached
    } else {
        let listing =
            if let Some(listing) = list_with_git(&path).await { listing } else { list_with_walk(&path).await };
        put_cached(&cwd, listing.clone(), None);
        (listing, None)
    };
    if !query.is_empty() {
        if entries.is_none() {
            entries = Some(build_entries(&listing.files));
        }
        let matches = matching_entries(entries.as_ref().unwrap(), &query);
        put_cached(&cwd, listing.clone(), entries);
        return (StatusCode::OK, Json(json!({"matches":matches}))).into_response();
    }
    (StatusCode::OK, Json(json!({"files":listing.files.iter().take(MAX_FILES).collect::<Vec<_>>(),"truncated":listing.hard_truncated || listing.files.len() > MAX_FILES}))).into_response()
}
