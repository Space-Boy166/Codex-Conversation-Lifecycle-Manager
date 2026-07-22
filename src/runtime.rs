use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub root: PathBuf,
    pub backend: PathBuf,
    pub optimistic_resume: OptimisticResumeRuntimePolicy,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum OptimisticResumeRuntimePolicy {
    #[default]
    Disabled,
    Canary(BTreeSet<String>),
    AllManaged,
}

impl OptimisticResumeRuntimePolicy {
    pub fn enabled_for(&self, thread_id: &str) -> bool {
        match self {
            Self::Disabled => false,
            Self::Canary(thread_ids) => thread_ids.contains(thread_id),
            Self::AllManaged => true,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OptimisticResumeConfigFile {
    format_version: u32,
    mode: String,
    #[serde(default)]
    thread_ids: Vec<String>,
}

impl RuntimeConfig {
    pub fn from_env() -> Result<Self> {
        let root = runtime_root_from_env()?;
        let backend = std::env::var_os("CLM_CODEX_BACKEND")
            .map(PathBuf::from)
            .context("CLM_CODEX_BACKEND is not set")?;
        if !backend.is_file() {
            bail!(
                "configured Codex backend does not exist: {}",
                backend.display()
            );
        }
        let optimistic_resume = load_optimistic_resume_policy_fail_open(&root);
        Ok(Self {
            root,
            backend,
            optimistic_resume,
        })
    }

    pub fn index_root(&self) -> PathBuf {
        self.root.join("Data").join("Indexes")
    }

    pub fn index_path(&self, thread_id: &str) -> Result<PathBuf> {
        validate_thread_id(thread_id)?;
        Ok(self.index_root().join(format!("{thread_id}.sqlite")))
    }
}

pub fn runtime_root_from_env() -> Result<PathBuf> {
    std::env::var_os("CLM_RUNTIME_ROOT")
        .map(PathBuf::from)
        .map(Ok)
        .unwrap_or_else(default_runtime_root)
}

pub fn validate_optimistic_resume_policy(root: &Path) -> Result<()> {
    load_optimistic_resume_policy(root).map(|_| ())
}

fn load_optimistic_resume_policy_fail_open(root: &Path) -> OptimisticResumeRuntimePolicy {
    match load_optimistic_resume_policy(root) {
        Ok(policy) => policy,
        Err(error) => {
            eprintln!(
                "CLM Optimistic Resume policy is invalid; continuing with the optimization disabled: {error:#}"
            );
            OptimisticResumeRuntimePolicy::Disabled
        }
    }
}

fn load_optimistic_resume_policy(root: &Path) -> Result<OptimisticResumeRuntimePolicy> {
    let path = root.join("Data").join("optimistic-resume.json");
    if !path.is_file() {
        return Ok(OptimisticResumeRuntimePolicy::Disabled);
    }
    // Accept UTF-8 BOM. Windows PowerShell `Set-Content -Encoding UTF8` writes one
    // by default; serde_json rejects it with "expected value at line 1 column 1"
    // and would otherwise fail closed before Desktop finishes connecting.
    let mut bytes =
        std::fs::read(&path).with_context(|| format!("failed to open {}", path.display()))?;
    const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
    if bytes.starts_with(UTF8_BOM) {
        bytes = bytes[UTF8_BOM.len()..].to_vec();
    }
    let config: OptimisticResumeConfigFile = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid optimistic Resume config {}", path.display()))?;
    if config.format_version != 1 {
        bail!(
            "unsupported optimistic Resume config version {}",
            config.format_version
        );
    }
    match config.mode.as_str() {
        "disabled" => {
            if !config.thread_ids.is_empty() {
                bail!("disabled optimistic Resume config must not list thread ids");
            }
            Ok(OptimisticResumeRuntimePolicy::Disabled)
        }
        "canary" => {
            if config.thread_ids.is_empty() {
                bail!("optimistic Resume canary config requires at least one thread id");
            }
            let mut thread_ids = BTreeSet::new();
            for thread_id in config.thread_ids {
                validate_thread_id(&thread_id)?;
                if !thread_ids.insert(thread_id.clone()) {
                    bail!("duplicate optimistic Resume canary thread id {thread_id}");
                }
            }
            Ok(OptimisticResumeRuntimePolicy::Canary(thread_ids))
        }
        "all_managed" => {
            if !config.thread_ids.is_empty() {
                bail!("all_managed optimistic Resume config must not list thread ids");
            }
            Ok(OptimisticResumeRuntimePolicy::AllManaged)
        }
        other => bail!("unsupported optimistic Resume mode {other:?}"),
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

#[cfg(test)]
mod tests {
    use std::fs;

    use anyhow::Result;
    use tempfile::TempDir;

    use super::OptimisticResumeRuntimePolicy;
    use super::load_optimistic_resume_policy_fail_open;
    use super::validate_optimistic_resume_policy;

    fn write_policy(root: &TempDir, bytes: &[u8]) -> Result<()> {
        let data = root.path().join("Data");
        fs::create_dir_all(&data)?;
        fs::write(data.join("optimistic-resume.json"), bytes)?;
        Ok(())
    }

    #[test]
    fn valid_bom_policy_remains_compatible() -> Result<()> {
        let root = TempDir::new()?;
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(br#"{"formatVersion":1,"mode":"all_managed","threadIds":[]}"#);
        write_policy(&root, &bytes)?;

        validate_optimistic_resume_policy(root.path())?;
        assert_eq!(
            load_optimistic_resume_policy_fail_open(root.path()),
            OptimisticResumeRuntimePolicy::AllManaged
        );
        Ok(())
    }

    #[test]
    fn empty_policy_fails_strict_validation_but_runtime_starts_disabled() -> Result<()> {
        let root = TempDir::new()?;
        write_policy(&root, b"")?;

        assert!(validate_optimistic_resume_policy(root.path()).is_err());
        assert_eq!(
            load_optimistic_resume_policy_fail_open(root.path()),
            OptimisticResumeRuntimePolicy::Disabled
        );
        Ok(())
    }

    #[test]
    fn truncated_policy_fails_strict_validation_but_runtime_starts_disabled() -> Result<()> {
        let root = TempDir::new()?;
        write_policy(&root, br#"{"formatVersion":1,"mode":"all_managed"#)?;

        assert!(validate_optimistic_resume_policy(root.path()).is_err());
        assert_eq!(
            load_optimistic_resume_policy_fail_open(root.path()),
            OptimisticResumeRuntimePolicy::Disabled
        );
        Ok(())
    }

    #[test]
    fn semantically_invalid_policy_fails_open() -> Result<()> {
        let root = TempDir::new()?;
        write_policy(
            &root,
            br#"{"formatVersion":1,"mode":"all_managed","threadIds":["unexpected"]}"#,
        )?;

        assert!(validate_optimistic_resume_policy(root.path()).is_err());
        assert_eq!(
            load_optimistic_resume_policy_fail_open(root.path()),
            OptimisticResumeRuntimePolicy::Disabled
        );
        Ok(())
    }
}
