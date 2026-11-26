use std::sync::OnceLock;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

const CACHE_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const LATEST_URL: &str = "https://registry.npmjs.org/@agegr%2Fpi-web/latest";

#[derive(Clone)]
struct CachedStatus {
    value: Value,
    expires_at: Instant,
}

fn cache() -> &'static Mutex<Option<CachedStatus>> {
    static CACHE: OnceLock<Mutex<Option<CachedStatus>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

#[derive(Deserialize)]
pub struct UpdateQuery {
    #[serde(rename = "currentVersion")]
    current_version: Option<String>,
    skip: Option<String>,
}

fn parse_stable_version(value: &str) -> Option<[u64; 3]> {
    let mut parts = value.split('.');
    let parsed = [parts.next()?, parts.next()?, parts.next()?];
    if parts.next().is_some() || parsed.iter().any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let values = [parsed[0].parse().ok()?, parsed[1].parse().ok()?, parsed[2].parse().ok()?];
    values.iter().all(|part: &u64| *part <= 9_007_199_254_740_991).then_some(values)
}

fn response(current: &str, latest: &str) -> Value {
    let url = format!("https://github.com/agegr/pi-web/releases/tag/v{latest}");
    let update_available = parse_stable_version(latest).zip(parse_stable_version(current)).is_some_and(|(a, b)| a > b);
    json!({"currentVersion":current,"latestVersion":latest,"updateAvailable":update_available,"releaseUrl":url})
}

async fn fetch_status(current: &str) -> Result<Value, String> {
    let registry_response = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())?
        .get(LATEST_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !registry_response.status().is_success() {
        return Err(format!("npm registry returned HTTP {}", registry_response.status()));
    }
    let body: Value = registry_response.json().await.map_err(|error| error.to_string())?;
    let latest = body
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| "npm registry returned an invalid version".to_owned())?;
    if parse_stable_version(latest).is_none() {
        return Err("npm registry returned an invalid version".to_owned());
    }
    Ok(response(current, latest))
}

pub async fn get(Query(query): Query<UpdateQuery>) -> Response {
    let current = query.current_version.as_deref().filter(|value| !value.is_empty()).unwrap_or("0.0.0");
    if query.skip.as_deref() == Some("1") || std::env::var("PI_WEB_SKIP_VERSION_CHECK").as_deref() == Ok("1") {
        return Json(json!({"currentVersion":current,"latestVersion":current,"updateAvailable":false,"releaseUrl":""}))
            .into_response();
    }

    let mut cache = cache().lock().await;
    if let Some(cached) = cache.as_ref().filter(|cached| cached.expires_at > Instant::now()) {
        // The registry result is version-independent; recompute against this UI's current version.
        let latest = cached.value["latestVersion"].as_str().unwrap_or(current);
        return Json(response(current, latest)).into_response();
    }

    match fetch_status(current).await {
        Ok(value) => {
            *cache = Some(CachedStatus { value: value.clone(), expires_at: Instant::now() + CACHE_TTL });
            Json(value).into_response()
        }
        Err(error) => {
            if let Some(cached) = cache.as_ref() {
                let latest = cached.value["latestVersion"].as_str().unwrap_or(current);
                return Json(response(current, latest)).into_response();
            }
            (StatusCode::BAD_GATEWAY, Json(json!({"error":error}))).into_response()
        }
    }
}
