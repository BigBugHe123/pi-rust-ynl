use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::SigningKey;
use p256::elliptic_curve::Generate;
use serde::Deserialize;
use serde_json::json;

use crate::AppState;

static STATE_LOCK: Mutex<()> = Mutex::new(());
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct VapidKeys {
    public_key: String,
    private_key: String,
}

#[derive(Clone, Deserialize, serde::Serialize)]
struct SubscriptionKeys {
    p256dh: String,
    auth: String,
}

#[derive(Clone, Deserialize, serde::Serialize)]
struct Subscription {
    endpoint: String,
    keys: SubscriptionKeys,
    locale: String,
}

#[derive(Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PushState {
    vapid_keys: VapidKeys,
    subscriptions: Vec<Subscription>,
}

#[derive(Deserialize)]
struct SubscribeRequest {
    subscription: Option<SubscriptionInput>,
    locale: Option<String>,
}

#[derive(Deserialize)]
struct SubscriptionInput {
    endpoint: String,
    keys: SubscriptionKeys,
}

fn state_path(state: &AppState) -> PathBuf {
    state.agent_dir.join("web-push.json")
}

fn new_vapid_keys() -> VapidKeys {
    let signing_key = SigningKey::generate();
    let public_key = signing_key.verifying_key().to_sec1_point(false);
    VapidKeys {
        public_key: URL_SAFE_NO_PAD.encode(public_key.as_bytes()),
        private_key: URL_SAFE_NO_PAD.encode(signing_key.to_bytes()),
    }
}

fn read_state(path: &PathBuf) -> Option<PushState> {
    let text = fs::read_to_string(path).ok()?;
    let state: PushState = serde_json::from_str(&text).ok()?;
    if state.vapid_keys.public_key.is_empty() || state.vapid_keys.private_key.is_empty() {
        return None;
    }
    Some(state)
}

fn write_state(path: &PathBuf, state: &PushState) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| "Invalid push state path".to_owned())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".web-push.json.{}.{}.tmp", std::process::id(), id));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp).map_err(|error| error.to_string())?;
        file.write_all(serde_json::to_string(state).map_err(|error| error.to_string())?.as_bytes())
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        #[cfg(windows)]
        if path.exists() {
            fs::remove_file(path).map_err(|error| error.to_string())?;
        }
        fs::rename(&temp, path).map_err(|error| error.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(|error: String| format!("Failed to write {}: {error}", path.display()))
}

fn load_or_create(path: &PathBuf) -> Result<PushState, String> {
    let _guard = STATE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(state) = read_state(path) {
        return Ok(state);
    }
    let state = PushState { vapid_keys: new_vapid_keys(), subscriptions: Vec::new() };
    write_state(path, &state)?;
    Ok(state)
}

pub async fn config(State(state): State<AppState>) -> Response {
    match load_or_create(&state_path(&state)) {
        Ok(push) => Json(json!({ "publicKey": push.vapid_keys.public_key })).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": error }))).into_response(),
    }
}

pub async fn subscribe(State(state): State<AppState>, body: Bytes) -> Response {
    let request: SubscribeRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "Invalid JSON body" }))).into_response(),
    };
    let Some(subscription) = request.subscription else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "Invalid push subscription" }))).into_response();
    };
    if !subscription.endpoint.starts_with("https://")
        || subscription.keys.p256dh.is_empty()
        || subscription.keys.auth.is_empty()
    {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "Invalid push subscription" }))).into_response();
    }
    let locale = match request.locale.as_deref() {
        Some("zh-CN") => "zh-CN",
        Some("zh-TW") => "zh-TW",
        Some("en") => "en",
        _ => "en",
    };

    let path = state_path(&state);
    let _guard = STATE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut push =
        read_state(&path).unwrap_or_else(|| PushState { vapid_keys: new_vapid_keys(), subscriptions: Vec::new() });
    push.subscriptions.retain(|item| item.endpoint != subscription.endpoint);
    push.subscriptions.push(Subscription {
        endpoint: subscription.endpoint,
        keys: subscription.keys,
        locale: locale.to_owned(),
    });
    match write_state(&path, &push) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": error }))).into_response(),
    }
}
