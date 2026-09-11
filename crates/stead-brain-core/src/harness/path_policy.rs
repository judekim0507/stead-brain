// SPDX-License-Identifier: MIT

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use pie_agent_core::AgentToolError;
use stead_brain_protocol::FileAccessMode;

use crate::{BrainError, FileAccess, Result, canonicalize_existing, is_safe_session_id};

const OUTSIDE_PREFIX: &str = "Path is outside the session workspace";

#[derive(Clone, Debug)]
pub struct PathPolicy {
    files: Arc<FileAccess>,
    session_id: String,
    workspace: PathBuf,
    artifacts: PathBuf,
}

impl PathPolicy {
    pub fn new(files: Arc<FileAccess>, session_id: String) -> Self {
        let session = files.session_root.join(&session_id);
        Self {
            files,
            session_id,
            workspace: session.join("workspace"),
            artifacts: session.join("artifacts"),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub async fn ensure_workspace(&self) -> Result<PathBuf> {
        if !is_safe_session_id(&self.session_id) {
            return Err(BrainError::InvalidRequest("invalid session id".to_string()));
        }
        let session = self.session_dir();
        tokio::fs::create_dir_all(&session).await?;
        self.ensure_directory_is_contained_async(&session, &self.files.session_root)
            .await?;
        tokio::fs::create_dir_all(&self.workspace).await?;
        tokio::fs::create_dir_all(&self.artifacts).await?;
        self.ensure_directory_is_contained_async(&self.workspace, &session)
            .await?;
        self.ensure_directory_is_contained_async(&self.artifacts, &session)
            .await?;
        let link = self.workspace.join("artifacts");
        match tokio::fs::symlink_metadata(&link).await {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = canonicalize_existing(&link).await?;
                let artifacts = canonicalize_existing(&self.artifacts).await?;
                if target != artifacts {
                    return Err(BrainError::FileAccessDenied(Self::outside_message(&link)));
                }
            }
            Ok(_) => {
                return Err(BrainError::FileAccessDenied(Self::outside_message(&link)));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                create_directory_symlink(&self.artifacts, &link)?;
            }
            Err(error) => return Err(error.into()),
        }
        canonicalize_existing(&self.workspace).await
    }

    pub fn resolve_existing(&self, raw: &str) -> std::result::Result<PathBuf, AgentToolError> {
        self.ensure_workspace_sync()?;
        let candidate = self.candidate(raw)?;
        let canonical = match std::fs::canonicalize(&candidate) {
            Ok(canonical) => canonical,
            Err(error) => {
                if std::fs::symlink_metadata(&candidate)
                    .is_ok_and(|meta| meta.file_type().is_symlink())
                    || nearest_existing_ancestor(&candidate)
                        .and_then(|ancestor| std::fs::canonicalize(ancestor).ok())
                        .is_some_and(|ancestor| !self.is_allowed(&ancestor))
                {
                    return Err(AgentToolError::from(Self::outside_message(Path::new(raw))));
                }
                return Err(AgentToolError::from(format!(
                    "{}: {error}",
                    candidate.display()
                )));
            }
        };
        if self.is_allowed(&canonical) {
            Ok(canonical)
        } else {
            Err(AgentToolError::from(Self::outside_message(Path::new(raw))))
        }
    }

    pub fn resolve_write(&self, raw: &str) -> std::result::Result<PathBuf, AgentToolError> {
        self.ensure_workspace_sync()?;
        let candidate = self.candidate(raw)?;
        self.ensure_creation_target_allowed(&candidate, raw)?;
        if let Ok(meta) = std::fs::symlink_metadata(&candidate) {
            if meta.file_type().is_symlink() {
                let canonical = std::fs::canonicalize(&candidate)
                    .map_err(|_| AgentToolError::from(Self::outside_message(Path::new(raw))))?;
                if !self.is_allowed(&canonical) {
                    return Err(AgentToolError::from(Self::outside_message(Path::new(raw))));
                }
            }
        }
        Ok(candidate)
    }

    pub fn classify_write(&self, raw: &str) -> Option<String> {
        self.resolve_write(raw).err().and_then(|error| {
            let message = error.to_string();
            message.starts_with(OUTSIDE_PREFIX).then_some(message)
        })
    }

    pub fn denied_shell_path(&self, raw: &str) -> bool {
        if self.files.mode == FileAccessMode::FullDisk {
            return false;
        }
        let expanded = if raw == "~" || raw.starts_with("~/") {
            let Some(home) = std::env::var_os("HOME") else {
                return true;
            };
            PathBuf::from(home).join(raw.trim_start_matches('~').trim_start_matches('/'))
        } else if raw == "$HOME" || raw.starts_with("$HOME/") {
            let Some(home) = std::env::var_os("HOME") else {
                return true;
            };
            PathBuf::from(home).join(raw.trim_start_matches("$HOME").trim_start_matches('/'))
        } else if raw == "${HOME}" || raw.starts_with("${HOME}/") {
            let Some(home) = std::env::var_os("HOME") else {
                return true;
            };
            PathBuf::from(home).join(raw.trim_start_matches("${HOME}").trim_start_matches('/'))
        } else {
            PathBuf::from(raw)
        };
        let expanded = if expanded.is_absolute() {
            expanded
        } else {
            self.workspace.join(expanded)
        };
        let canonical = if expanded.exists() {
            std::fs::canonicalize(&expanded).ok()
        } else {
            nearest_existing_ancestor(&expanded)
                .and_then(|ancestor| std::fs::canonicalize(ancestor).ok())
        };
        canonical.is_none_or(|path| !self.is_allowed(&path))
    }

    fn ensure_workspace_sync(&self) -> std::result::Result<(), AgentToolError> {
        if !is_safe_session_id(&self.session_id) {
            return Err(AgentToolError::from("invalid session id"));
        }
        let session = self.session_dir();
        std::fs::create_dir_all(&session)
            .map_err(|error| AgentToolError::from(format!("session: {error}")))?;
        self.ensure_directory_is_contained_sync(&session, &self.files.session_root)?;
        std::fs::create_dir_all(&self.workspace)
            .map_err(|error| AgentToolError::from(format!("workspace: {error}")))?;
        std::fs::create_dir_all(&self.artifacts)
            .map_err(|error| AgentToolError::from(format!("artifacts: {error}")))?;
        self.ensure_directory_is_contained_sync(&self.workspace, &session)?;
        self.ensure_directory_is_contained_sync(&self.artifacts, &session)?;
        let link = self.workspace.join("artifacts");
        match std::fs::symlink_metadata(&link) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = std::fs::canonicalize(&link)
                    .map_err(|error| AgentToolError::from(format!("artifacts link: {error}")))?;
                let artifacts = std::fs::canonicalize(&self.artifacts)
                    .map_err(|error| AgentToolError::from(format!("artifacts: {error}")))?;
                if target != artifacts {
                    return Err(AgentToolError::from(Self::outside_message(&link)));
                }
            }
            Ok(_) => return Err(AgentToolError::from(Self::outside_message(&link))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                create_directory_symlink(&self.artifacts, &link)
                    .map_err(|error| AgentToolError::from(format!("artifacts link: {error}")))?;
            }
            Err(error) => {
                return Err(AgentToolError::from(format!("artifacts link: {error}")));
            }
        }
        Ok(())
    }

    fn session_dir(&self) -> PathBuf {
        self.files.session_root.join(&self.session_id)
    }

    async fn ensure_directory_is_contained_async(&self, path: &Path, parent: &Path) -> Result<()> {
        let metadata = tokio::fs::symlink_metadata(path).await?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(BrainError::FileAccessDenied(Self::outside_message(path)));
        }
        let canonical = canonicalize_existing(path).await?;
        let canonical_parent = canonicalize_existing(parent).await?;
        if !canonical.starts_with(canonical_parent) {
            return Err(BrainError::FileAccessDenied(Self::outside_message(path)));
        }
        Ok(())
    }

    fn ensure_directory_is_contained_sync(
        &self,
        path: &Path,
        parent: &Path,
    ) -> std::result::Result<(), AgentToolError> {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| AgentToolError::from(format!("{}: {error}", path.display())))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(AgentToolError::from(Self::outside_message(path)));
        }
        let canonical = std::fs::canonicalize(path)
            .map_err(|error| AgentToolError::from(format!("{}: {error}", path.display())))?;
        let canonical_parent = std::fs::canonicalize(parent)
            .map_err(|error| AgentToolError::from(format!("{}: {error}", parent.display())))?;
        if !canonical.starts_with(canonical_parent) {
            return Err(AgentToolError::from(Self::outside_message(path)));
        }
        Ok(())
    }

    fn candidate(&self, raw: &str) -> std::result::Result<PathBuf, AgentToolError> {
        if raw.trim().is_empty() {
            return Err(AgentToolError::from("path is empty"));
        }
        let path = Path::new(raw);
        if path.is_absolute() {
            return Ok(path.to_path_buf());
        }
        let mut depth = 0usize;
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(_) => depth += 1,
                Component::ParentDir if depth > 0 => depth -= 1,
                Component::ParentDir => {
                    return Err(AgentToolError::from(Self::outside_message(path)));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(AgentToolError::from(Self::outside_message(path)));
                }
            }
        }
        Ok(self.workspace.join(path))
    }

    fn ensure_creation_target_allowed(
        &self,
        candidate: &Path,
        raw: &str,
    ) -> std::result::Result<(), AgentToolError> {
        let ancestor = nearest_existing_ancestor(candidate)
            .ok_or_else(|| AgentToolError::from(Self::outside_message(Path::new(raw))))?;
        let canonical = std::fs::canonicalize(ancestor)
            .map_err(|_| AgentToolError::from(Self::outside_message(Path::new(raw))))?;
        if self.is_allowed(&canonical) {
            Ok(())
        } else {
            Err(AgentToolError::from(Self::outside_message(Path::new(raw))))
        }
    }

    fn is_allowed(&self, canonical: &Path) -> bool {
        let workspace =
            std::fs::canonicalize(&self.workspace).unwrap_or_else(|_| self.workspace.clone());
        let artifacts =
            std::fs::canonicalize(&self.artifacts).unwrap_or_else(|_| self.artifacts.clone());
        if canonical.starts_with(&workspace) || canonical.starts_with(&artifacts) {
            return true;
        }
        match self.files.mode {
            FileAccessMode::SessionOnly => false,
            FileAccessMode::ApprovedRoots => self
                .files
                .roots
                .iter()
                .any(|root| canonical.starts_with(&root.path)),
            FileAccessMode::FullDisk => true,
        }
    }

    fn outside_message(path: &Path) -> String {
        format!("{OUTSIDE_PREFIX}: {}", path.display())
    }
}

fn nearest_existing_ancestor(path: &Path) -> Option<&Path> {
    let mut candidate = Some(path);
    while let Some(current) = candidate {
        if std::fs::symlink_metadata(current).is_ok() {
            return Some(current);
        }
        candidate = current.parent();
    }
    None
}

#[cfg(unix)]
fn create_directory_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_directory_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}
