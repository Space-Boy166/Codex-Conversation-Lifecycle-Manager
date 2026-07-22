use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;

pub(crate) fn remove_dir_all_scoped(
    target: &Path,
    allowed_root: &Path,
    purpose: &str,
) -> Result<()> {
    if !target.exists() {
        return Ok(());
    }
    if !target.is_dir() {
        bail!("{purpose} target is not a directory: {}", target.display());
    }
    let target = std::fs::canonicalize(target)
        .with_context(|| format!("failed to resolve {purpose} target {}", target.display()))?;
    let allowed_root = std::fs::canonicalize(allowed_root).with_context(|| {
        format!(
            "failed to resolve {purpose} allowed root {}",
            allowed_root.display()
        )
    })?;
    validate_recursive_delete_target(&target, &allowed_root, purpose, &runtime_safety_roots()?)?;
    std::fs::remove_dir_all(&target)
        .with_context(|| format!("failed to perform {purpose} at {}", target.display()))
}

#[derive(Debug)]
struct SafetyRoots {
    user_profile: Option<PathBuf>,
    temp_root: Option<PathBuf>,
    protected_roots: Vec<PathBuf>,
    protected_subtrees: Vec<PathBuf>,
}

fn runtime_safety_roots() -> Result<SafetyRoots> {
    let user_profile = environment_root("USERPROFILE");
    let temp_root = canonical_existing(std::env::temp_dir());
    let mut protected_roots = Vec::new();
    for name in ["USERPROFILE", "APPDATA", "LOCALAPPDATA", "TEMP", "TMP"] {
        if let Some(path) = environment_root(name) {
            push_unique(&mut protected_roots, path);
        }
    }
    if let Some(path) = environment_root("CODEX_HOME") {
        push_unique(&mut protected_roots, path);
    } else if let Some(profile) = &user_profile
        && let Some(path) = canonical_existing(profile.join(".codex"))
    {
        push_unique(&mut protected_roots, path);
    }
    let mut protected_subtrees = Vec::new();
    for name in [
        "SystemRoot",
        "ProgramData",
        "ProgramFiles",
        "ProgramFiles(x86)",
    ] {
        if let Some(path) = environment_root(name) {
            push_unique(&mut protected_subtrees, path);
        }
    }
    Ok(SafetyRoots {
        user_profile,
        temp_root,
        protected_roots,
        protected_subtrees,
    })
}

fn validate_recursive_delete_target(
    target: &Path,
    allowed_root: &Path,
    purpose: &str,
    roots: &SafetyRoots,
) -> Result<()> {
    if target == allowed_root || !target.starts_with(allowed_root) {
        bail!(
            "refusing {purpose}: target must be a strict child of {} but was {}",
            allowed_root.display(),
            target.display()
        );
    }
    if target.parent().is_none() || target.parent() == Some(target) {
        bail!("refusing {purpose}: filesystem roots cannot be removed");
    }
    for protected in &roots.protected_roots {
        if target == protected || protected.starts_with(target) {
            bail!(
                "refusing {purpose}: target {} is or contains protected root {}",
                target.display(),
                protected.display()
            );
        }
    }
    for protected in &roots.protected_subtrees {
        if target.starts_with(protected) {
            bail!(
                "refusing {purpose}: target {} is inside protected system subtree {}",
                target.display(),
                protected.display()
            );
        }
    }
    if let Some(profile) = &roots.user_profile
        && target.starts_with(profile)
    {
        let temp_root = roots.temp_root.as_deref().context(
            "refusing recursive deletion inside the user profile because TEMP is unavailable",
        )?;
        if !is_strict_child(target, temp_root) {
            bail!(
                "refusing {purpose}: recursive deletion inside the user profile is forbidden outside TEMP: {}",
                target.display()
            );
        }
        let scoped_temp = if allowed_root == temp_root {
            target.parent() == Some(temp_root)
                && target
                    .file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|value| value.starts_with("clm-"))
        } else {
            is_strict_child(allowed_root, temp_root)
        };
        if !scoped_temp {
            bail!("refusing {purpose}: TEMP deletion is not confined to a CLM-owned scope");
        }
    }
    Ok(())
}

fn is_strict_child(path: &Path, root: &Path) -> bool {
    path != root && path.starts_with(root)
}

fn environment_root(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).and_then(|value| canonical_existing(PathBuf::from(value)))
}

fn canonical_existing(path: PathBuf) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn scoped_removal_deletes_only_the_strict_child() -> Result<()> {
        let temp = tempdir()?;
        let scope = temp.path().join("scope");
        let child = scope.join("child");
        std::fs::create_dir_all(&child)?;
        std::fs::write(child.join("proof.txt"), b"proof")?;

        remove_dir_all_scoped(&child, &scope, "test cleanup")?;

        assert!(scope.is_dir());
        assert!(!child.exists());
        Ok(())
    }

    #[test]
    fn guard_rejects_scope_root_and_sibling_prefixes() -> Result<()> {
        let temp = tempdir()?;
        let scope = std::fs::canonicalize(temp.path())?;
        let sibling = temp.path().with_extension("sibling");
        std::fs::create_dir_all(&sibling)?;
        let sibling = std::fs::canonicalize(&sibling)?;
        let roots = SafetyRoots {
            user_profile: None,
            temp_root: None,
            protected_roots: Vec::new(),
            protected_subtrees: Vec::new(),
        };

        assert!(validate_recursive_delete_target(&scope, &scope, "test", &roots).is_err());
        assert!(validate_recursive_delete_target(&sibling, &scope, "test", &roots).is_err());
        std::fs::remove_dir(&sibling)?;
        Ok(())
    }

    #[test]
    fn guard_rejects_codex_and_profile_subtrees() -> Result<()> {
        let temp = tempdir()?;
        let profile = temp.path().join("profile");
        let codex_home = profile.join(".codex");
        let temp_root = profile.join("AppData").join("Local").join("Temp");
        std::fs::create_dir_all(&codex_home)?;
        std::fs::create_dir_all(&temp_root)?;
        let profile = std::fs::canonicalize(profile)?;
        let codex_home = std::fs::canonicalize(codex_home)?;
        let temp_root = std::fs::canonicalize(temp_root)?;
        let roots = SafetyRoots {
            user_profile: Some(profile.clone()),
            temp_root: Some(temp_root),
            protected_roots: vec![profile.clone(), codex_home.clone()],
            protected_subtrees: Vec::new(),
        };

        assert!(validate_recursive_delete_target(&codex_home, &profile, "test", &roots).is_err());
        Ok(())
    }
}
