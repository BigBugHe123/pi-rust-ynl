use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;

use crate::rpc_worker::RpcSessionWorker;
use crate::terminal::TerminalManager;
use crate::web_ui::WebExtensionUi;

#[derive(Clone)]
pub(crate) struct WebSession {
    pub(crate) rpc_worker: Arc<RpcSessionWorker>,
    pub(crate) events: broadcast::Sender<serde_json::Value>,
    pub(crate) ui: Arc<WebExtensionUi>,
    pub(crate) created_at: i64,
}

impl WebSession {
    pub(crate) async fn dispose(&self) {
        self.rpc_worker.stop().await;
    }

    pub(crate) async fn session_id(&self) -> String {
        self.rpc_worker.state().await.session_id
    }

    pub(crate) async fn cwd(&self) -> PathBuf {
        self.rpc_worker.identity().await.cwd
    }

    pub(crate) async fn session_file(&self) -> Option<PathBuf> {
        self.rpc_worker.session_file().await
    }

    pub(crate) async fn is_busy(&self) -> anyhow::Result<bool> {
        self.rpc_worker.ensure_alive()?;
        self.rpc_worker
            .refresh_state()
            .await
            .map(|state| state.is_streaming || state.is_bash_running || state.is_compacting)
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
