use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};

pub(crate) fn static_dir() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os("PI_WEB_STATIC_DIR") {
        let configured = PathBuf::from(configured);
        if configured.is_absolute() {
            return Some(configured);
        }
        return std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(|parent| parent.join(&configured)))
            .or_else(|| Some(configured));
    }

    let executable_web = std::env::current_exe().ok()?.parent()?.join("web");
    if executable_web.is_dir() {
        return Some(executable_web);
    }

    // Development fallback for running the backend from the source checkout.
    let source_export = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../pi-web-ynl/out");
    source_export.is_dir().then_some(source_export)
}

fn safe_relative_path(path: &str) -> Option<PathBuf> {
    let path = path.trim_start_matches('/').trim_end_matches('/');
    if path.is_empty() {
        return Some(PathBuf::from("index.html"));
    }

    let mut result = PathBuf::new();
    for segment in path.split('/') {
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.chars().any(|ch| matches!(ch, '\\' | ':' | '\0'))
        {
            return None;
        }
        result.push(segment);
    }
    Some(result)
}

fn is_within_root(root: &Path, candidate: &Path) -> bool {
    candidate.starts_with(root)
}

async fn existing_file(root: &Path, relative: &Path) -> Option<PathBuf> {
    let root = tokio::fs::canonicalize(root).await.ok()?;
    let candidate = tokio::fs::canonicalize(root.join(relative)).await.ok()?;
    if !is_within_root(&root, &candidate) || !tokio::fs::metadata(&candidate).await.ok()?.is_file() {
        return None;
    }
    Some(candidate)
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|value| value.to_str()).unwrap_or_default().to_ascii_lowercase().as_str() {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "webmanifest" => "application/manifest+json",
        "txt" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

fn cache_control(path: &Path, uri_path: &str) -> &'static str {
    let name = path.file_name().and_then(|value| value.to_str()).unwrap_or_default();
    if name == "sw.js" || name == "manifest.webmanifest" || path.extension().is_some_and(|ext| ext == "html") {
        "no-cache"
    } else if uri_path.starts_with("/_next/static/") {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    }
}

fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

pub(crate) async fn serve(request: Request) -> Response {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }

    let uri_path = request.uri().path();
    if uri_path == "/api" || uri_path.starts_with("/api/") || uri_path.starts_with("/_next/") && uri_path.contains("..")
    {
        return not_found();
    }

    let Some(root) = static_dir() else { return not_found() };
    let Some(relative) = safe_relative_path(uri_path) else { return StatusCode::BAD_REQUEST.into_response() };

    let mut selected = existing_file(&root, &relative).await;
    if selected.is_none() && uri_path.starts_with("/_next/") {
        return not_found();
    }
    if selected.is_none() && relative.extension().is_none() {
        let html_path = relative.with_extension("html");
        selected = existing_file(&root, &html_path).await;
    }
    if selected.is_none() && relative.extension().is_none() {
        selected = existing_file(&root, Path::new("index.html")).await;
    }
    let Some(path) = selected else { return not_found() };

    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(_) => return not_found(),
    };
    let mime = HeaderValue::from_static(content_type(&path));
    let cache = HeaderValue::from_static(cache_control(&path, uri_path));
    let mut response = Response::new(if request.method() == Method::HEAD { Body::empty() } else { Body::from(bytes) });
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(header::CONTENT_TYPE, mime);
    response.headers_mut().insert(header::CACHE_CONTROL, cache);
    response.headers_mut().insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if path.file_name().is_some_and(|name| name == "sw.js") {
        response.headers_mut().insert("service-worker-allowed", HeaderValue::from_static("/"));
    }
    response
}
