//! One Pi RPC subprocess and its current session identity.
//!
//! A worker owns exactly one active Pi process. RPC commands such as `fork` can
//! switch that process to another session, so callers must read `state()` after
//! session-changing commands and update the backend's session-to-worker index.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pi_ai::ThinkingLevel;
use pi_coding_agent::rpc::{RpcClient, RpcClientOptions, RpcCommand, RpcSessionState};
use serde_json::Value;
use tokio::sync::{Mutex, RwLock, broadcast};

pub(crate) struct RpcWorkerOptions {
    pub(crate) cwd: PathBuf,
    pub(crate) session_file: Option<PathBuf>,
    pub(crate) agent_dir: PathBuf,
    pub(crate) provider: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) thinking_level: Option<ThinkingLevel>,
    pub(crate) tool_names: Option<Vec<String>>,
}

pub(crate) struct RpcSessionWorker {
    client: Arc<RpcClient>,
    command_lock: Mutex<()>,
    state: Arc<RwLock<RpcSessionState>>,
    pub(crate) events: broadcast::Sender<Value>,
    pub(crate) ui: Arc<crate::web_ui::WebExtensionUi>,
    cwd: PathBuf,
    agent_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub(crate) struct RpcSessionIdentity {
    pub(crate) cwd: PathBuf,
    pub(crate) session_file: Option<PathBuf>,
}

impl RpcSessionWorker {
    pub(crate) async fn start(options: RpcWorkerOptions) -> anyhow::Result<Self> {
        let cwd = options.cwd.clone();
        let agent_dir = options.agent_dir.clone();
        let mut args = Vec::new();
        if let Some(path) = &options.session_file {
            args.extend(["--session".to_owned(), path.display().to_string()]);
        }
        let client = Arc::new(RpcClient::new(RpcClientOptions {
            program: pi_executable(),
            cwd: Some(options.cwd),
            env: vec![("PI_CODING_AGENT_DIR".into(), options.agent_dir.display().to_string())],
            provider: options.provider,
            model: options.model,
            args,
            request_timeout: Duration::from_secs(120),
            forward_stderr: true,
        }));

        // Subscribe before spawn so startup extension events are not lost.
        let mut records = client.subscribe();
        client.start().await?;
        let setup = async {
            if let Some(level) = options.thinking_level {
                client.set_thinking_level(level).await?;
            }
            if let Some(names) = options.tool_names {
                client.set_tools_transient(Some(names)).await?;
            }
            client.get_state().await
        }
        .await;
        let initial_state = match setup {
            Ok(state) => state,
            Err(error) => {
                client.stop().await;
                return Err(error);
            }
        };
        let state = Arc::new(RwLock::new(initial_state));
        let (events, _) = broadcast::channel(512);
        let event_sender = events.clone();
        let ui = Arc::new(crate::web_ui::WebExtensionUi::default());
        let observed = ui.clone();
        tokio::spawn(async move {
            while let Some(record) = records.recv().await {
                observed.observe(&record);
                let _ = event_sender.send(record);
            }
            let _ = event_sender.send(serde_json::json!({"type":"server_shutdown"}));
        });

        Ok(Self { client, command_lock: Mutex::new(()), state, events, ui, cwd, agent_dir })
    }

    pub(crate) async fn request(&self, command: RpcCommand) -> anyhow::Result<Value> {
        let changes_active_session = matches!(
            &command,
            RpcCommand::NewSession { .. }
                | RpcCommand::SwitchSession { .. }
                | RpcCommand::Fork { .. }
                | RpcCommand::CloneSession { .. }
        );
        let _guard = if changes_active_session { Some(self.command_lock.lock().await) } else { None };
        let response = self.client.request(command).await?;
        if changes_active_session {
            self.refresh_state().await?;
        }
        Ok(response)
    }

    pub(crate) async fn refresh_state(&self) -> anyhow::Result<RpcSessionState> {
        let state = self.client.get_state().await?;
        *self.state.write().await = state.clone();
        Ok(state)
    }

    pub(crate) async fn state(&self) -> RpcSessionState {
        self.state.read().await.clone()
    }

    pub(crate) async fn identity(&self) -> RpcSessionIdentity {
        let state = self.state().await;
        RpcSessionIdentity { cwd: self.cwd.clone(), session_file: state.session_file.map(PathBuf::from) }
    }

    pub(crate) async fn session_file(&self) -> Option<PathBuf> {
        self.state().await.session_file.map(PathBuf::from)
    }

    pub(crate) fn ensure_alive(&self) -> anyhow::Result<()> {
        if let Some(error) = self.client.exit_error() {
            anyhow::bail!("Pi RPC worker exited: {error}");
        }
        Ok(())
    }

    pub(crate) async fn respond_to_ui(
        &self,
        request_id: &str,
        response: pi_coding_agent::rpc::RpcUiResponse,
    ) -> anyhow::Result<()> {
        self.client.respond_to_ui(request_id, response).await?;
        self.ui.dismiss(request_id);
        Ok(())
    }

    pub(crate) async fn generate_and_set_session_title(&self, expected_session_id: &str) -> anyhow::Result<Value> {
        let _command_guard = self.command_lock.lock().await;
        if self.state().await.session_id != expected_session_id {
            anyhow::bail!("The active RPC session changed before title generation started");
        }
        let result = self.client.generate_session_title().await?;
        let name =
            result["title"].as_str().ok_or_else(|| anyhow::anyhow!("RPC did not return a generated session title"))?;
        if self.state().await.session_id != expected_session_id {
            anyhow::bail!("The active RPC session changed while its title was being generated");
        }
        self.client.set_session_name(name).await?;
        Ok(result)
    }

    pub(crate) async fn stop(&self) {
        self.client.stop().await;
    }

    /// Reopen another persistent session under the same process configuration. This is used
    /// after RPC `fork`/`clone` changes the worker's active session and the old session needs
    /// its own worker again.
    pub(crate) async fn reopen(&self, path: &Path) -> anyhow::Result<Self> {
        Self::open_session_file(path, self.cwd.clone(), self.agent_dir.clone()).await
    }

    pub(crate) async fn open_session_file(path: &Path, cwd: PathBuf, agent_dir: PathBuf) -> anyhow::Result<Self> {
        let worker = Self::start(RpcWorkerOptions {
            cwd,
            session_file: Some(path.to_path_buf()),
            agent_dir,
            provider: None,
            model: None,
            thinking_level: None,
            tool_names: None,
        })
        .await?;
        let entries = worker.request(RpcCommand::GetEntries { since: None }).await?;
        if let Some(tools) =
            session_tool_selection(entries["entries"].as_array().map(Vec::as_slice).unwrap_or_default())
        {
            worker.request(RpcCommand::SetTools { tool_names: Some(tools), persist: false }).await?;
        }
        Ok(worker)
    }
}

fn pi_executable() -> PathBuf {
    if let Ok(current_exe) = std::env::current_exe()
        && let Some(directory) = current_exe.parent()
    {
        #[cfg(windows)]
        let sibling = directory.join("pi.exe");
        #[cfg(not(windows))]
        let sibling = directory.join("pi");
        if sibling.is_file() {
            return sibling;
        }
    }
    if let Some(path) = std::env::var_os("PI_BIN") {
        return PathBuf::from(path);
    }
    PathBuf::from("pi")
}

pub(crate) fn session_tool_selection(entries: &[Value]) -> Option<Vec<String>> {
    for raw in entries.iter().rev() {
        if raw.get("customType").and_then(Value::as_str) != Some("pi-web:tool-selection") {
            continue;
        }
        let Some(data) = raw.get("data") else { continue };
        if data.get("version").and_then(Value::as_u64) != Some(1) {
            continue;
        }
        if data.get("cleared").and_then(Value::as_bool) == Some(true) {
            return None;
        }
        let Some(tools) = data.get("tools").and_then(Value::as_array) else { continue };
        let allowed = ["bash", "read", "edit", "write", "grep", "find", "ls", "powershell"];
        if tools.iter().all(|tool| tool.as_str().is_some_and(|name| allowed.contains(&name))) {
            let mut result = Vec::new();
            for name in tools.iter().filter_map(Value::as_str) {
                if !result.iter().any(|existing| existing == name) {
                    result.push(name.to_string());
                }
            }
            return Some(result);
        }
    }
    None
}
