use crate::agent_session::AgentSessionRuntime;
use crate::app::{AppOptions, create_app_runtime};
use crate::session::SessionManager;
use async_trait::async_trait;
use std::path::PathBuf;

/// Application capabilities supplied by pi to an in-process web backend.
#[async_trait]
pub trait Host: Send + Sync {
    fn agent_dir(&self) -> PathBuf;
    async fn create_app_runtime(
        &self,
        options: AppOptions,
        session_manager: SessionManager,
    ) -> anyhow::Result<AgentSessionRuntime>;
}

/// The command-line application's Host implementation.
#[derive(Clone)]
pub struct PiHost {
    agent_dir: PathBuf,
}

impl PiHost {
    pub fn new() -> Self {
        Self { agent_dir: crate::config::agent_dir() }
    }
}

impl Default for PiHost {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Host for PiHost {
    fn agent_dir(&self) -> PathBuf {
        self.agent_dir.clone()
    }

    async fn create_app_runtime(
        &self,
        options: AppOptions,
        session_manager: SessionManager,
    ) -> anyhow::Result<AgentSessionRuntime> {
        create_app_runtime(options, session_manager).await
    }
}
