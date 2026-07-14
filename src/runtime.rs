use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub root: PathBuf,
    pub backend: PathBuf,
}

impl RuntimeConfig {
    pub fn from_env() -> Result<Self> {
        let root = std::env::var_os("CLM_RUNTIME_ROOT")
            .map(PathBuf::from)
            .map(Ok)
            .unwrap_or_else(default_runtime_root)?;
        let backend = std::env::var_os("CLM_CODEX_BACKEND")
            .map(PathBuf::from)
            .context("CLM_CODEX_BACKEND is not set")?;
        if !backend.is_file() {
            bail!(
                "configured Codex backend does not exist: {}",
                backend.display()
            );
        }
        Ok(Self { root, backend })
    }

    pub fn index_root(&self) -> PathBuf {
        self.root.join("Data").join("Indexes")
    }

    pub fn index_path(&self, thread_id: &str) -> Result<PathBuf> {
        validate_thread_id(thread_id)?;
        Ok(self.index_root().join(format!("{thread_id}.sqlite")))
    }
}

pub fn default_runtime_root() -> Result<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    Ok(PathBuf::from(local).join("ConversationLifecycleManager"))
}

pub fn default_codex_home() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(home));
    }
    let profile = std::env::var_os("USERPROFILE").context("USERPROFILE is not set")?;
    Ok(PathBuf::from(profile).join(".codex"))
}

pub fn index_path(index_root: &Path, thread_id: &str) -> Result<PathBuf> {
    validate_thread_id(thread_id)?;
    Ok(index_root.join(format!("{thread_id}.sqlite")))
}

fn validate_thread_id(thread_id: &str) -> Result<()> {
    if thread_id.is_empty()
        || !thread_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        bail!("invalid thread id {thread_id:?}");
    }
    Ok(())
}
