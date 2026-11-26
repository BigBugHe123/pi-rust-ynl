use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

use pi_coding_agent::agent_session::AgentSessionRuntime;
use pi_coding_agent::app::AppOptions;
use pi_coding_agent::host::Host;
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;

use crate::web_backend::terminal::TerminalManager;
use crate::web_backend::web_ui::WebExtensionUi;

#[derive(Clone)]
pub(crate) struct WebSession {
    pub(crate) runtime: Arc<AgentSessionRuntime>,
    pub(crate) events: broadcast::Sender<serde_json::Value>,
    pub(crate) ui: Arc<WebExtensionUi>,
    pub(crate) bash_running: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) created_at: i64,
}

impl WebSession {
    pub(crate) fn session(&self) -> Arc<pi_coding_agent::agent_session::AgentSession> {
        self.runtime.session()
    }
}

pub(crate) struct PendingLoginCallback {
    pub(crate) provider: String,
    pub(crate) sender: oneshot::Sender<String>,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) sessions: Arc<Mutex<HashMap<String, WebSession>>>,
    pub(crate) opening_sessions: Arc<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>>,
    pub(crate) login_callbacks: Arc<Mutex<HashMap<String, PendingLoginCallback>>>,
    pub(crate) options: AppOptions,
    pub(crate) host: Arc<dyn Host>,
    pub(crate) static_dir: Option<PathBuf>,
    pub(crate) agent_dir: PathBuf,
    pub(crate) session_catalog_version: Arc<Mutex<(Option<u64>, u64)>>,
    pub(crate) started_at: i64,
    pub(crate) stop: CancellationToken,
    pub(crate) terminals: TerminalManager,
    pub(crate) auth_providers_cache: Arc<Mutex<Option<(std::time::Instant, serde_json::Value)>>>,
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn failure(status: axum::http::StatusCode, message: impl Into<String>) -> axum::response::Response {
    use axum::response::IntoResponse;
    (status, axum::Json(serde_json::json!({"error": message.into()}))).into_response()
}

pub(crate) fn ok(data: serde_json::Value) -> axum::response::Response {
    use axum::response::IntoResponse;
    (axum::http::StatusCode::OK, axum::Json(serde_json::json!({"success": true, "data": data}))).into_response()
}
