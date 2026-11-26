//! Workspace file browser and upload endpoints.
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::body::{Body, to_bytes};
use axum::extract::{FromRequest, Multipart, Path as RoutePath, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use futures::stream;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::process::Command;
use tokio::time::interval;
use tokio_util::io::ReaderStream;

use crate::web_backend::AppState;

const TEXT_PREVIEW_MAX_BYTES: u64 = 256 * 1024;
const IMAGE_PREVIEW_MAX_BYTES: u64 = 10 * 1024 * 1024;
const DOCX_PREVIEW_MAX_BYTES: u64 = 10 * 1024 * 1024;
const MAX_UPLOAD_TOTAL_BYTES: usize = 100 * 1024 * 1024;
pub(super) const MAX_UPLOAD_REQUEST_BYTES: usize = MAX_UPLOAD_TOTAL_BYTES + 1024 * 1024;
const WATCH_HEARTBEAT: Duration = Duration::from_secs(15);
const WATCH_POLL: Duration = Duration::from_millis(500);

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error":message.into()}))).into_response()
}

fn api_path(route_path: &str) -> Result<PathBuf, Response> {
    let path = route_path.replace('\\', "/");
    if path.split('/').any(|part| part == "..") {
        return Err(error(StatusCode::FORBIDDEN, "Access denied"));
    }
    #[cfg(windows)]
    let native = path.replace('/', "\\");
    #[cfg(not(windows))]
    let native = if path.starts_with('/') { path } else { format!("/{path}") };
    Ok(PathBuf::from(native))
}

fn within(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

fn authorized_existing(path: &Path, roots: &[PathBuf]) -> bool {
    path.canonicalize().is_ok_and(|real| within(&real, roots))
}

fn authorized_future(path: &Path, roots: &[PathBuf]) -> bool {
    let mut ancestor = path;
    while !ancestor.exists() {
        let Some(parent) = ancestor.parent() else { return false };
        if parent == ancestor {
            return false;
        }
        ancestor = parent;
    }
    authorized_existing(ancestor, roots)
}

fn ext(path: &Path) -> String {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    name.rsplit_once('.')
        .filter(|(base, _)| !base.is_empty())
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default()
}

fn mime(path: &Path) -> &'static str {
    match ext(path).as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "avif" => "image/avif",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "weba" => "audio/webm",
        "mp4" => "video/mp4",
        "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "ogv" => "video/ogg",
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        _ => "application/octet-stream",
    }
}

fn preview_kind(path: &Path) -> Option<&'static str> {
    match ext(path).as_str() {
        "pdf" => Some("pdf"),
        "docx" => Some("docx"),
        _ => None,
    }
}

fn language(path: &Path) -> &'static str {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_ascii_lowercase();
    if name == "dockerfile" || name.starts_with("dockerfile.") {
        return "dockerfile";
    }
    if name == ".env" || name.starts_with(".env.") {
        return "bash";
    }
    if name == "makefile" || name == "gnumakefile" {
        return "makefile";
    }
    match ext(path).as_str() {
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "jsx",
        "py" => "python",
        "rb" => "ruby",
        "php" => "php",
        "lua" => "lua",
        "pl" => "perl",
        "r" => "r",
        "go" => "go",
        "rs" => "rust",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "scala" => "scala",
        "groovy" | "gradle" => "groovy",
        "dart" => "dart",
        "zig" => "zig",
        "c" | "h" => "c",
        "cpp" | "hpp" => "cpp",
        "cs" => "csharp",
        "mm" => "objectivec",
        "ex" | "exs" => "elixir",
        "hs" => "haskell",
        "ml" => "ocaml",
        "clj" => "clojure",
        "nix" => "nix",
        "html" | "htm" => "html",
        "vue" | "svelte" | "astro" => "markup",
        "css" => "css",
        "scss" => "scss",
        "less" => "less",
        "json" | "jsonl" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "ini" | "cfg" => "ini",
        "properties" => "properties",
        "xml" => "xml",
        "md" | "mdx" => "markdown",
        "tex" => "latex",
        "sh" | "bash" | "zsh" | "fish" | "env" | "gitignore" => "bash",
        "ps1" | "psm1" => "powershell",
        "bat" => "batch",
        "cmd" => "batch",
        "sql" => "sql",
        "graphql" | "gql" => "graphql",
        "proto" => "protobuf",
        "tf" | "hcl" => "hcl",
        "diff" | "patch" => "diff",
        "pdf" => "pdf",
        "docx" => "word",
        _ => "text",
    }
}

fn iso_mtime(metadata: &std::fs::Metadata) -> String {
    // Match JS Date.toISOString() with millisecond precision.
    let millis = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    iso_millis(millis)
}

fn iso_millis(millis: i64) -> String {
    let secs = millis.div_euclid(1000);
    let ms = millis.rem_euclid(1000);
    // Howard Hinnant civil-from-days conversion.
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    year += i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{ms:03}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

async fn session_reference(state: &AppState, session_id: Option<&str>, path: &str) -> bool {
    let Some(id) = session_id else { return false };
    let Ok(manager) = crate::web_backend::sessions::open_session_manager(state, id) else { return false };
    let entries = manager.entries().iter().map(|entry| Value::Object(entry.raw().clone())).collect::<Vec<_>>();
    crate::web_backend::session_file_reference::is_file_path_referenced(&entries, path)
}

async fn authorize(
    state: &AppState,
    path: &Path,
    kind: &str,
    session_id: Option<&str>,
) -> Result<(Vec<PathBuf>, bool), Response> {
    let roots = crate::web_backend::path_access::allowed_roots(state);
    let root_allowed = authorized_future(path, &roots);
    let referenced =
        !root_allowed && kind != "list" && session_reference(state, session_id, &path.to_string_lossy()).await;
    if !root_allowed && !referenced {
        return Err(error(StatusCode::FORBIDDEN, "Access denied"));
    }
    if root_allowed && path.exists() && !authorized_existing(path, &roots) {
        return Err(error(StatusCode::FORBIDDEN, "Access denied"));
    }
    Ok((roots, referenced))
}

fn response_headers(
    path: &Path,
    content_type: &str,
    size: u64,
    start_end: Option<(u64, u64)>,
    download: bool,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, no-transform"));
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    let disposition = if download { "attachment" } else { "inline" };
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("download");
    let fallback = name
        .chars()
        .map(|c| if c.is_ascii() && !c.is_control() && !matches!(c, '"' | '\\' | ';') { c } else { '_' })
        .collect::<String>();
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "{disposition}; filename=\"{fallback}\"; filename*=UTF-8''{}",
            percent_encode(name.as_bytes())
        ))
        .unwrap_or_else(|_| HeaderValue::from_static("inline")),
    );
    if content_type == "image/svg+xml" {
        headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; img-src data:; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'self'"));
        headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    }
    if let Some((start, end)) = start_end {
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_str(&(end - start + 1).to_string()).unwrap());
        headers.insert(header::CONTENT_RANGE, HeaderValue::from_str(&format!("bytes {start}-{end}/{size}")).unwrap());
    } else {
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_str(&size.to_string()).unwrap());
    }
    headers
}

fn percent_encode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(b) {
                char::from(*b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

async fn file_response(
    path: &Path,
    metadata: &std::fs::Metadata,
    content_type: &str,
    range: Option<&str>,
    download: bool,
) -> Response {
    let size = metadata.len();
    let range = match range {
        None => None,
        Some(raw) => {
            let Some(spec) = raw.strip_prefix("bytes=") else { return range_error(path, content_type, size, download) };
            if spec.contains(',') {
                return range_error(path, content_type, size, download);
            }
            let Some((a, b)) = spec.split_once('-') else { return range_error(path, content_type, size, download) };
            if a.is_empty() && b.is_empty() {
                return range_error(path, content_type, size, download);
            }
            let (start, end) = if a.is_empty() {
                let Ok(suffix) = b.parse::<u64>() else { return range_error(path, content_type, size, download) };
                (size.saturating_sub(suffix), size.saturating_sub(1))
            } else {
                let Ok(start) = a.parse::<u64>() else { return range_error(path, content_type, size, download) };
                let end = if b.is_empty() {
                    size.saturating_sub(1)
                } else {
                    match b.parse::<u64>() {
                        Ok(value) => value,
                        Err(_) => return range_error(path, content_type, size, download),
                    }
                };
                (start, end.min(size.saturating_sub(1)))
            };
            if size == 0 || start >= size || end < start {
                return range_error(path, content_type, size, download);
            }
            Some((start, end))
        }
    };
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(_) => return error(StatusCode::NOT_FOUND, "Not found"),
    };
    let mut file = file;
    if let Some((start, end)) = range {
        if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to seek file");
        }
        let stream = ReaderStream::new(file.take(end - start + 1));
        let mut response = Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .body(Body::from_stream(stream))
            .unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to stream file"));
        *response.headers_mut() = response_headers(path, content_type, size, Some((start, end)), download);
        return response;
    }
    let stream = ReaderStream::new(file);
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to stream file"));
    *response.headers_mut() = response_headers(path, content_type, size, None, download);
    response
}

fn range_error(path: &Path, content_type: &str, size: u64, download: bool) -> Response {
    let mut headers = response_headers(path, content_type, size, None, download);
    headers.remove(header::CONTENT_LENGTH);
    headers.insert(header::CONTENT_RANGE, HeaderValue::from_str(&format!("bytes */{size}")).unwrap());
    let mut response = Response::builder().status(StatusCode::RANGE_NOT_SATISFIABLE).body(Body::empty()).unwrap();
    *response.headers_mut() = headers;
    response
}

fn hidden_fallback(name: &str) -> Option<&'static str> {
    if name == ".git" || name == ".DS_Store" {
        return Some("always");
    }
    matches!(
        name,
        "node_modules"
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
    )
    .then_some("excluded")
    .or_else(|| name.ends_with(".pyc").then_some("excluded"))
}

async fn ignored_names(directory: &Path, names: &[String]) -> Option<std::collections::HashSet<String>> {
    let mut input = String::from(".\0");
    for name in names {
        input.push_str("./");
        input.push_str(name);
        input.push('\0');
    }
    let output = tokio::time::timeout(Duration::from_secs(5), async {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(["-c", "core.fsmonitor=false", "check-ignore", "-z", "--stdin"])
            .env("LC_ALL", "C")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        use tokio::io::AsyncWriteExt;
        child.stdin.take()?.write_all(input.as_bytes()).await.ok()?;
        child.wait_with_output().await.ok()
    })
    .await
    .ok()
    .flatten()?;
    if !output.status.success() && output.status.code() != Some(1) {
        return None;
    }
    let mut matched = std::collections::HashSet::new();
    for item in output.stdout.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        if let Ok(value) = std::str::from_utf8(item) {
            if let Some(name) = value.strip_prefix("./") {
                matched.insert(name.to_string());
            }
        }
    }
    Some(matched)
}

async fn list_directory(path: &Path, roots: &[PathBuf], show_hidden: bool) -> Response {
    let mut entries = Vec::new();
    let mut dir = match tokio::fs::read_dir(path).await {
        Ok(v) => v,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let mut all = Vec::new();
    loop {
        match dir.next_entry().await {
            Ok(Some(entry)) => all.push(entry),
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    let names = all.iter().map(|e| e.file_name().to_string_lossy().to_string()).collect::<Vec<_>>();
    let ignored = ignored_names(path, &names).await;
    for entry in all {
        let name = entry.file_name().to_string_lossy().to_string();
        let fallback = hidden_fallback(&name);
        let reason = if matches!(fallback, Some("always")) {
            fallback
        } else if let Some(ref ignored) = ignored {
            ignored.contains(&name).then_some("ignored")
        } else {
            fallback
        };
        if matches!(reason, Some("always")) || (reason.is_some() && !show_hidden) {
            continue;
        }
        let file_type = match entry.file_type().await {
            Ok(t) => t,
            Err(_) => continue,
        };
        let full = entry.path();
        let meta = tokio::fs::metadata(&full).await.ok();
        let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
        if !is_dir && !meta.as_ref().is_some_and(|m| m.is_file()) {
            continue;
        }
        let mut value = json!({"name":name,"isDir":is_dir,"size":0,"modified":""});
        if let Some(reason) = reason {
            value["hidden"] = json!(reason);
        }
        if file_type.is_symlink() && is_dir {
            if let Ok(target) = full.canonicalize() {
                if !within(&target, roots) {
                    value["outsideLinkTarget"] = json!(target.to_string_lossy());
                    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
                    if home.and_then(|h| h.canonicalize().ok()).is_some_and(|h| target.starts_with(h)) {
                        value["outsideLinkEncloses"] = json!(true);
                    }
                }
            }
        }
        entries.push(value);
    }
    entries.sort_by(|a, b| {
        let ad = a["isDir"].as_bool().unwrap_or(false);
        let bd = b["isDir"].as_bool().unwrap_or(false);
        bd.cmp(&ad).then_with(|| a["name"].as_str().unwrap_or_default().cmp(b["name"].as_str().unwrap_or_default()))
    });
    Json(json!({"entries":entries,"path":path.to_string_lossy()})).into_response()
}

pub(super) async fn get(
    State(state): State<AppState>,
    RoutePath(route_path): RoutePath<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let path = match api_path(&route_path) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let kind = query.get("type").map(String::as_str).unwrap_or("list");
    if !matches!(kind, "list" | "read" | "download" | "meta" | "preview" | "watch") {
        return error(StatusCode::BAD_REQUEST, "Invalid file request type");
    }
    let (roots, referenced) = match authorize(&state, &path, kind, query.get("sessionId").map(String::as_str)).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let metadata = tokio::fs::metadata(&path).await.ok();
    if metadata.is_none() && kind != "watch" {
        return error(StatusCode::NOT_FOUND, "Not found");
    }
    if let Some(meta) = metadata.as_ref() {
        if !meta.is_file() && !meta.is_dir() {
            return error(StatusCode::BAD_REQUEST, "Not a file or directory");
        }
        if !referenced && !authorized_existing(&path, &roots) {
            return error(StatusCode::FORBIDDEN, "Access denied");
        }
    }
    match kind {
        "list" => {
            if metadata.as_ref().is_some_and(|m| m.is_dir()) {
                list_directory(&path, &roots, query.get("hidden").is_some_and(|v| v == "1")).await
            } else {
                error(StatusCode::BAD_REQUEST, "Not a directory")
            }
        }
        "read" | "download" => {
            let Some(meta) = metadata.as_ref().filter(|m| m.is_file()) else {
                return error(StatusCode::BAD_REQUEST, "Not a file");
            };
            let content = mime(&path);
            if kind == "read" && content.starts_with("image/") && meta.len() > IMAGE_PREVIEW_MAX_BYTES {
                return error(StatusCode::PAYLOAD_TOO_LARGE, "Image too large (>10MB)");
            }
            if kind == "read" && matches!(content, "application/octet-stream") {
                let offset = match query.get("offset") {
                    Some(v) => match v.parse::<u64>() {
                        Ok(n) => n,
                        Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid text preview offset"),
                    },
                    None => 0,
                };
                if offset > meta.len() {
                    return error(StatusCode::BAD_REQUEST, "Invalid text preview offset");
                }
                return text_preview(&path, meta, offset).await;
            }
            let download = kind == "download";
            let content = if download && content == "application/octet-stream" {
                "application/octet-stream"
            } else if content == "application/octet-stream" {
                "text/plain"
            } else {
                content
            };
            file_response(&path, meta, content, headers.get(header::RANGE).and_then(|v| v.to_str().ok()), download)
                .await
        }
        "meta" => {
            let Some(meta) = metadata.as_ref().filter(|m| m.is_file()) else {
                return error(StatusCode::BAD_REQUEST, "Not a file");
            };
            let content = mime(&path);
            Json(json!({"size":meta.len(),"mtime":iso_mtime(meta),"language":language(&path),"mime":if content=="application/octet-stream"{"text/plain"}else{content},"previewKind":preview_kind(&path)})).into_response()
        }
        "preview" => {
            let Some(meta) = metadata.as_ref().filter(|m| m.is_file()) else {
                return error(StatusCode::BAD_REQUEST, "Not a file");
            };
            if ext(&path) != "docx" {
                return error(StatusCode::BAD_REQUEST, "Preview not available for this file type");
            }
            if meta.len() > DOCX_PREVIEW_MAX_BYTES {
                return error(StatusCode::PAYLOAD_TOO_LARGE, "DOCX too large for preview (>10MB)");
            }
            docx_preview(&path).await
        }
        "watch" => watch(path, metadata).into_response(),
        _ => error(StatusCode::BAD_REQUEST, "Invalid file request type"),
    }
}

async fn text_preview(path: &Path, metadata: &std::fs::Metadata, offset: u64) -> Response {
    let size = metadata.len();
    let length = (TEXT_PREVIEW_MAX_BYTES + 1).min(size.saturating_sub(offset)) as usize;
    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return error(StatusCode::NOT_FOUND, "Not found"),
    };
    let mut header_bytes = [0u8; 2];
    let _ = file.read_exact(&mut header_bytes).await;
    if file.seek(std::io::SeekFrom::Start(offset)).await.is_err() {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to read file");
    }
    let mut bytes = vec![0; length];
    let read = match file.read(&mut bytes).await {
        Ok(n) => n,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    bytes.truncate(read);
    let utf16 = match header_bytes {
        [0xff, 0xfe] => Some(true),
        [0xfe, 0xff] => Some(false),
        _ => None,
    };
    if offset == 0 && utf16.is_none() && bytes.iter().take(8000).any(|b| *b == 0) {
        return Json(json!({"content":"","nextOffset":0,"truncated":false,"binary":true,"language":language(path),"size":size,"mtime":iso_mtime(metadata)})).into_response();
    }
    let mut end = read.min(TEXT_PREVIEW_MAX_BYTES as usize);
    if offset + (end as u64) < size {
        if let Some(_little) = utf16 {
            end -= end % 2;
            if end >= 2 {
                let word = if _little {
                    u16::from_le_bytes([bytes[end - 2], bytes[end - 1]])
                } else {
                    u16::from_be_bytes([bytes[end - 2], bytes[end - 1]])
                };
                if (0xd800..=0xdbff).contains(&word) {
                    end = end.saturating_sub(2);
                }
            }
        } else {
            while end > 0 && end < bytes.len() && (bytes[end] & 0xc0) == 0x80 {
                end -= 1;
            }
        }
    }
    let content = if let Some(little) = utf16 {
        let start = if offset == 0 { 2.min(end) } else { 0 };
        String::from_utf16_lossy(
            &bytes[start..end]
                .chunks_exact(2)
                .map(|c| if little { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
                .collect::<Vec<_>>(),
        )
    } else {
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    };
    let next = offset + end as u64;
    Json(json!({"content":content,"nextOffset":next,"truncated":next<size,"binary":false,"language":language(path),"size":size,"mtime":iso_mtime(metadata)})).into_response()
}

fn watch(
    path: PathBuf,
    initial: Option<std::fs::Metadata>,
) -> Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let file_path = std::sync::Arc::<str>::from(path.to_string_lossy().to_string());
    let events = stream::unfold(
        (path, initial.map(|meta| fingerprint(&meta)), false, interval(WATCH_POLL), tokio::time::Instant::now()),
        move |(path, mut last, mut connected, mut tick, mut heartbeat)| {
            let file_path = std::sync::Arc::clone(&file_path);
            async move {
                loop {
                    tick.tick().await;
                    let metadata = tokio::fs::metadata(&path).await.ok();
                    let current = metadata.as_ref().map(fingerprint);
                    let (event, data) = if !connected {
                        connected = true;
                        ("connected", json!({"filePath":file_path.as_ref()}))
                    } else if current != last {
                        last = current;
                        if let Some(meta) = metadata.as_ref() {
                            ("change", json!({"mtime":iso_mtime(meta),"size":meta.len()}))
                        } else {
                            ("change", json!({"mtime":iso_now(),"size":0}))
                        }
                    } else if heartbeat.elapsed() >= WATCH_HEARTBEAT {
                        heartbeat = tokio::time::Instant::now();
                        ("ping", json!({}))
                    } else {
                        continue;
                    };
                    let event = Event::default()
                        .event(event)
                        .json_data(data)
                        .unwrap_or_else(|_| Event::default().event("error"));
                    return Some((Ok(event), (path, last, connected, tick, heartbeat)));
                }
            }
        },
    );
    Sse::new(events).keep_alive(KeepAlive::new().interval(WATCH_HEARTBEAT).text("ping"))
}

fn fingerprint(meta: &std::fs::Metadata) -> (u64, u128) {
    (
        meta.len(),
        meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0),
    )
}
fn iso_now() -> String {
    iso_millis(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64)
}

fn xml_unescape(text: &str) -> String {
    let mut result = text
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&");
    let mut cursor = 0;
    while let Some(relative) = result[cursor..].find("&#") {
        let start = cursor + relative;
        let Some(end_rel) = result[start..].find(';') else { break };
        let end = start + end_rel;
        let entity = &result[start + 2..end];
        let code = if let Some(hex) = entity.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()
        } else {
            entity.parse::<u32>().ok()
        };
        if let Some(ch) = code.and_then(char::from_u32) {
            result.replace_range(start..=end, &ch.to_string());
            cursor = start + ch.len_utf8();
        } else {
            cursor = end + 1;
        }
    }
    result
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

fn has_tag(fragment: &str, tag: &str) -> bool {
    fragment.contains(&format!("<{tag}/"))
        || fragment.contains(&format!("<{tag}>"))
        || fragment.contains(&format!("<{tag} "))
}

fn attr(fragment: &str, key: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let needle = format!("{key}={quote}");
        if let Some(start) = fragment.find(&needle) {
            let rest = &fragment[start + needle.len()..];
            if let Some(end) = rest.find(quote) {
                return Some(xml_unescape(&rest[..end]));
            }
        }
    }
    None
}

fn tag_blocks<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let mut found = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = xml[cursor..].find(&open) {
        let start = cursor + relative;
        let after = start + open.len();
        if xml.as_bytes().get(after).is_some_and(|byte| !byte.is_ascii_whitespace() && *byte != b'>' && *byte != b'/') {
            cursor = after;
            continue;
        }
        let Some(start_tag_end_relative) = xml[after..].find('>') else { break };
        let start_tag_end = after + start_tag_end_relative;
        if xml[start..start_tag_end].trim_end().ends_with('/') {
            found.push(&xml[start..=start_tag_end]);
            cursor = start_tag_end + 1;
            continue;
        }
        let Some(close_relative) = xml[after..].find(&close) else { break };
        let end = after + close_relative + close.len();
        found.push(&xml[start..end]);
        cursor = end;
    }
    found
}

fn text_elements(xml: &str) -> String {
    let mut result = String::new();
    let mut cursor = 0;
    while let Some(relative) = xml[cursor..].find("<w:t") {
        let start = cursor + relative;
        let after = start + 4;
        if xml.as_bytes().get(after).is_some_and(|byte| !byte.is_ascii_whitespace() && *byte != b'>' && *byte != b'/') {
            cursor = after;
            continue;
        }
        let Some(tag_end_rel) = xml[after..].find('>') else { break };
        let content_start = after + tag_end_rel + 1;
        let Some(close_rel) = xml[content_start..].find("</w:t>") else { break };
        result.push_str(&html_escape(&xml_unescape(&xml[content_start..content_start + close_rel])));
        cursor = content_start + close_rel + 6;
    }
    if xml.contains("<w:tab") {
        result.push(' ');
    }
    if xml.contains("<w:br") {
        result.push_str("<br>");
    }
    result
}

fn relationship_targets(xml: &str) -> HashMap<String, String> {
    let mut result = HashMap::new();
    for rel in tag_blocks(xml, "Relationship") {
        if let (Some(id), Some(target)) = (attr(rel, "Id"), attr(rel, "Target")) {
            result.insert(id, target);
        }
    }
    result
}

fn image_uri(run: &str, rels: &HashMap<String, String>, media: &HashMap<String, Vec<u8>>) -> Option<String> {
    let rid = attr(run, "r:embed")?;
    let target = rels.get(&rid)?;
    let path =
        if target.starts_with('/') { target.trim_start_matches('/').to_string() } else { format!("word/{target}") };
    let normalized = path.split('/').fold(Vec::<&str>::new(), |mut parts, part| {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
        parts
    });
    let data = media.get(&normalized.join("/"))?;
    let mime = match Path::new(target).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "emf" => "image/emf",
        "wmf" => "image/wmf",
        _ => "application/octet-stream",
    };
    Some(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(data)))
}

fn render_run(run: &str, rels: &HashMap<String, String>, media: &HashMap<String, Vec<u8>>) -> String {
    let mut output = text_elements(run);
    if let Some(uri) = image_uri(run, rels, media) {
        output.push_str(&format!("<img src=\"{uri}\" alt=\"\" />"));
    }
    if has_tag(run, "w:b") {
        output = format!("<strong>{output}</strong>");
    }
    if has_tag(run, "w:i") {
        output = format!("<em>{output}</em>");
    }
    if has_tag(run, "w:u") {
        output = format!("<u>{output}</u>");
    }
    if has_tag(run, "w:strike") || has_tag(run, "w:dstrike") {
        output = format!("<s>{output}</s>");
    }
    if let Some(align) = tag_blocks(run, "w:vertAlign").first().and_then(|tag| attr(tag, "w:val")) {
        output = match align.as_str() {
            "superscript" => format!("<sup>{output}</sup>"),
            "subscript" => format!("<sub>{output}</sub>"),
            _ => output,
        };
    }
    output
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ListStyle {
    num_id: String,
    level: u8,
    ordered: bool,
    marker: Option<&'static str>,
}

fn list_styles(numbering: &str) -> HashMap<(String, u8), (bool, Option<&'static str>)> {
    let mut abstract_styles = HashMap::<String, HashMap<u8, (bool, Option<&'static str>)>>::new();
    for abstract_num in tag_blocks(numbering, "w:abstractNum") {
        let Some(abstract_id) = attr(abstract_num, "w:abstractNumId") else { continue };
        let mut levels = HashMap::new();
        for level in tag_blocks(abstract_num, "w:lvl") {
            let level_id = attr(level, "w:ilvl").and_then(|value| value.parse::<u8>().ok()).unwrap_or(0);
            let Some(format) = tag_blocks(level, "w:numFmt").first().and_then(|tag| attr(tag, "w:val")) else {
                continue;
            };
            let style = match format.as_str() {
                "bullet" => (false, None),
                "decimal" | "decimalZero" | "decimalFullWidth" | "decimalEnclosedCircle" => (true, None),
                "lowerLetter" => (true, Some("a")),
                "upperLetter" => (true, Some("A")),
                "lowerRoman" => (true, Some("i")),
                "upperRoman" => (true, Some("I")),
                _ => continue,
            };
            levels.insert(level_id, style);
        }
        abstract_styles.insert(abstract_id, levels);
    }
    let mut result = HashMap::new();
    for num in tag_blocks(numbering, "w:num") {
        let (Some(num_id), Some(abstract_id)) =
            (attr(num, "w:numId"), tag_blocks(num, "w:abstractNumId").first().and_then(|tag| attr(tag, "w:val")))
        else {
            continue;
        };
        if let Some(levels) = abstract_styles.get(&abstract_id) {
            for (level, style) in levels {
                result.insert((num_id.clone(), *level), *style);
            }
        }
    }
    result
}

fn paragraph_list_style(
    properties: &str,
    number_styles: &HashMap<(String, u8), (bool, Option<&'static str>)>,
) -> Option<ListStyle> {
    let num_pr = tag_blocks(properties, "w:numPr").first().copied()?;
    let num_id = tag_blocks(num_pr, "w:numId").first().and_then(|tag| attr(tag, "w:val"))?;
    let level = tag_blocks(num_pr, "w:ilvl")
        .first()
        .and_then(|tag| attr(tag, "w:val"))
        .and_then(|value| value.parse::<u8>().ok())
        .unwrap_or(0);
    let (ordered, marker) = number_styles.get(&(num_id.clone(), level)).copied().unwrap_or((false, None));
    Some(ListStyle { num_id, level, ordered, marker })
}

fn next_element<'a>(xml: &'a str, cursor: usize, tag: &str) -> Option<(usize, &'a str)> {
    let open = format!("<{tag}");
    let relative = xml.get(cursor..)?.find(&open)?;
    let start = cursor + relative;
    let after = start + open.len();
    if xml.as_bytes().get(after).is_some_and(|byte| !byte.is_ascii_whitespace() && *byte != b'>' && *byte != b'/') {
        return next_element(xml, after, tag);
    }
    let block = tag_blocks(&xml[start..], tag).first().copied()?;
    Some((start, block))
}

fn safe_hyperlink(link: &str, rels: &HashMap<String, String>) -> Option<String> {
    if let Some(anchor) = attr(link, "w:anchor") {
        if !anchor.is_empty() && anchor.chars().all(|ch| ch.is_ascii_alphanumeric() || "-_:.".contains(ch)) {
            return Some(format!("#{anchor}"));
        }
    }
    let target = rels.get(&attr(link, "r:id")?)?;
    let lower = target.trim().to_ascii_lowercase();
    ["https://", "http://", "mailto:", "tel:"].iter().any(|scheme| lower.starts_with(scheme)).then(|| target.clone())
}

fn paragraph_content(paragraph: &str, rels: &HashMap<String, String>, media: &HashMap<String, Vec<u8>>) -> String {
    let mut cursor = 0;
    let mut output = String::new();
    while cursor < paragraph.len() {
        let run = next_element(paragraph, cursor, "w:r");
        let link = next_element(paragraph, cursor, "w:hyperlink");
        match (run, link) {
            (None, None) => break,
            (Some((start, block)), None) => {
                output.push_str(&render_run(block, rels, media));
                cursor = start + block.len();
            }
            (None, Some((start, block))) => {
                let label = paragraph_content(block, rels, media);
                if let Some(href) = safe_hyperlink(block, rels) {
                    output.push_str(&format!(
                        "<a href=\"{}\" rel=\"noopener noreferrer\">{label}</a>",
                        html_escape(&href)
                    ));
                } else {
                    output.push_str(&label);
                }
                cursor = start + block.len();
            }
            (Some((run_start, run_block)), Some((link_start, _link_block))) if run_start < link_start => {
                output.push_str(&render_run(run_block, rels, media));
                cursor = run_start + run_block.len();
            }
            (_, Some((start, block))) => {
                let label = paragraph_content(block, rels, media);
                if let Some(href) = safe_hyperlink(block, rels) {
                    output.push_str(&format!(
                        "<a href=\"{}\" rel=\"noopener noreferrer\">{label}</a>",
                        html_escape(&href)
                    ));
                } else {
                    output.push_str(&label);
                }
                cursor = start + block.len();
            }
        }
    }
    output
}

fn paragraph_html(
    paragraph: &str,
    rels: &HashMap<String, String>,
    media: &HashMap<String, Vec<u8>>,
    number_styles: &HashMap<(String, u8), (bool, Option<&'static str>)>,
) -> (String, Option<ListStyle>) {
    let properties = tag_blocks(paragraph, "w:pPr").first().copied().unwrap_or_default();
    let style = tag_blocks(properties, "w:pStyle").first().and_then(|tag| attr(tag, "w:val")).unwrap_or_default();
    let mut text = paragraph_content(paragraph, rels, media);
    if text.is_empty() {
        text = text_elements(paragraph);
    }
    if let Some(list) = paragraph_list_style(properties, number_styles) {
        return (format!("<li>{text}</li>"), Some(list));
    }
    let compact_style = style.chars().filter(|ch| ch.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
    if let Some(level) =
        compact_style.strip_prefix("heading").and_then(|n| n.parse::<u8>().ok()).filter(|n| (1..=6).contains(n))
    {
        return (format!("<h{level}>{text}</h{level}>"), None);
    }
    if compact_style == "title" {
        return (format!("<h1>{text}</h1>"), None);
    }
    if compact_style == "subtitle" {
        return (format!("<p class=\"subtitle\">{text}</p>"), None);
    }
    let alignment_value = tag_blocks(properties, "w:jc").first().and_then(|tag| attr(tag, "w:val"));
    let style = match alignment_value.as_deref() {
        Some("center") => " style=\"text-align:center\"",
        Some("right" | "end") => " style=\"text-align:right\"",
        Some("both" | "distribute") => " style=\"text-align:justify\"",
        _ => "",
    };
    (format!("<p{style}>{text}</p>"), None)
}

fn table_html(
    table: &str,
    rels: &HashMap<String, String>,
    media: &HashMap<String, Vec<u8>>,
    number_styles: &HashMap<(String, u8), (bool, Option<&'static str>)>,
) -> String {
    let mut html = String::from("<table>");
    for row in tag_blocks(table, "w:tr") {
        html.push_str("<tr>");
        let is_header = tag_blocks(row, "w:trPr").first().is_some_and(|props| has_tag(props, "w:tblHeader"));
        let cell_tag = if is_header { "th" } else { "td" };
        for cell in tag_blocks(row, "w:tc") {
            html.push_str(&format!("<{cell_tag}>"));
            for paragraph in tag_blocks(cell, "w:p") {
                html.push_str(&paragraph_html(paragraph, rels, media, number_styles).0);
            }
            html.push_str(&format!("</{cell_tag}>"));
        }
        html.push_str("</tr>");
    }
    html.push_str("</table>");
    html
}

fn render_document(
    xml: &str,
    rels: &HashMap<String, String>,
    media: &HashMap<String, Vec<u8>>,
    number_styles: &HashMap<(String, u8), (bool, Option<&'static str>)>,
) -> String {
    let body = tag_blocks(xml, "w:body").first().copied().unwrap_or(xml);
    let mut cursor = 0;
    let mut html = String::new();
    let mut open_list: Option<ListStyle> = None;
    while cursor < body.len() {
        let table = body[cursor..].find("<w:tbl");
        let paragraph = body[cursor..].find("<w:p").filter(|relative| {
            let pos = cursor + relative;
            let after = pos + 4;
            body.as_bytes().get(after).is_some_and(|byte| !byte.is_ascii_whitespace() && *byte != b'>' && *byte != b'/')
        });
        match (table, paragraph) {
            (None, None) => break,
            (Some(t), None) => {
                close_list(&mut html, &mut open_list);
                let start = cursor + t;
                let blocks = tag_blocks(&body[start..], "w:tbl");
                if let Some(block) = blocks.first() {
                    html.push_str(&table_html(block, rels, media, number_styles));
                    cursor = start + block.len();
                } else {
                    break;
                }
            }
            (None, Some(p)) => {
                let start = cursor + p;
                let blocks = tag_blocks(&body[start..], "w:p");
                if let Some(block) = blocks.first() {
                    let (markup, list) = paragraph_html(block, rels, media, number_styles);
                    update_list(&mut html, &mut open_list, list);
                    html.push_str(&markup);
                    cursor = start + block.len();
                } else {
                    break;
                }
            }
            (Some(t), Some(p)) if t < p => {
                close_list(&mut html, &mut open_list);
                let start = cursor + t;
                let block = tag_blocks(&body[start..], "w:tbl").first().copied();
                if let Some(block) = block {
                    html.push_str(&table_html(block, rels, media, number_styles));
                    cursor = start + block.len();
                } else {
                    break;
                }
            }
            (_, Some(p)) => {
                let start = cursor + p;
                let block = tag_blocks(&body[start..], "w:p").first().copied();
                if let Some(block) = block {
                    let (markup, list) = paragraph_html(block, rels, media, number_styles);
                    update_list(&mut html, &mut open_list, list);
                    html.push_str(&markup);
                    cursor = start + block.len();
                } else {
                    break;
                }
            }
        }
    }
    close_list(&mut html, &mut open_list);
    html
}

fn close_list(html: &mut String, open_list: &mut Option<ListStyle>) {
    if let Some(list) = open_list.take() {
        html.push_str(if list.ordered { "</ol>" } else { "</ul>" });
    }
}

fn update_list(html: &mut String, open_list: &mut Option<ListStyle>, next: Option<ListStyle>) {
    if *open_list == next {
        return;
    }
    if let Some(previous) = open_list.take() {
        html.push_str(if previous.ordered { "</ol>" } else { "</ul>" });
    }
    if let Some(list) = next {
        let tag = if list.ordered { "ol" } else { "ul" };
        let marker = list.marker.map(|value| format!(" type=\"{value}\"")).unwrap_or_default();
        let indent = usize::from(list.level.min(8)) * 18;
        html.push_str(&format!("<{tag}{marker} style=\"margin-left:{indent}px\">"));
        *open_list = Some(list);
    }
}

fn docx_html(bytes: &[u8], file_name: &str) -> Result<String, String> {
    if bytes.len() < 22 {
        return Err("Invalid DOCX archive".into());
    }
    // The shared ZIP reader allocates each entry based on its declared expanded size.
    // Preflight the central directory before invoking it to cap decompression.
    let search = bytes.len().saturating_sub(22 + 65_535);
    let eocd = (search..=bytes.len().saturating_sub(22))
        .rev()
        .find(|at| bytes.get(*at..*at + 4) == Some(b"PK\x05\x06"))
        .ok_or("Invalid DOCX archive")?;
    let mut cursor =
        u32::from_le_bytes(bytes.get(eocd + 16..eocd + 20).ok_or("Invalid DOCX archive")?.try_into().unwrap()) as usize;
    let mut total = 0usize;
    while bytes.get(cursor..cursor + 4) == Some(b"PK\x01\x02") {
        let size =
            u32::from_le_bytes(bytes.get(cursor + 24..cursor + 28).ok_or("Invalid DOCX archive")?.try_into().unwrap())
                as usize;
        total = total.saturating_add(size);
        if size > 32 * 1024 * 1024 || total > 64 * 1024 * 1024 {
            return Err("DOCX expanded content exceeds 64MB".into());
        }
        let name =
            u16::from_le_bytes(bytes.get(cursor + 28..cursor + 30).ok_or("Invalid DOCX archive")?.try_into().unwrap())
                as usize;
        let extra =
            u16::from_le_bytes(bytes.get(cursor + 30..cursor + 32).ok_or("Invalid DOCX archive")?.try_into().unwrap())
                as usize;
        let comment =
            u16::from_le_bytes(bytes.get(cursor + 32..cursor + 34).ok_or("Invalid DOCX archive")?.try_into().unwrap())
                as usize;
        cursor = cursor.checked_add(46 + name + extra + comment).ok_or("Invalid DOCX archive")?;
    }
    let entries = pi_coding_agent::diagnostics::zip::read_zip_archive(bytes).map_err(|e| e.to_string())?;
    let mut media = HashMap::new();
    let mut document = None;
    let mut rels_xml = String::new();
    let mut numbering_xml = String::new();
    for entry in entries {
        if entry.name == "word/document.xml" {
            document = Some(String::from_utf8_lossy(&entry.data).into_owned());
        } else if entry.name == "word/_rels/document.xml.rels" {
            rels_xml = String::from_utf8_lossy(&entry.data).into_owned();
        } else if entry.name == "word/numbering.xml" {
            numbering_xml = String::from_utf8_lossy(&entry.data).into_owned();
        } else if entry.name.starts_with("word/media/") {
            media.insert(entry.name, entry.data);
        }
    }
    let document = document.ok_or("DOCX has no document.xml")?;
    let rels = relationship_targets(&rels_xml);
    let number_styles = list_styles(&numbering_xml);
    let html = render_document(&document, &rels, &media, &number_styles);
    let title = html_escape(file_name);
    Ok(format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><style>:root{{color-scheme:light}}html,body{{margin:0;min-height:100%;background:#eef1f5;color:#171717}}body{{font-family:-apple-system,BlinkMacSystemFont,\"Segoe UI\",sans-serif;padding:28px}}main{{box-sizing:border-box;max-width:840px;min-height:calc(100vh - 56px);margin:0 auto;padding:56px 64px;background:white;box-shadow:0 8px 28px #0f172a24}}.file-title{{margin:0 0 28px;padding-bottom:10px;border-bottom:1px solid #e5e7eb;color:#6b7280;font:12px ui-monospace,monospace;word-break:break-word}}table{{border-collapse:collapse;max-width:100%;margin:1em 0}}td{{border:1px solid #d1d5db;padding:6px 9px;vertical-align:top}}img{{max-width:100%;height:auto}}p{{line-height:1.7}}@media(max-width:720px){{body{{padding:0;background:#fff}}main{{min-height:100vh;padding:28px 22px;box-shadow:none}}}}</style></head><body><main><div class=\"file-title\">{title}</div>{html}</main></body></html>"
    ))
}

async fn docx_preview(path: &Path) -> Response {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(_) => return error(StatusCode::NOT_FOUND, "Not found"),
    };
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("document.docx");
    let html = match docx_html(&bytes, name) {
        Ok(html) => html,
        Err(message) => return error(StatusCode::INTERNAL_SERVER_ERROR, message),
    };
    let mut response = Response::new(Body::from(html));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response.headers_mut().insert(header::CONTENT_SECURITY_POLICY,HeaderValue::from_static("default-src 'none'; img-src data:; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'self'"));
    response.headers_mut().insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response.headers_mut().insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

pub(super) async fn post(
    State(state): State<AppState>,
    RoutePath(route_path): RoutePath<String>,
    Query(query): Query<HashMap<String, String>>,
    request: Request,
) -> Response {
    let path = match api_path(&route_path) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let request_type = query.get("type").map(String::as_str).unwrap_or("upload");
    let roots = crate::web_backend::path_access::allowed_roots(&state);
    if request_type == "allow-link" {
        return allow_link(&state, &path, &roots, request).await;
    }
    let (roots, _) = match authorize(&state, &path, "upload", None).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let directory = match path.canonicalize() {
        Ok(p) if p.is_dir() && within(&p, &roots) => p,
        Ok(p) if !within(&p, &roots) => return error(StatusCode::FORBIDDEN, "Access denied"),
        Ok(_) => return error(StatusCode::BAD_REQUEST, "Upload target is not a directory"),
        Err(_) => return error(StatusCode::NOT_FOUND, "Upload directory not found"),
    };
    match request_type {
        "allow-link" => unreachable!("handled before resolving linked directory"),
        "upload-check" => upload_check(&directory, request).await,
        "upload" => upload_files(&directory, &query, request, &state).await,
        _ => error(StatusCode::BAD_REQUEST, "Invalid upload request type"),
    }
}

async fn allow_link(_state: &AppState, path: &Path, roots: &[PathBuf], request: Request) -> Response {
    let bytes = match to_bytes(request.into_body(), 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large"),
    };
    let body = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
    let Some(expected) = body.get("target").and_then(Value::as_str) else {
        return error(StatusCode::BAD_REQUEST, "target must be the link target shown in the listing");
    };
    if !within(path, roots) || !path.parent().is_some_and(|p| authorized_existing(p, roots)) {
        return error(StatusCode::FORBIDDEN, "Access denied");
    }
    let link = match tokio::fs::symlink_metadata(path).await {
        Ok(s) => s,
        Err(_) => return error(StatusCode::NOT_FOUND, "Not found"),
    };
    if !link.file_type().is_symlink() {
        return error(StatusCode::BAD_REQUEST, "Not a linked directory");
    }
    let real = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => return error(StatusCode::NOT_FOUND, "Link target not found"),
    };
    if !tokio::fs::metadata(&real).await.is_ok_and(|m| m.is_dir()) {
        return error(StatusCode::BAD_REQUEST, "Not a linked directory");
    }
    let expected_path = PathBuf::from(expected);
    let same_target = expected_path.canonicalize().ok().is_some_and(|expected_real| {
        if cfg!(windows) {
            expected_real.to_string_lossy().eq_ignore_ascii_case(&real.to_string_lossy())
        } else {
            expected_real == real
        }
    });
    if !same_target {
        return error(StatusCode::CONFLICT, "Link target changed; refresh the file list");
    }
    let mut home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .and_then(|p| p.canonicalize().ok());
    if real.parent().is_none()
        || roots.iter().any(|root| root.starts_with(&real))
        || home.take().is_some_and(|h| real == h || h.starts_with(&real))
    {
        return error(StatusCode::CONFLICT, "Linked directory would expose the home or filesystem root");
    }
    if !within(&real, roots) {
        if crate::web_backend::path_access::allow_root(&real).is_err() {
            return error(StatusCode::BAD_REQUEST, "Failed to allow linked directory");
        }
    }
    Json(json!({"path":real.to_string_lossy()})).into_response()
}

async fn upload_check(directory: &Path, request: Request) -> Response {
    let bytes = match to_bytes(request.into_body(), 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large"),
    };
    let Some(body) = serde_json::from_slice::<Value>(&bytes).ok() else {
        return error(StatusCode::BAD_REQUEST, "fileNames must be an array of strings");
    };
    inspect_targets(
        directory,
        body.get("fileNames")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
    )
}
fn validate_names(names: &[String]) -> Option<String> {
    if names.is_empty() {
        return Some("No files selected".into());
    }
    let mut seen = std::collections::HashSet::new();
    for name in names {
        if name.is_empty() || name == "." || name == ".." || name.contains('\0') {
            return Some(format!("Invalid file name: {}", if name.is_empty() { "(empty)" } else { name }));
        }
        if name.contains('/')
            || name.contains('\\')
            || Path::new(name).file_name().and_then(|x| x.to_str()) != Some(name)
        {
            return Some(format!("File names must not contain a path: {name}"));
        }
        if !seen.insert(name) {
            return Some(format!("Duplicate file name in upload: {name}"));
        }
    }
    None
}
fn inspect_targets(directory: &Path, names: Vec<String>) -> Response {
    if let Some(message) = validate_names(&names) {
        return error(StatusCode::BAD_REQUEST, message);
    }
    let mut conflicts = Vec::new();
    let mut non_replaceable = Vec::new();
    for name in names {
        let target = directory.join(&name);
        if let Ok(metadata) = std::fs::symlink_metadata(&target) {
            conflicts.push(name.clone());
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                non_replaceable.push(name);
            }
        }
    }
    Json(json!({"conflicts":conflicts,"nonReplaceable":non_replaceable})).into_response()
}
async fn upload_files(
    directory: &Path,
    query: &HashMap<String, String>,
    request: Request,
    state: &AppState,
) -> Response {
    let strategy = query.get("conflict").map(String::as_str).unwrap_or("error");
    if !matches!(strategy, "error" | "overwrite" | "skip") {
        return error(StatusCode::BAD_REQUEST, "Invalid conflict strategy");
    }
    let mut multipart = match Multipart::from_request(request, state).await {
        Ok(m) => m,
        Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid multipart upload"),
    };
    let mut files = Vec::<(String, Vec<u8>)>::new();
    let mut total = 0usize;
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                if field.name() != Some("files") {
                    continue;
                }
                let name = field.file_name().unwrap_or_default().to_string();
                let bytes = match field.bytes().await {
                    Ok(v) => v,
                    Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "Uploads must total 100MB or less"),
                };
                total += bytes.len();
                if total > MAX_UPLOAD_TOTAL_BYTES {
                    return error(StatusCode::PAYLOAD_TOO_LARGE, "Uploads must total 100MB or less");
                }
                files.push((name, bytes.to_vec()));
            }
            Ok(None) => break,
            Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid multipart upload"),
        }
    }
    let names = files.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
    if let Some(message) = validate_names(&names) {
        return error(StatusCode::BAD_REQUEST, message);
    }
    let inspection = inspect_targets_data(directory, &names);
    if strategy == "error" && !inspection.0.is_empty() {
        return (StatusCode::CONFLICT,Json(json!({"error":"One or more files already exist","conflicts":inspection.0,"nonReplaceable":inspection.1}))).into_response();
    }
    let mut uploaded = Vec::new();
    let mut skipped = Vec::new();
    let mut errors = Vec::new();
    for (name, bytes) in files {
        let target = directory.join(&name);
        let existing = std::fs::symlink_metadata(&target).ok();
        if existing.is_some() && strategy == "skip" {
            skipped.push(name);
            continue;
        }
        if existing.as_ref().is_some_and(|m| !m.file_type().is_file() || m.file_type().is_symlink()) {
            errors.push(json!({"name":name,"error":"Cannot replace a directory or symbolic link"}));
            continue;
        }
        let result = write_upload(&target, &bytes, existing.as_ref()).await;
        match result {
            Ok(()) => uploaded.push(name),
            Err(e) => errors.push(json!({"name":name,"error":e.to_string()})),
        }
    }
    (
        if errors.is_empty() { StatusCode::OK } else { StatusCode::MULTI_STATUS },
        Json(json!({"uploaded":uploaded,"skipped":skipped,"errors":errors})),
    )
        .into_response()
}
fn inspect_targets_data(directory: &Path, names: &[String]) -> (Vec<String>, Vec<String>) {
    let mut c = Vec::new();
    let mut n = Vec::new();
    for name in names {
        if let Ok(meta) = std::fs::symlink_metadata(directory.join(name)) {
            c.push(name.clone());
            if !meta.file_type().is_file() || meta.file_type().is_symlink() {
                n.push(name.clone());
            }
        }
    }
    (c, n)
}
async fn write_upload(target: &Path, bytes: &[u8], existing: Option<&std::fs::Metadata>) -> io::Result<()> {
    let parent = target.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid upload path"))?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let staging_dir = parent.join(format!(".pi-upload-{}-{nonce}", std::process::id()));
    tokio::fs::create_dir(&staging_dir).await?;
    let stage = staging_dir.join("upload");
    let mode = existing.and_then(|m| permissions_mode(m));
    let result = async {
        tokio::fs::write(&stage, bytes).await?;
        if let Some(mode) = mode {
            set_mode(&stage, mode).await?;
        }
        if let Some(meta) = tokio::fs::symlink_metadata(target).await.ok() {
            if !meta.file_type().is_file() || meta.file_type().is_symlink() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "Cannot replace a directory or symbolic link"));
            }
        }
        if cfg!(windows) {
            if tokio::fs::try_exists(target).await.unwrap_or(false) {
                tokio::fs::remove_file(target).await?;
            }
        }
        tokio::fs::rename(&stage, target).await
    }
    .await;
    let _ = tokio::fs::remove_dir_all(staging_dir).await;
    result
}
#[cfg(unix)]
fn permissions_mode(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o777)
}
#[cfg(not(unix))]
fn permissions_mode(_: &std::fs::Metadata) -> Option<u32> {
    None
}
#[cfg(unix)]
async fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await
}
#[cfg(not(unix))]
async fn set_mode(_: &Path, _: u32) -> io::Result<()> {
    Ok(())
}
