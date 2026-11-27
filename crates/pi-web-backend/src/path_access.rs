//! Workspace roots shared by file, Git, and workspace picker APIs.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::AppState;

static ADDITIONAL_ROOTS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

fn additional_roots() -> &'static Mutex<HashSet<PathBuf>> {
    ADDITIONAL_ROOTS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Register a canonical workspace selected through the picker.
pub(super) fn allow_root(path: &Path) -> Result<PathBuf, std::io::Error> {
    let canonical = path.canonicalize()?;
    additional_roots().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(canonical.clone());
    Ok(canonical)
}

/// Resolve session, explicit, and recently selected roots for workspace APIs.
pub(super) fn allowed_roots(state: &AppState) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for session in pi_coding_agent::session::listing::list_all_sessions(None, &state.agent_dir) {
        let cwd = PathBuf::from(session.cwd);
        if let Ok(real) = cwd.canonicalize() {
            roots.push(real.clone());
            if let Ok(output) = std::process::Command::new("git")
                .args(["-C"])
                .arg(&real)
                .args(["rev-parse", "--show-toplevel"])
                .output()
            {
                if output.status.success() {
                    if let Some(root) = String::from_utf8_lossy(&output.stdout).lines().next() {
                        if let Ok(root) = PathBuf::from(root).canonicalize() {
                            roots.push(root);
                        }
                    }
                }
            }
        } else if let Some(repo) = infer_removed_worktree_repo(&cwd) {
            // A removed subagent worktree still has a valid session cwd. Keep
            // its originating repository authorized so the model selector can
            // fall back to that project without trusting an arbitrary sibling.
            roots.push(repo);
        }
    }
    if let Some(configured) = std::env::var_os("PI_WEB_ALLOWED_ROOTS") {
        roots.extend(std::env::split_paths(&configured).filter_map(|path| path.canonicalize().ok()));
    }
    roots.extend(additional_roots().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).iter().cloned());
    roots.sort();
    roots.dedup();
    roots
}

/// Canonicalize an existing path and authorize it against the workspace roots.
/// Callers that start processes or read workspace data should use the returned
/// canonical path, not the untrusted input spelling.
pub(super) fn allowed_existing_path(state: &AppState, candidate: &Path) -> Option<PathBuf> {
    let canonical = candidate.canonicalize().ok()?;
    allowed_roots(state).iter().any(|root| canonical.starts_with(root)).then_some(canonical)
}

/// Resolve the originating repository for `<repo>-worktrees/<removed-name>`.
/// Only accept a sibling that still looks like a Git repository.
pub(super) fn infer_removed_worktree_repo(cwd: &Path) -> Option<PathBuf> {
    let parent = cwd.parent()?;
    let worktrees_name = parent.file_name()?.to_string_lossy();
    let repo_name = worktrees_name.strip_suffix("-worktrees")?;
    if repo_name.is_empty() {
        return None;
    }
    let repo = parent.parent()?.join(repo_name);
    let looks_like_repo =
        repo.join(".git").exists() || ["HEAD", "objects", "refs"].iter().all(|name| repo.join(name).exists());
    looks_like_repo.then(|| repo.canonicalize().ok()).flatten()
}
