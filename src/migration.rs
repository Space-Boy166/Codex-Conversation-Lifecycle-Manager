use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

use crate::ActiveCandidateReport;
use crate::ApiProjectionReport;
use crate::CodexOracle;
use crate::IndexedRollout;
use crate::compact_images::COMPACT_IMAGE_POLICY;
use crate::compact_images::apply_compact_image_externalization;
use crate::compact_images::has_supported_inline_compact_images;
use crate::compact_images::prepare_compact_image_externalization;
use crate::read_rollout_thread_id;
use crate::sha256_file;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationManifest {
    pub format_version: u32,
    pub prepared_at_unix_ms: u128,
    pub thread_id: String,
    pub original_path: String,
    pub archive_path: String,
    pub candidate_path: String,
    pub rollback_path: String,
    pub index_path: String,
    pub source_bytes: u64,
    pub candidate_bytes: u64,
    pub source_sha256: String,
    pub candidate_sha256: String,
    pub oracle_version: String,
    pub full_turns: u64,
    pub active_tail_turns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_image_policy: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationApplyReport {
    pub thread_id: String,
    pub active_path: String,
    pub rollback_path: String,
    pub active_sha256: String,
    pub active_bytes: u64,
    pub state: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationRehydrateReport {
    pub thread_id: String,
    pub active_path: String,
    pub displaced_candidate_path: String,
    pub disabled_index_path: String,
    pub receipt_path: String,
    pub restored_sha256: String,
    pub restored_bytes: u64,
    pub appended_bytes: u64,
    pub state: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct MigrationRehydrationReceipt {
    format_version: u32,
    rehydrated_at_unix_ms: u128,
    thread_id: String,
    rollout_path: String,
    displaced_candidate_path: String,
    disabled_index_path: String,
    restored_sha256: String,
    restored_bytes: u64,
    appended_bytes: u64,
    state: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationReactivationReport {
    pub thread_id: String,
    pub active_path: String,
    pub active_sha256: String,
    pub active_bytes_before: u64,
    pub active_bytes_after: u64,
    pub active_bytes_reclaimed: u64,
    pub manifest_path: String,
    pub rollback_path: String,
    pub retired_manifest_path: String,
    pub retired_rollback_path: Option<String>,
    pub retired_receipt_path: String,
    pub retired_candidate_path: String,
    pub retired_index_path: String,
    pub state: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationRefreshReport {
    pub thread_id: String,
    pub active_path: String,
    pub active_sha256: String,
    pub active_bytes_before: u64,
    pub active_bytes_after: u64,
    pub active_bytes_reclaimed: u64,
    pub rehydrated_sha256: String,
    pub rehydrated_bytes: u64,
    pub appended_bytes: u64,
    pub manifest_path: String,
    pub rollback_path: String,
    pub previous_manifest_path: String,
    pub previous_rollback_path: Option<String>,
    pub previous_rollback_was_present: bool,
    pub previous_active_path: String,
    pub previous_index_path: String,
    pub state: String,
}

#[derive(Debug)]
struct RefreshManifestPaths {
    original: PathBuf,
    rollback: PathBuf,
    index: PathBuf,
    archive: PathBuf,
    vault: PathBuf,
}

pub fn prepare_migration(
    rollout: &Path,
    backend: PathBuf,
    runtime_root: PathBuf,
    fixture_mode: bool,
) -> Result<(PathBuf, MigrationManifest)> {
    prepare_migration_with_policy(rollout, backend, runtime_root, fixture_mode, None)
}

fn prepare_migration_with_policy(
    rollout: &Path,
    backend: PathBuf,
    runtime_root: PathBuf,
    fixture_mode: bool,
    compact_image_policy: Option<String>,
) -> Result<(PathBuf, MigrationManifest)> {
    if !fixture_mode {
        ensure_codex_closed()?;
    }
    let original = std::fs::canonicalize(rollout)
        .with_context(|| format!("failed to resolve {}", rollout.display()))?;
    let thread_id = read_rollout_thread_id(&original)?;
    let source_bytes = std::fs::metadata(&original)?.len();
    let source_sha256 = sha256_file(&original)?;
    let vault = runtime_root
        .join("Data")
        .join("Vault")
        .join("Codex")
        .join(&thread_id);
    let manifest_path = vault.join("manifest.json");
    if manifest_path.exists() {
        bail!("manifest already exists: {}", manifest_path.display());
    }
    let segments = vault.join("segments");
    std::fs::create_dir_all(&segments)?;
    let archive = segments.join(format!("rollout-full-{}.jsonl", &source_sha256[..16]));
    copy_verified(&original, &archive, &source_sha256)?;
    let source_after_copy = sha256_file(&original)?;
    if source_after_copy != source_sha256 {
        bail!("source rollout changed while its archive copy was being prepared");
    }

    let index_root = runtime_root.join("Data").join("Indexes");
    std::fs::create_dir_all(&index_root)?;
    let final_index = index_root.join(format!("{thread_id}.sqlite"));
    let staging_index = index_root.join(format!("{thread_id}.sqlite.clm-new"));
    if staging_index.exists() {
        bail!("staging index already exists: {}", staging_index.display());
    }
    let oracle = CodexOracle::new(backend, runtime_root.clone());
    let full_projection = oracle.project(&archive)?;
    if full_projection.thread_id != thread_id {
        bail!("full archive oracle returned the wrong thread id");
    }
    let mut index = IndexedRollout::open(&staging_index)?;
    let api_report = index.replace_api_projection(
        &archive,
        &thread_id,
        &source_sha256,
        &full_projection.oracle_version,
        &full_projection.turns,
    )?;
    index.sync_rollout(&archive)?;

    let candidate = sidecar_path(&original, "clm-new")?;
    if candidate.exists() {
        bail!("candidate already exists: {}", candidate.display());
    }
    let candidate_report = build_active_candidate(&index, &candidate)?;
    if candidate_report.source_sha256 != source_sha256 {
        bail!("candidate builder observed a different source hash");
    }
    let active_projection = oracle.project(&candidate)?;
    if active_projection.thread_id != thread_id {
        bail!("active candidate oracle returned the wrong thread id");
    }
    let final_projection = index.replace_active_tail(&thread_id, &active_projection.turns)?;
    verify_projection_counts(&api_report, &final_projection)?;
    drop(index);

    install_staging_index(&staging_index, &final_index)?;
    let rollback = sidecar_path(&original, "clm-rollback")?;
    let manifest = MigrationManifest {
        format_version: 1,
        prepared_at_unix_ms: SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
        thread_id,
        original_path: original.to_string_lossy().into_owned(),
        archive_path: archive.to_string_lossy().into_owned(),
        candidate_path: candidate.to_string_lossy().into_owned(),
        rollback_path: rollback.to_string_lossy().into_owned(),
        index_path: final_index.to_string_lossy().into_owned(),
        source_bytes,
        candidate_bytes: candidate_report.candidate_bytes,
        source_sha256,
        candidate_sha256: candidate_report.candidate_sha256,
        oracle_version: full_projection.oracle_version,
        full_turns: final_projection.turns_total,
        active_tail_turns: final_projection.active_tail_turns,
        compact_image_policy,
    };
    write_new_json(&manifest_path, &manifest)?;
    Ok((manifest_path, manifest))
}

pub fn apply_migration(manifest_path: &Path, fixture_mode: bool) -> Result<MigrationApplyReport> {
    if !fixture_mode {
        ensure_codex_closed()?;
    }
    let manifest: MigrationManifest = serde_json::from_reader(File::open(manifest_path)?)
        .context("invalid migration manifest")?;
    let original = PathBuf::from(&manifest.original_path);
    let archive = PathBuf::from(&manifest.archive_path);
    let candidate = PathBuf::from(&manifest.candidate_path);
    let rollback = PathBuf::from(&manifest.rollback_path);
    let index = PathBuf::from(&manifest.index_path);
    verify_file(
        &original,
        manifest.source_bytes,
        &manifest.source_sha256,
        "source",
    )?;
    verify_file(
        &archive,
        manifest.source_bytes,
        &manifest.source_sha256,
        "archive",
    )?;
    verify_file(
        &candidate,
        manifest.candidate_bytes,
        &manifest.candidate_sha256,
        "candidate",
    )?;
    if rollback.exists() {
        bail!("rollback path already exists: {}", rollback.display());
    }
    let indexed = IndexedRollout::open(&index)?;
    if !indexed.has_api_projection(&manifest.thread_id)? {
        bail!(
            "installed index is not ready for thread {}",
            manifest.thread_id
        );
    }
    drop(indexed);

    std::fs::rename(&original, &rollback).with_context(|| {
        format!(
            "failed to move original {} to rollback {}",
            original.display(),
            rollback.display()
        )
    })?;
    if let Err(error) = std::fs::rename(&candidate, &original) {
        let restore = std::fs::rename(&rollback, &original);
        return match restore {
            Ok(()) => Err(error).context("candidate activation failed; original was restored"),
            Err(restore_error) => Err(anyhow::anyhow!(
                "candidate activation failed ({error}); EMERGENCY: rollback restore also failed ({restore_error})"
            )),
        };
    }
    let active_hash = sha256_file(&original)?;
    if active_hash != manifest.candidate_sha256 {
        bail!("activated candidate hash mismatch; rollback is preserved");
    }
    Ok(MigrationApplyReport {
        thread_id: manifest.thread_id,
        active_path: original.to_string_lossy().into_owned(),
        rollback_path: rollback.to_string_lossy().into_owned(),
        active_sha256: active_hash,
        active_bytes: std::fs::metadata(&original)?.len(),
        state: "applied_with_rollback_retained".to_string(),
    })
}

pub fn rollback_migration(
    manifest_path: &Path,
    fixture_mode: bool,
) -> Result<MigrationApplyReport> {
    if !fixture_mode {
        ensure_codex_closed()?;
    }
    let manifest: MigrationManifest = serde_json::from_reader(File::open(manifest_path)?)
        .context("invalid migration manifest")?;
    let original = PathBuf::from(&manifest.original_path);
    let rollback = PathBuf::from(&manifest.rollback_path);
    verify_file(
        &rollback,
        manifest.source_bytes,
        &manifest.source_sha256,
        "rollback",
    )?;
    let displaced = sidecar_path(&original, "clm-displaced")?;
    if displaced.exists() {
        bail!(
            "displaced candidate path already exists: {}",
            displaced.display()
        );
    }
    std::fs::rename(&original, &displaced)?;
    if let Err(error) = std::fs::rename(&rollback, &original) {
        let restore = std::fs::rename(&displaced, &original);
        return match restore {
            Ok(()) => Err(error).context("rollback activation failed; candidate was restored"),
            Err(restore_error) => Err(anyhow::anyhow!(
                "rollback activation failed ({error}); EMERGENCY: candidate restore also failed ({restore_error})"
            )),
        };
    }
    let active_hash = sha256_file(&original)?;
    if active_hash != manifest.source_sha256 {
        bail!("restored rollout hash mismatch; displaced candidate is preserved");
    }
    Ok(MigrationApplyReport {
        thread_id: manifest.thread_id,
        active_path: original.to_string_lossy().into_owned(),
        rollback_path: displaced.to_string_lossy().into_owned(),
        active_sha256: active_hash,
        active_bytes: std::fs::metadata(&original)?.len(),
        state: "rolled_back_with_candidate_retained".to_string(),
    })
}

/// Restores the byte-exact pre-activation history and preserves records appended
/// after activation. Unlike `rollback_migration`, this is safe after the managed
/// conversation has continued receiving new turns.
pub fn rehydrate_migration(
    manifest_path: &Path,
    fixture_mode: bool,
) -> Result<MigrationRehydrateReport> {
    if !fixture_mode {
        ensure_codex_closed()?;
    }
    let manifest: MigrationManifest = serde_json::from_reader(File::open(manifest_path)?)
        .context("invalid migration manifest")?;
    let original = PathBuf::from(&manifest.original_path);
    rehydrate_rollout_at_path(
        manifest_path,
        manifest,
        original,
        "rehydrated_with_managed_candidate_retained",
    )
}

pub fn rehydrate_archived_migration(
    manifest_path: &Path,
    archived_rollout: &Path,
    fixture_mode: bool,
) -> Result<MigrationRehydrateReport> {
    if !fixture_mode {
        ensure_codex_closed()?;
    }
    let manifest: MigrationManifest = serde_json::from_reader(File::open(manifest_path)?)
        .context("invalid migration manifest")?;
    validate_archived_rollout_path(&manifest, archived_rollout)?;
    rehydrate_rollout_at_path(
        manifest_path,
        manifest,
        archived_rollout.to_path_buf(),
        "archived_rehydrated_native_with_managed_candidate_retained",
    )
}

fn validate_archived_rollout_path(
    manifest: &MigrationManifest,
    archived_rollout: &Path,
) -> Result<()> {
    if !archived_rollout.is_file() {
        bail!("missing archived rollout {}", archived_rollout.display());
    }
    let original = Path::new(&manifest.original_path);
    if original.exists() {
        bail!(
            "active rollout still exists; refusing archived rehydration: {}",
            original.display()
        );
    }
    let original_name = original
        .file_name()
        .context("managed original rollout has no filename")?;
    if archived_rollout.file_name() != Some(original_name) {
        bail!(
            "archived rollout filename does not match the managed original: {}",
            archived_rollout.display()
        );
    }
    let sessions_root = original
        .ancestors()
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("sessions"))
        })
        .context("managed original rollout is not below a sessions directory")?;
    let codex_home = sessions_root
        .parent()
        .context("managed sessions directory has no Codex home")?;
    let expected_archive_root = std::fs::canonicalize(codex_home.join("archived_sessions"))
        .context("failed to resolve the expected archived_sessions directory")?;
    let actual_archive_root = std::fs::canonicalize(
        archived_rollout
            .parent()
            .context("archived rollout has no parent directory")?,
    )
    .context("failed to resolve the archived rollout directory")?;
    if actual_archive_root != expected_archive_root {
        bail!(
            "archived rollout is outside the managed Codex archived_sessions directory: {}",
            archived_rollout.display()
        );
    }
    if read_rollout_thread_id(archived_rollout)? != manifest.thread_id {
        bail!("archived rollout thread id does not match the activation manifest");
    }
    Ok(())
}

fn rehydrate_rollout_at_path(
    manifest_path: &Path,
    manifest: MigrationManifest,
    rollout: PathBuf,
    state: &str,
) -> Result<MigrationRehydrateReport> {
    let archive = PathBuf::from(&manifest.archive_path);
    let rollback = PathBuf::from(&manifest.rollback_path);
    let index = PathBuf::from(&manifest.index_path);

    verify_file(
        &archive,
        manifest.source_bytes,
        &manifest.source_sha256,
        "archive",
    )?;
    if rollback.exists() {
        verify_file(
            &rollback,
            manifest.source_bytes,
            &manifest.source_sha256,
            "rollback",
        )?;
    }

    let active_bytes = std::fs::metadata(&rollout)
        .with_context(|| format!("missing managed rollout {}", rollout.display()))?
        .len();
    if active_bytes < manifest.candidate_bytes {
        bail!(
            "active rollout is shorter than its activation prefix: expected at least {}, got {active_bytes}",
            manifest.candidate_bytes
        );
    }
    let active_prefix_sha256 = sha256_prefix(&rollout, manifest.candidate_bytes)?;
    if active_prefix_sha256 != manifest.candidate_sha256 {
        bail!("active rollout prefix no longer matches the activated candidate");
    }
    if read_rollout_thread_id(&rollout)? != manifest.thread_id {
        bail!("managed rollout thread id does not match the activation manifest");
    }

    let appended_bytes = active_bytes - manifest.candidate_bytes;
    validate_complete_jsonl_suffix(&rollout, manifest.candidate_bytes, appended_bytes)?;
    let rehydrated = sidecar_path(&rollout, "clm-rehydrated")?;
    if rehydrated.exists() {
        bail!(
            "rehydrated candidate path already exists: {}",
            rehydrated.display()
        );
    }

    build_rehydrated_rollout(
        &archive,
        &rollout,
        manifest.candidate_bytes,
        appended_bytes,
        &rehydrated,
    )?;
    let expected_bytes = manifest.source_bytes + appended_bytes;
    let actual_bytes = std::fs::metadata(&rehydrated)?.len();
    if actual_bytes != expected_bytes {
        bail!("rehydrated rollout length mismatch: expected {expected_bytes}, got {actual_bytes}");
    }
    if read_rollout_thread_id(&rehydrated)? != manifest.thread_id {
        bail!("rehydrated rollout thread id does not match the activation manifest");
    }

    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let displaced = rollout.with_file_name(format!(
        "{}.clm-displaced-{stamp}",
        rollout
            .file_name()
            .and_then(|value| value.to_str())
            .context("rollout filename is not valid UTF-8")?
    ));
    let disabled_index = index.with_extension(format!("sqlite.clm-disabled-{stamp}"));
    if displaced.exists() || disabled_index.exists() {
        bail!("rehydration transaction output already exists");
    }

    if index.exists() {
        std::fs::rename(&index, &disabled_index)
            .with_context(|| format!("failed to disable managed index {}", index.display()))?;
    } else {
        bail!("managed index is missing: {}", index.display());
    }

    if let Err(error) = std::fs::rename(&rollout, &displaced) {
        let _ = std::fs::rename(&disabled_index, &index);
        return Err(error).context("failed to displace the managed rollout");
    }
    if let Err(error) = std::fs::rename(&rehydrated, &rollout) {
        let active_restore = std::fs::rename(&displaced, &rollout);
        let index_restore = std::fs::rename(&disabled_index, &index);
        return match (active_restore, index_restore) {
            (Ok(()), Ok(())) => {
                Err(error).context("rehydration activation failed; managed state was restored")
            }
            (active, index_result) => Err(anyhow::anyhow!(
                "rehydration activation failed ({error}); EMERGENCY restore results: active={active:?}, index={index_result:?}"
            )),
        };
    }

    let restored_sha256 = sha256_file(&rollout)?;
    let receipt_path = manifest_path
        .parent()
        .context("migration manifest has no vault directory")?
        .join(format!("rehydration-{stamp}.json"));
    let receipt = MigrationRehydrationReceipt {
        format_version: 1,
        rehydrated_at_unix_ms: stamp,
        thread_id: manifest.thread_id.clone(),
        rollout_path: rollout.to_string_lossy().into_owned(),
        displaced_candidate_path: displaced.to_string_lossy().into_owned(),
        disabled_index_path: disabled_index.to_string_lossy().into_owned(),
        restored_sha256: restored_sha256.clone(),
        restored_bytes: expected_bytes,
        appended_bytes,
        state: state.to_string(),
    };
    if let Err(error) = write_new_json(&receipt_path, &receipt) {
        let failed_rehydrated =
            sidecar_path(&rollout, &format!("clm-receipt-failed-{stamp}-rehydrated"))?;
        let full_preserve = std::fs::rename(&rollout, &failed_rehydrated);
        let active_restore = std::fs::rename(&displaced, &rollout);
        let index_restore = std::fs::rename(&disabled_index, &index);
        return match (full_preserve, active_restore, index_restore) {
            (Ok(()), Ok(()), Ok(())) => Err(error)
                .context("failed to record rehydration receipt; managed state was restored"),
            (full, active, index_result) => Err(anyhow::anyhow!(
                "failed to record rehydration receipt ({error}); EMERGENCY recovery results: full={full:?}, active={active:?}, index={index_result:?}"
            )),
        };
    }
    Ok(MigrationRehydrateReport {
        thread_id: manifest.thread_id,
        active_path: rollout.to_string_lossy().into_owned(),
        displaced_candidate_path: displaced.to_string_lossy().into_owned(),
        disabled_index_path: disabled_index.to_string_lossy().into_owned(),
        receipt_path: receipt_path.to_string_lossy().into_owned(),
        restored_sha256,
        restored_bytes: expected_bytes,
        appended_bytes,
        state: state.to_string(),
    })
}

/// Rebuilds an already managed conversation around its newest native checkpoint.
///
/// The operation is deliberately offline and generation preserving. It first
/// reconstructs the complete current rollout, rotates the previous manifest and
/// rollback into timestamped evidence paths, prepares a new indexed generation,
/// and activates it only when the new candidate is smaller than the active file
/// it replaces. Any failure after rehydration restores the previous managed
/// active rollout and index while retaining the failed generation for diagnosis.
pub fn refresh_migration(
    manifest_path: &Path,
    backend: PathBuf,
    runtime_root: PathBuf,
    fixture_mode: bool,
) -> Result<MigrationRefreshReport> {
    refresh_migration_with_policy_upgrade(manifest_path, backend, runtime_root, fixture_mode, false)
}

/// Rebuilds a managed generation and explicitly enables Compact-image
/// externalization in the same offline, rollback-preserving transaction.
pub fn upgrade_compact_image_policy(
    manifest_path: &Path,
    backend: PathBuf,
    runtime_root: PathBuf,
    fixture_mode: bool,
) -> Result<MigrationRefreshReport> {
    refresh_migration_with_policy_upgrade(manifest_path, backend, runtime_root, fixture_mode, true)
}

fn refresh_migration_with_policy_upgrade(
    manifest_path: &Path,
    backend: PathBuf,
    runtime_root: PathBuf,
    fixture_mode: bool,
    enable_compact_image_policy: bool,
) -> Result<MigrationRefreshReport> {
    if !fixture_mode {
        ensure_codex_closed()?;
    }

    let manifest_path = std::fs::canonicalize(manifest_path)
        .with_context(|| format!("failed to resolve {}", manifest_path.display()))?;
    let previous_manifest: MigrationManifest = serde_json::from_reader(File::open(&manifest_path)?)
        .context("invalid migration manifest")?;
    let previous_policy_enabled = match previous_manifest.compact_image_policy.as_deref() {
        None => false,
        Some(COMPACT_IMAGE_POLICY) => true,
        Some(policy) => bail!("unsupported Compact-image policy in managed manifest: {policy}"),
    };
    if enable_compact_image_policy && previous_policy_enabled {
        bail!("Compact-image policy is already enabled for this managed generation");
    }
    let target_compact_image_policy = if enable_compact_image_policy {
        Some(COMPACT_IMAGE_POLICY.to_string())
    } else {
        previous_manifest.compact_image_policy.clone()
    };
    let preserve_compact_image_policy = target_compact_image_policy.is_some();
    let expected_manifest = runtime_root
        .join("Data")
        .join("Vault")
        .join("Codex")
        .join(&previous_manifest.thread_id)
        .join("manifest.json");
    let expected_manifest = std::fs::canonicalize(&expected_manifest).with_context(|| {
        format!(
            "canonical managed manifest is missing: {}",
            expected_manifest.display()
        )
    })?;
    if manifest_path != expected_manifest {
        bail!(
            "refresh requires the canonical managed manifest: expected {}, got {}",
            expected_manifest.display(),
            manifest_path.display()
        );
    }

    let paths = validate_refresh_manifest_paths(&manifest_path, &previous_manifest, &runtime_root)?;
    let original = paths.original;
    let rollback = paths.rollback;
    let index = paths.index;
    verify_file(
        &paths.archive,
        previous_manifest.source_bytes,
        &previous_manifest.source_sha256,
        "previous immutable archive",
    )?;
    let previous_rollback_was_present = rollback.is_file();
    if previous_rollback_was_present {
        verify_file(
            &rollback,
            previous_manifest.source_bytes,
            &previous_manifest.source_sha256,
            "previous same-volume rollback",
        )?;
    } else if rollback.exists() {
        bail!(
            "previous rollback path exists but is not a file: {}",
            rollback.display()
        );
    }
    if !index.is_file() {
        bail!("refresh requires the managed index: {}", index.display());
    }
    let active_bytes_before = std::fs::metadata(&original)
        .with_context(|| format!("missing active rollout {}", original.display()))?
        .len();
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let vault = paths.vault;
    let vault_name = vault
        .file_name()
        .and_then(|value| value.to_str())
        .context("managed vault name is not valid UTF-8")?;
    let cycle_vault = vault.with_file_name(format!("{vault_name}.clm-cycle-{stamp}"));
    let cycle_rollback = sidecar_path(&rollback, &format!("clm-cycle-{stamp}"))?;
    if cycle_vault.exists() || cycle_rollback.exists() {
        bail!("refresh generation paths already exist");
    }

    let rehydrated = rehydrate_migration(&manifest_path, true)?;
    let previous_active = PathBuf::from(&rehydrated.displaced_candidate_path);
    let previous_index = PathBuf::from(&rehydrated.disabled_index_path);

    if let Err(error) = std::fs::rename(&vault, &cycle_vault) {
        let recovery = restore_rehydrated_managed_state(
            &original,
            &previous_active,
            &index,
            &previous_index,
            stamp,
        );
        return match recovery {
            Ok(_) => {
                Err(error).context("failed to rotate the previous vault; managed state restored")
            }
            Err(recovery_error) => Err(anyhow::anyhow!(
                "failed to rotate the previous vault ({error}); EMERGENCY recovery also failed ({recovery_error})"
            )),
        };
    }
    if previous_rollback_was_present && let Err(error) = std::fs::rename(&rollback, &cycle_rollback)
    {
        let vault_restore = std::fs::rename(&cycle_vault, &vault);
        let state_restore = restore_rehydrated_managed_state(
            &original,
            &previous_active,
            &index,
            &previous_index,
            stamp,
        );
        return match (vault_restore, state_restore) {
            (Ok(()), Ok(_)) => {
                Err(error).context("failed to rotate the previous rollback; managed state restored")
            }
            (vault_result, state_result) => Err(anyhow::anyhow!(
                "failed to rotate the previous rollback ({error}); EMERGENCY recovery results: vault={vault_result:?}, managed_state={state_result:?}"
            )),
        };
    }

    let refresh_attempt = (|| -> Result<MigrationRefreshReport> {
        let (new_manifest_path, mut new_manifest) = prepare_migration_with_policy(
            &original,
            backend.clone(),
            runtime_root.clone(),
            true,
            target_compact_image_policy.clone(),
        )?;
        if new_manifest.thread_id != previous_manifest.thread_id {
            bail!("refreshed manifest changed the thread id");
        }
        if new_manifest.source_bytes != rehydrated.restored_bytes
            || new_manifest.source_sha256 != rehydrated.restored_sha256
        {
            bail!("refreshed manifest does not describe the rehydrated source exactly");
        }
        let candidate_has_compact_images = preserve_compact_image_policy
            && has_supported_inline_compact_images(Path::new(&new_manifest.candidate_path))?;
        if enable_compact_image_policy && !candidate_has_compact_images {
            bail!(
                "Compact-image policy upgrade found no supported inline image in the rebuilt candidate"
            );
        }
        if new_manifest.candidate_bytes >= active_bytes_before && !candidate_has_compact_images {
            bail!(
                "new active candidate would not reduce resume cost: current {active_bytes_before} bytes, candidate {} bytes",
                new_manifest.candidate_bytes
            );
        }

        let mut applied = apply_migration(&new_manifest_path, true)?;
        if candidate_has_compact_images {
            let (plan_path, _) = prepare_compact_image_externalization(
                &new_manifest_path,
                Some(backend.clone()),
                runtime_root.clone(),
                true,
            )?;
            let image_report = apply_compact_image_externalization(&plan_path, true)?;
            new_manifest = serde_json::from_reader(File::open(&new_manifest_path)?)
                .context("invalid externalized migration manifest")?;
            applied.active_path = image_report.active_path;
            applied.active_sha256 = new_manifest.candidate_sha256.clone();
            applied.active_bytes = new_manifest.candidate_bytes;
            applied.state = image_report.state;
        }
        if new_manifest.candidate_bytes >= active_bytes_before {
            bail!(
                "refreshed active candidate would not reduce resume cost after image policy: current {active_bytes_before} bytes, candidate {} bytes",
                new_manifest.candidate_bytes
            );
        }
        verify_file(
            Path::new(&new_manifest.rollback_path),
            new_manifest.source_bytes,
            &new_manifest.source_sha256,
            "refreshed rollback",
        )?;
        verify_file(
            Path::new(&new_manifest.archive_path),
            new_manifest.source_bytes,
            &new_manifest.source_sha256,
            "refreshed archive",
        )?;
        verify_file(
            &original,
            new_manifest.candidate_bytes,
            &new_manifest.candidate_sha256,
            "refreshed active rollout",
        )?;
        if !cycle_vault.join("manifest.json").is_file()
            || (previous_rollback_was_present && !cycle_rollback.is_file())
            || !previous_active.is_file()
            || !previous_index.is_file()
        {
            bail!("previous managed generation is not fully retained");
        }

        Ok(MigrationRefreshReport {
            thread_id: new_manifest.thread_id,
            active_path: applied.active_path,
            active_sha256: applied.active_sha256,
            active_bytes_before,
            active_bytes_after: applied.active_bytes,
            active_bytes_reclaimed: active_bytes_before - applied.active_bytes,
            rehydrated_sha256: rehydrated.restored_sha256,
            rehydrated_bytes: rehydrated.restored_bytes,
            appended_bytes: rehydrated.appended_bytes,
            manifest_path: new_manifest_path.to_string_lossy().into_owned(),
            rollback_path: applied.rollback_path,
            previous_manifest_path: cycle_vault
                .join("manifest.json")
                .to_string_lossy()
                .into_owned(),
            previous_rollback_path: previous_rollback_was_present
                .then(|| cycle_rollback.to_string_lossy().into_owned()),
            previous_rollback_was_present,
            previous_active_path: previous_active.to_string_lossy().into_owned(),
            previous_index_path: previous_index.to_string_lossy().into_owned(),
            state: if candidate_has_compact_images {
                if enable_compact_image_policy {
                    "compact_image_policy_upgraded_with_previous_generation_retained".to_string()
                } else {
                    "refreshed_with_compact_images_externalized_and_previous_generation_retained"
                        .to_string()
                }
            } else {
                "refreshed_with_previous_generation_retained".to_string()
            },
        })
    })();

    match refresh_attempt {
        Ok(report) => Ok(report),
        Err(error) => {
            let recovery = restore_previous_refresh_generation(
                &original,
                &previous_active,
                &index,
                &previous_index,
                &vault,
                &cycle_vault,
                &rollback,
                previous_rollback_was_present.then_some(cycle_rollback.as_path()),
                &runtime_root,
                &previous_manifest.thread_id,
                stamp,
            );
            match recovery {
                Ok(_) => Err(error).context("refresh failed; previous managed state was restored"),
                Err(recovery_error) => Err(anyhow::anyhow!(
                    "refresh failed ({error}); EMERGENCY previous-state recovery also failed ({recovery_error})"
                )),
            }
        }
    }
}

/// Starts a new managed generation after a previously managed Archive task was
/// restored to native full history and later unarchived by Codex.
///
/// The retired Archive generation, its rehydration receipt, compact candidate,
/// disabled index, and same-volume rollback are retained. The active full
/// rollout becomes the byte authority for a new generation only after its
/// rehydrated prefix and complete appended suffix have been verified.
pub fn reactivate_unarchived_migration(
    manifest_path: &Path,
    backend: PathBuf,
    runtime_root: PathBuf,
    fixture_mode: bool,
) -> Result<MigrationReactivationReport> {
    if !fixture_mode {
        ensure_codex_closed()?;
    }

    let manifest_path = std::fs::canonicalize(manifest_path)
        .with_context(|| format!("failed to resolve {}", manifest_path.display()))?;
    let previous_manifest: MigrationManifest = serde_json::from_reader(File::open(&manifest_path)?)
        .context("invalid migration manifest")?;
    let preserve_compact_image_policy = match previous_manifest.compact_image_policy.as_deref() {
        None => false,
        Some(COMPACT_IMAGE_POLICY) => true,
        Some(policy) => bail!("unsupported Compact-image policy in managed manifest: {policy}"),
    };
    let expected_manifest = runtime_root
        .join("Data")
        .join("Vault")
        .join("Codex")
        .join(&previous_manifest.thread_id)
        .join("manifest.json");
    let expected_manifest = std::fs::canonicalize(&expected_manifest).with_context(|| {
        format!(
            "canonical managed manifest is missing: {}",
            expected_manifest.display()
        )
    })?;
    if manifest_path != expected_manifest {
        bail!(
            "unarchived reactivation requires the canonical managed manifest: expected {}, got {}",
            expected_manifest.display(),
            manifest_path.display()
        );
    }

    let vault = manifest_path
        .parent()
        .context("canonical managed manifest has no vault directory")?
        .to_path_buf();
    let original = std::fs::canonicalize(&previous_manifest.original_path).with_context(|| {
        format!(
            "unarchived native rollout is missing: {}",
            previous_manifest.original_path
        )
    })?;
    if read_rollout_thread_id(&original)? != previous_manifest.thread_id {
        bail!("unarchived native rollout thread id does not match its retired manifest");
    }
    let index = PathBuf::from(&previous_manifest.index_path);
    let expected_index = runtime_root
        .join("Data")
        .join("Indexes")
        .join(format!("{}.sqlite", previous_manifest.thread_id));
    if canonical_missing_target(&index)? != canonical_missing_target(&expected_index)? {
        bail!("retired manifest index path is outside the canonical runtime index root");
    }
    if index.exists() {
        bail!(
            "unarchived reactivation requires a disabled canonical index: {}",
            index.display()
        );
    }
    let candidate = PathBuf::from(&previous_manifest.candidate_path);
    if candidate.exists() {
        bail!(
            "retired candidate staging path still exists and requires inspection: {}",
            candidate.display()
        );
    }
    verify_file(
        Path::new(&previous_manifest.archive_path),
        previous_manifest.source_bytes,
        &previous_manifest.source_sha256,
        "retired immutable archive",
    )?;

    let (receipt_path, receipt) = latest_rehydration_receipt(&vault)?;
    if receipt.thread_id != previous_manifest.thread_id {
        bail!("latest rehydration receipt belongs to a different thread");
    }
    if receipt.state != "archived_rehydrated_native_with_managed_candidate_retained" {
        bail!(
            "latest rehydration receipt is not an Archive de-layering receipt: {}",
            receipt.state
        );
    }
    validate_rehydration_receipt_paths(&previous_manifest, &receipt, &index)?;
    let active_bytes_before = std::fs::metadata(&original)?.len();
    if active_bytes_before < receipt.restored_bytes {
        bail!("unarchived rollout is shorter than its verified Archive restoration");
    }
    if sha256_prefix(&original, receipt.restored_bytes)? != receipt.restored_sha256 {
        bail!("unarchived rollout no longer starts with its verified Archive restoration");
    }
    validate_complete_jsonl_suffix(
        &original,
        receipt.restored_bytes,
        active_bytes_before - receipt.restored_bytes,
    )?;
    let active_sha256 = sha256_file(&original)?;

    let retired_candidate = PathBuf::from(&receipt.displaced_candidate_path);
    let retired_index = PathBuf::from(&receipt.disabled_index_path);
    if !retired_candidate.is_file() || !retired_index.is_file() {
        bail!("retired Archive generation is missing its compact candidate or disabled index");
    }
    if std::fs::metadata(&retired_candidate)?.len() < previous_manifest.candidate_bytes
        || sha256_prefix(&retired_candidate, previous_manifest.candidate_bytes)?
            != previous_manifest.candidate_sha256
    {
        bail!("retired Archive compact candidate no longer matches its manifest");
    }
    let retired_index_db = IndexedRollout::open(&retired_index)?;
    if !retired_index_db.has_api_projection(&previous_manifest.thread_id)? {
        bail!("retired Archive index has no API projection for its thread");
    }
    drop(retired_index_db);

    let rollback = PathBuf::from(&previous_manifest.rollback_path);
    let previous_rollback_was_present = rollback.is_file();
    if previous_rollback_was_present {
        verify_file(
            &rollback,
            previous_manifest.source_bytes,
            &previous_manifest.source_sha256,
            "retired Archive rollback",
        )?;
    } else if rollback.exists() {
        bail!(
            "retired rollback path exists but is not a file: {}",
            rollback.display()
        );
    }

    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let vault_name = vault
        .file_name()
        .and_then(|value| value.to_str())
        .context("managed vault name is not valid UTF-8")?;
    let retired_vault = vault.with_file_name(format!("{vault_name}.clm-archive-cycle-{stamp}"));
    let retired_rollback = sidecar_path(&rollback, &format!("clm-archive-cycle-{stamp}"))?;
    if retired_vault.exists() || retired_rollback.exists() {
        bail!("unarchived reactivation generation paths already exist");
    }

    std::fs::rename(&vault, &retired_vault)
        .context("failed to retire the previous Archive generation vault")?;
    if previous_rollback_was_present
        && let Err(error) = std::fs::rename(&rollback, &retired_rollback)
    {
        let vault_restore = std::fs::rename(&retired_vault, &vault);
        return match vault_restore {
            Ok(()) => Err(error).context("failed to retire the previous Archive rollback"),
            Err(restore_error) => Err(anyhow::anyhow!(
                "failed to retire the previous Archive rollback ({error}); EMERGENCY vault restore also failed ({restore_error})"
            )),
        };
    }

    let reactivation_attempt = (|| -> Result<MigrationReactivationReport> {
        let (new_manifest_path, mut new_manifest) = prepare_migration_with_policy(
            &original,
            backend.clone(),
            runtime_root.clone(),
            true,
            previous_manifest.compact_image_policy.clone(),
        )?;
        if new_manifest.thread_id != previous_manifest.thread_id {
            bail!("reactivated manifest changed the thread id");
        }
        if new_manifest.source_bytes != active_bytes_before
            || new_manifest.source_sha256 != active_sha256
        {
            bail!("reactivated manifest does not describe the verified full active rollout");
        }
        let candidate_has_compact_images = preserve_compact_image_policy
            && has_supported_inline_compact_images(Path::new(&new_manifest.candidate_path))?;
        if new_manifest.candidate_bytes >= active_bytes_before && !candidate_has_compact_images {
            bail!(
                "reactivated candidate would not reduce resume cost: current {active_bytes_before} bytes, candidate {} bytes",
                new_manifest.candidate_bytes
            );
        }

        let mut applied = apply_migration(&new_manifest_path, true)?;
        if candidate_has_compact_images {
            let (plan_path, _) = prepare_compact_image_externalization(
                &new_manifest_path,
                Some(backend.clone()),
                runtime_root.clone(),
                true,
            )?;
            let image_report = apply_compact_image_externalization(&plan_path, true)?;
            new_manifest = serde_json::from_reader(File::open(&new_manifest_path)?)
                .context("invalid externalized reactivation manifest")?;
            applied.active_path = image_report.active_path;
            applied.active_sha256 = new_manifest.candidate_sha256.clone();
            applied.active_bytes = new_manifest.candidate_bytes;
            applied.state = image_report.state;
        }
        if new_manifest.candidate_bytes >= active_bytes_before {
            bail!(
                "reactivated candidate would not reduce resume cost after image policy: current {active_bytes_before} bytes, candidate {} bytes",
                new_manifest.candidate_bytes
            );
        }
        verify_file(
            Path::new(&new_manifest.rollback_path),
            new_manifest.source_bytes,
            &new_manifest.source_sha256,
            "reactivated rollback",
        )?;
        verify_file(
            Path::new(&new_manifest.archive_path),
            new_manifest.source_bytes,
            &new_manifest.source_sha256,
            "reactivated immutable archive",
        )?;
        verify_file(
            &original,
            new_manifest.candidate_bytes,
            &new_manifest.candidate_sha256,
            "reactivated active rollout",
        )?;

        let retired_receipt = retired_vault.join(
            receipt_path
                .file_name()
                .context("rehydration receipt has no filename")?,
        );
        if !retired_vault.join("manifest.json").is_file()
            || !retired_receipt.is_file()
            || !retired_candidate.is_file()
            || !retired_index.is_file()
            || (previous_rollback_was_present && !retired_rollback.is_file())
        {
            bail!("retired Archive generation is not fully retained");
        }

        Ok(MigrationReactivationReport {
            thread_id: new_manifest.thread_id,
            active_path: applied.active_path,
            active_sha256: applied.active_sha256,
            active_bytes_before,
            active_bytes_after: applied.active_bytes,
            active_bytes_reclaimed: active_bytes_before - applied.active_bytes,
            manifest_path: new_manifest_path.to_string_lossy().into_owned(),
            rollback_path: applied.rollback_path,
            retired_manifest_path: retired_vault
                .join("manifest.json")
                .to_string_lossy()
                .into_owned(),
            retired_rollback_path: previous_rollback_was_present
                .then(|| retired_rollback.to_string_lossy().into_owned()),
            retired_receipt_path: retired_receipt.to_string_lossy().into_owned(),
            retired_candidate_path: retired_candidate.to_string_lossy().into_owned(),
            retired_index_path: retired_index.to_string_lossy().into_owned(),
            state: "unarchived_native_history_reactivated_with_archive_generation_retained"
                .to_string(),
        })
    })();

    match reactivation_attempt {
        Ok(report) => Ok(report),
        Err(error) => {
            let recovery = restore_previous_unarchived_generation(
                &original,
                active_bytes_before,
                &active_sha256,
                &candidate,
                &index,
                &vault,
                &retired_vault,
                &rollback,
                previous_rollback_was_present.then_some(retired_rollback.as_path()),
                &previous_manifest.thread_id,
                stamp,
            );
            match recovery {
                Ok(_) => Err(error)
                    .context("unarchived reactivation failed; native full history was restored"),
                Err(recovery_error) => Err(anyhow::anyhow!(
                    "unarchived reactivation failed ({error}); EMERGENCY previous-state recovery also failed ({recovery_error})"
                )),
            }
        }
    }
}

fn canonical_missing_target(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .context("target path has no parent directory")?;
    let name = path.file_name().context("target path has no filename")?;
    Ok(std::fs::canonicalize(parent)
        .with_context(|| format!("failed to resolve target parent {}", parent.display()))?
        .join(name))
}

fn latest_rehydration_receipt(vault: &Path) -> Result<(PathBuf, MigrationRehydrationReceipt)> {
    let mut latest: Option<(PathBuf, MigrationRehydrationReceipt)> = None;
    for entry in std::fs::read_dir(vault)
        .with_context(|| format!("failed to inspect managed vault {}", vault.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if !entry.file_type()?.is_file()
            || !name.starts_with("rehydration-")
            || !name.ends_with(".json")
        {
            continue;
        }
        let receipt: MigrationRehydrationReceipt = serde_json::from_reader(File::open(&path)?)
            .with_context(|| format!("invalid rehydration receipt {}", path.display()))?;
        if receipt.format_version != 1 {
            bail!(
                "unsupported rehydration receipt format in {}",
                path.display()
            );
        }
        if latest.as_ref().is_none_or(|(_, current)| {
            receipt.rehydrated_at_unix_ms > current.rehydrated_at_unix_ms
        }) {
            latest = Some((path, receipt));
        }
    }
    latest.context("managed vault has no durable rehydration receipt")
}

fn validate_rehydration_receipt_paths(
    manifest: &MigrationManifest,
    receipt: &MigrationRehydrationReceipt,
    canonical_index: &Path,
) -> Result<()> {
    let original = Path::new(&manifest.original_path);
    let original_name = original
        .file_name()
        .and_then(|value| value.to_str())
        .context("managed original rollout filename is not valid UTF-8")?;
    let sessions_root = original
        .ancestors()
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("sessions"))
        })
        .context("managed original rollout is not below a sessions directory")?;
    let codex_home = sessions_root
        .parent()
        .context("managed sessions directory has no Codex home")?;
    let archived_root = std::fs::canonicalize(codex_home.join("archived_sessions"))
        .context("failed to resolve the receipt archived_sessions root")?;
    let expected_archived = archived_root.join(original_name);
    if canonical_missing_target(Path::new(&receipt.rollout_path))?
        != canonical_missing_target(&expected_archived)?
    {
        bail!("Archive rehydration receipt rollout path is outside the expected archived task");
    }

    let retired_candidate = Path::new(&receipt.displaced_candidate_path);
    let retired_candidate_parent = std::fs::canonicalize(
        retired_candidate
            .parent()
            .context("retired Archive candidate has no parent directory")?,
    )?;
    let retired_candidate_name = retired_candidate
        .file_name()
        .and_then(|value| value.to_str())
        .context("retired Archive candidate filename is not valid UTF-8")?;
    if retired_candidate_parent != archived_root
        || !retired_candidate_name.starts_with(&format!("{original_name}.clm-displaced-"))
    {
        bail!("Archive rehydration receipt candidate path is outside its archived task");
    }

    let retired_index = Path::new(&receipt.disabled_index_path);
    let retired_index_parent = std::fs::canonicalize(
        retired_index
            .parent()
            .context("retired Archive index has no parent directory")?,
    )?;
    let canonical_index_parent = std::fs::canonicalize(
        canonical_index
            .parent()
            .context("canonical managed index has no parent directory")?,
    )?;
    let canonical_index_name = canonical_index
        .file_name()
        .and_then(|value| value.to_str())
        .context("canonical managed index filename is not valid UTF-8")?;
    let retired_index_name = retired_index
        .file_name()
        .and_then(|value| value.to_str())
        .context("retired Archive index filename is not valid UTF-8")?;
    if retired_index_parent != canonical_index_parent
        || !retired_index_name.starts_with(&format!("{canonical_index_name}.clm-disabled-"))
    {
        bail!("Archive rehydration receipt index path is outside its managed index root");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn restore_previous_unarchived_generation(
    original: &Path,
    expected_bytes: u64,
    expected_sha256: &str,
    candidate: &Path,
    index: &Path,
    vault: &Path,
    retired_vault: &Path,
    rollback: &Path,
    retired_rollback: Option<&Path>,
    thread_id: &str,
    stamp: u128,
) -> Result<Vec<PathBuf>> {
    let failed_vault = vault.with_file_name(format!("{thread_id}.clm-reactivation-failed-{stamp}"));
    let failed_candidate = sidecar_path(
        original,
        &format!("clm-reactivation-failed-{stamp}-candidate"),
    )?;
    let failed_active = sidecar_path(original, &format!("clm-reactivation-failed-{stamp}-active"))?;
    let failed_index = index.with_extension(format!("sqlite.clm-reactivation-failed-{stamp}"));
    let mut preserved = Vec::new();

    if rollback.is_file() {
        verify_file(
            rollback,
            expected_bytes,
            expected_sha256,
            "reactivation recovery source",
        )?;
        if move_if_exists(original, &failed_active)? {
            preserved.push(failed_active);
        }
        std::fs::rename(rollback, original)
            .context("failed to restore the full native rollout after reactivation failure")?;
    }
    verify_file(
        original,
        expected_bytes,
        expected_sha256,
        "restored full native rollout",
    )?;
    if move_if_exists(candidate, &failed_candidate)? {
        preserved.push(failed_candidate);
    }
    if move_if_exists(index, &failed_index)? {
        preserved.push(failed_index);
    }
    if move_if_exists(vault, &failed_vault)? {
        preserved.push(failed_vault);
    }
    std::fs::rename(retired_vault, vault)
        .context("failed to restore the retired Archive generation vault")?;
    if let Some(retired_rollback) = retired_rollback {
        if rollback.exists() {
            bail!(
                "reactivation recovery rollback path is unexpectedly occupied: {}",
                rollback.display()
            );
        }
        std::fs::rename(retired_rollback, rollback)
            .context("failed to restore the retired Archive rollback")?;
    } else if rollback.exists() {
        bail!(
            "reactivation recovery created an unexpected rollback: {}",
            rollback.display()
        );
    }
    Ok(preserved)
}

fn validate_refresh_manifest_paths(
    manifest_path: &Path,
    manifest: &MigrationManifest,
    runtime_root: &Path,
) -> Result<RefreshManifestPaths> {
    let vault = manifest_path
        .parent()
        .context("managed manifest has no vault directory")?
        .to_path_buf();
    let original = std::fs::canonicalize(&manifest.original_path).with_context(|| {
        format!(
            "failed to resolve managed active rollout {}",
            manifest.original_path
        )
    })?;
    if read_rollout_thread_id(&original)? != manifest.thread_id {
        bail!("managed active rollout thread id does not match its manifest");
    }

    let expected_candidate = sidecar_path(&original, "clm-new")?;
    let expected_image_candidate = sidecar_path(&original, "clm-images-new")?;
    let candidate = PathBuf::from(&manifest.candidate_path);
    let candidate_is_allowed = candidate == expected_candidate
        || (manifest.compact_image_policy.as_deref() == Some(COMPACT_IMAGE_POLICY)
            && candidate == expected_image_candidate);
    if !candidate_is_allowed {
        bail!(
            "managed candidate path escaped its accepted active-rollout sidecars: expected {}{}; got {}",
            expected_candidate.display(),
            if manifest.compact_image_policy.as_deref() == Some(COMPACT_IMAGE_POLICY) {
                format!(" or {}", expected_image_candidate.display())
            } else {
                String::new()
            },
            candidate.display()
        );
    }
    let expected_rollback = sidecar_path(&original, "clm-rollback")?;
    let rollback = PathBuf::from(&manifest.rollback_path);
    if rollback != expected_rollback {
        bail!(
            "managed rollback path escaped its active-rollout sidecar: expected {}, got {}",
            expected_rollback.display(),
            rollback.display()
        );
    }

    let archive = std::fs::canonicalize(&manifest.archive_path).with_context(|| {
        format!(
            "failed to resolve managed archive {}",
            manifest.archive_path
        )
    })?;
    if archive == vault || !archive.starts_with(&vault) {
        bail!(
            "managed archive must be a strict child of its thread vault: {}",
            archive.display()
        );
    }

    let index = std::fs::canonicalize(&manifest.index_path)
        .with_context(|| format!("failed to resolve managed index {}", manifest.index_path))?;
    let index_root = std::fs::canonicalize(runtime_root.join("Data").join("Indexes"))
        .context("failed to resolve the managed index root")?;
    if index == index_root || !index.starts_with(&index_root) {
        bail!(
            "managed index must be a strict child of the runtime index root: {}",
            index.display()
        );
    }

    Ok(RefreshManifestPaths {
        original,
        rollback,
        index,
        archive,
        vault,
    })
}

pub fn create_native_checkpoint_offline(
    rollout: &Path,
    backend: PathBuf,
    runtime_root: PathBuf,
    codex_home: PathBuf,
    force: bool,
) -> Result<crate::NativeCompactionReport> {
    ensure_codex_closed()?;
    CodexOracle::new(backend, runtime_root).compact_with_native_backend(rollout, &codex_home, force)
}

pub fn build_active_candidate(
    index: &IndexedRollout,
    output_path: &Path,
) -> Result<ActiveCandidateReport> {
    let slice = index.resume_slice()?;
    if slice.full_scan_required || slice.checkpoint_offset == 0 {
        bail!("rollout has no native compacted item with replacement_history");
    }
    let source_path = Path::new(&slice.source_path);
    let source_bytes = std::fs::metadata(source_path)?.len();
    if source_bytes != slice.indexed_end_offset {
        bail!(
            "rollout changed after indexing: indexed {}, current {source_bytes}",
            slice.indexed_end_offset
        );
    }
    if output_path.exists() {
        bail!("candidate already exists: {}", output_path.display());
    }
    let thread_id = read_rollout_thread_id(source_path)?;
    let source_sha256 = sha256_file(source_path)?;
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let source = File::open(source_path)?;
    let mut reader = BufReader::new(source);
    let mut session_meta = Vec::new();
    let session_meta_bytes = reader.read_until(b'\n', &mut session_meta)?;
    if session_meta_bytes == 0 || session_meta.last() != Some(&b'\n') {
        bail!("rollout session_meta is missing or incomplete");
    }
    let session_value: Value = serde_json::from_slice(trim_line_ending(&session_meta))?;
    if session_value.get("type").and_then(Value::as_str) != Some("session_meta") {
        bail!("first rollout record is not session_meta");
    }
    if slice.checkpoint_offset < session_meta_bytes as u64 {
        bail!("checkpoint overlaps session_meta");
    }

    reader.seek(SeekFrom::Start(slice.checkpoint_offset))?;
    let suffix_bytes = slice
        .indexed_end_offset
        .checked_sub(slice.checkpoint_offset)
        .context("checkpoint lies beyond indexed end")?;
    let candidate = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output_path)
        .with_context(|| format!("failed to create {}", output_path.display()))?;
    let mut writer = BufWriter::new(candidate);
    writer.write_all(&session_meta)?;
    let copied = std::io::copy(&mut reader.take(suffix_bytes), &mut writer)?;
    if copied != suffix_bytes {
        bail!("candidate copied {copied} suffix bytes, expected {suffix_bytes}");
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);

    let candidate_bytes = std::fs::metadata(output_path)?.len();
    let expected_bytes = session_meta_bytes as u64 + suffix_bytes;
    if candidate_bytes != expected_bytes {
        bail!("candidate length mismatch: wrote {candidate_bytes}, expected {expected_bytes}");
    }
    let candidate_sha256 = sha256_file(output_path)?;
    Ok(ActiveCandidateReport {
        thread_id,
        source_path: source_path.to_string_lossy().into_owned(),
        candidate_path: output_path.to_string_lossy().into_owned(),
        source_bytes,
        candidate_bytes,
        checkpoint_offset: slice.checkpoint_offset,
        source_sha256,
        candidate_sha256,
    })
}

fn sha256_prefix(path: &Path, length: u64) -> Result<String> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file).take(length);
    let mut hasher = Sha256::new();
    let copied = std::io::copy(&mut reader, &mut hasher)?;
    if copied != length {
        bail!("file ended while hashing prefix: expected {length} bytes, read {copied}");
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn validate_complete_jsonl_suffix(path: &Path, start: u64, length: u64) -> Result<()> {
    if length == 0 {
        return Ok(());
    }
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file.take(length));
    let mut consumed = 0_u64;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        consumed += read as u64;
        if line.last() != Some(&b'\n') {
            bail!("active rollout has an incomplete appended JSONL record");
        }
        serde_json::from_slice::<Value>(trim_line_ending(&line))
            .context("active rollout has an invalid appended JSONL record")?;
    }
    if consumed != length {
        bail!("appended suffix length changed during validation");
    }
    Ok(())
}

fn build_rehydrated_rollout(
    archive: &Path,
    active: &Path,
    active_prefix_bytes: u64,
    appended_bytes: u64,
    output: &Path,
) -> Result<()> {
    let mut archive_reader = File::open(archive)?;
    let mut active_reader = File::open(active)?;
    active_reader.seek(SeekFrom::Start(active_prefix_bytes))?;
    let output_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .with_context(|| format!("failed to create {}", output.display()))?;
    let mut writer = BufWriter::new(output_file);
    std::io::copy(&mut archive_reader, &mut writer)?;
    let copied = std::io::copy(&mut active_reader.take(appended_bytes), &mut writer)?;
    if copied != appended_bytes {
        bail!(
            "active rollout changed while rehydrating: expected {appended_bytes} appended bytes, copied {copied}"
        );
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}

fn trim_line_ending(mut line: &[u8]) -> &[u8] {
    if let Some(stripped) = line.strip_suffix(b"\n") {
        line = stripped;
    }
    if let Some(stripped) = line.strip_suffix(b"\r") {
        line = stripped;
    }
    line
}

fn copy_verified(source: &Path, destination: &Path, expected_hash: &str) -> Result<()> {
    if destination.exists() {
        if sha256_file(destination)? == expected_hash {
            return Ok(());
        }
        bail!(
            "immutable archive path contains different bytes: {}",
            destination.display()
        );
    }
    let partial = destination.with_extension("jsonl.partial");
    if partial.exists() {
        bail!("partial archive already exists: {}", partial.display());
    }
    let mut input = File::open(source)?;
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)?;
    let mut output = BufWriter::new(output);
    std::io::copy(&mut input, &mut output)?;
    output.flush()?;
    output.get_ref().sync_all()?;
    drop(output);
    if sha256_file(&partial)? != expected_hash {
        bail!("archive copy hash mismatch: {}", partial.display());
    }
    std::fs::rename(&partial, destination)?;
    Ok(())
}

fn verify_file(path: &Path, expected_bytes: u64, expected_hash: &str, label: &str) -> Result<()> {
    let actual_bytes = std::fs::metadata(path)
        .with_context(|| format!("missing {label} file {}", path.display()))?
        .len();
    if actual_bytes != expected_bytes {
        bail!("{label} length mismatch: expected {expected_bytes}, got {actual_bytes}");
    }
    let actual_hash = sha256_file(path)?;
    if actual_hash != expected_hash {
        bail!("{label} SHA-256 mismatch");
    }
    Ok(())
}

fn sidecar_path(path: &Path, suffix: &str) -> Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("rollout filename is not valid UTF-8")?;
    Ok(path.with_file_name(format!("{name}.{suffix}")))
}

fn move_if_exists(source: &Path, destination: &Path) -> Result<bool> {
    if !source.exists() {
        return Ok(false);
    }
    if destination.exists() {
        bail!(
            "refusing to overwrite preserved refresh artifact: {}",
            destination.display()
        );
    }
    std::fs::rename(source, destination).with_context(|| {
        format!(
            "failed to preserve {} as {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(true)
}

fn restore_rehydrated_managed_state(
    original: &Path,
    previous_active: &Path,
    index: &Path,
    previous_index: &Path,
    stamp: u128,
) -> Result<Vec<PathBuf>> {
    let failed_active = sidecar_path(original, &format!("clm-refresh-failed-{stamp}-rehydrated"))?;
    let failed_index = index.with_extension(format!("sqlite.clm-refresh-failed-{stamp}"));
    let mut preserved = Vec::new();
    if move_if_exists(original, &failed_active)? {
        preserved.push(failed_active);
    }
    if move_if_exists(index, &failed_index)? {
        preserved.push(failed_index);
    }
    std::fs::rename(previous_active, original)
        .context("failed to restore the previous managed active rollout")?;
    std::fs::rename(previous_index, index)
        .context("failed to restore the previous managed index")?;
    Ok(preserved)
}

#[allow(clippy::too_many_arguments)]
fn restore_previous_refresh_generation(
    original: &Path,
    previous_active: &Path,
    index: &Path,
    previous_index: &Path,
    vault: &Path,
    cycle_vault: &Path,
    rollback: &Path,
    cycle_rollback: Option<&Path>,
    runtime_root: &Path,
    thread_id: &str,
    stamp: u128,
) -> Result<Vec<PathBuf>> {
    let failed_vault = vault.with_file_name(format!("{thread_id}.clm-refresh-failed-{stamp}"));
    let failed_rollback =
        sidecar_path(rollback, &format!("clm-refresh-failed-{stamp}-rehydrated"))?;
    let candidate = sidecar_path(original, "clm-new")?;
    let failed_candidate =
        sidecar_path(original, &format!("clm-refresh-failed-{stamp}-candidate"))?;
    let staging_index = runtime_root
        .join("Data")
        .join("Indexes")
        .join(format!("{thread_id}.sqlite.clm-new"));
    let failed_staging_index = runtime_root.join("Data").join("Indexes").join(format!(
        "{thread_id}.sqlite.clm-refresh-failed-{stamp}-staging"
    ));

    let mut preserved = Vec::new();
    if move_if_exists(vault, &failed_vault)? {
        preserved.push(failed_vault);
    }
    if move_if_exists(rollback, &failed_rollback)? {
        preserved.push(failed_rollback);
    }
    if move_if_exists(&candidate, &failed_candidate)? {
        preserved.push(failed_candidate);
    }
    if move_if_exists(&staging_index, &failed_staging_index)? {
        preserved.push(failed_staging_index);
    }
    preserved.extend(restore_rehydrated_managed_state(
        original,
        previous_active,
        index,
        previous_index,
        stamp,
    )?);
    std::fs::rename(cycle_vault, vault).context("failed to restore the previous managed vault")?;
    if let Some(cycle_rollback) = cycle_rollback {
        std::fs::rename(cycle_rollback, rollback)
            .context("failed to restore the previous same-volume rollback")?;
    } else if rollback.exists() {
        bail!(
            "failed refresh created an unexpected rollback that could not be preserved: {}",
            rollback.display()
        );
    }
    Ok(preserved)
}

fn install_staging_index(staging: &Path, final_path: &Path) -> Result<()> {
    let previous = if final_path.exists() {
        let previous = final_path.with_extension(format!(
            "sqlite.previous-{}",
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
        ));
        std::fs::rename(final_path, &previous)?;
        Some(previous)
    } else {
        None
    };
    if let Err(error) = std::fs::rename(staging, final_path) {
        if let Some(previous) = previous {
            let restore = std::fs::rename(previous, final_path);
            return match restore {
                Ok(()) => {
                    Err(error).context("index activation failed; previous index was restored")
                }
                Err(restore_error) => Err(anyhow::anyhow!(
                    "index activation failed ({error}); EMERGENCY: previous index restore also failed ({restore_error})"
                )),
            };
        }
        return Err(error).context("index activation failed");
    }
    Ok(())
}

fn verify_projection_counts(
    full: &ApiProjectionReport,
    final_projection: &ApiProjectionReport,
) -> Result<()> {
    if final_projection.active_tail_turns == 0 {
        bail!("candidate oracle returned no active tail turns");
    }
    let minimum_expected = full
        .turns_total
        .saturating_sub(final_projection.active_tail_turns);
    if final_projection.turns_total < minimum_expected {
        bail!(
            "active-tail merge lost too many turns: full {}, final {}, active tail {}",
            full.turns_total,
            final_projection.turns_total,
            final_projection.active_tail_turns
        );
    }
    Ok(())
}

fn write_new_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if path.exists() {
        bail!("manifest already exists: {}", path.display());
    }
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}

pub fn ensure_codex_closed() -> Result<()> {
    let output = Command::new("tasklist")
        .args(["/FO", "CSV", "/NH"])
        .output()
        .context("failed to inspect running processes")?;
    if !output.status.success() {
        bail!("tasklist failed while checking Codex ownership");
    }
    let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    let blockers = [
        "\"chatgpt.exe\"",
        "\"codex.exe\"",
        "\"codex-clm-proxy.exe\"",
    ];
    let running: Vec<_> = blockers
        .iter()
        .filter(|name| text.contains(**name))
        .map(|name| name.trim_matches('"'))
        .collect();
    if !running.is_empty() {
        bail!(
            "Codex owners are still running ({}); refusing offline mutation",
            running.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn refresh_manifest_fixture(
        root: &Path,
        thread_id: &str,
    ) -> Result<(PathBuf, PathBuf, MigrationManifest)> {
        let runtime_root = root.join("runtime");
        let vault = runtime_root
            .join("Data")
            .join("Vault")
            .join("Codex")
            .join(thread_id);
        let archive = vault.join("segments").join("rollout-full.jsonl");
        let index = runtime_root
            .join("Data")
            .join("Indexes")
            .join(format!("{thread_id}.sqlite"));
        let original = root.join("sessions").join("rollout.jsonl");
        for parent in [archive.parent(), index.parent(), original.parent()]
            .into_iter()
            .flatten()
        {
            std::fs::create_dir_all(parent)?;
        }
        let session_meta =
            format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{thread_id}\"}}}}\n");
        std::fs::write(&original, session_meta.as_bytes())?;
        std::fs::write(&archive, session_meta.as_bytes())?;
        std::fs::write(&index, b"index")?;
        let original = std::fs::canonicalize(original)?;
        let manifest_path = vault.join("manifest.json");
        let manifest = MigrationManifest {
            format_version: 1,
            prepared_at_unix_ms: 1,
            thread_id: thread_id.to_string(),
            original_path: original.to_string_lossy().into_owned(),
            archive_path: archive.to_string_lossy().into_owned(),
            candidate_path: sidecar_path(&original, "clm-new")?
                .to_string_lossy()
                .into_owned(),
            rollback_path: sidecar_path(&original, "clm-rollback")?
                .to_string_lossy()
                .into_owned(),
            index_path: index.to_string_lossy().into_owned(),
            source_bytes: session_meta.len() as u64,
            candidate_bytes: session_meta.len() as u64,
            source_sha256: sha256_file(&archive)?,
            candidate_sha256: sha256_file(&original)?,
            oracle_version: "fixture".to_string(),
            full_turns: 0,
            active_tail_turns: 0,
            compact_image_policy: None,
        };
        std::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
        Ok((runtime_root, manifest_path, manifest))
    }

    #[test]
    fn rehydration_recovery_restores_previous_active_and_index() -> Result<()> {
        let temp = tempdir()?;
        let original = temp.path().join("rollout.jsonl");
        let previous_active = temp.path().join("rollout.jsonl.clm-displaced");
        let index = temp.path().join("thread.sqlite");
        let previous_index = temp.path().join("thread.sqlite.clm-disabled");
        std::fs::write(&original, b"rehydrated-full")?;
        std::fs::write(&previous_active, b"previous-active")?;
        std::fs::write(&previous_index, b"previous-index")?;

        let preserved = restore_rehydrated_managed_state(
            &original,
            &previous_active,
            &index,
            &previous_index,
            42,
        )?;

        assert_eq!(std::fs::read(&original)?, b"previous-active");
        assert_eq!(std::fs::read(&index)?, b"previous-index");
        assert_eq!(preserved.len(), 1);
        assert_eq!(std::fs::read(&preserved[0])?, b"rehydrated-full");
        Ok(())
    }

    #[test]
    fn refresh_manifest_paths_refuse_rollback_and_archive_escape() -> Result<()> {
        let temp = tempdir()?;
        let thread_id = "00000000-0000-7000-8000-000000000042";
        let (runtime_root, manifest_path, manifest) =
            refresh_manifest_fixture(temp.path(), thread_id)?;
        let manifest_path = std::fs::canonicalize(manifest_path)?;

        validate_refresh_manifest_paths(&manifest_path, &manifest, &runtime_root)?;

        let mut image_policy_manifest = manifest.clone();
        image_policy_manifest.compact_image_policy = Some(COMPACT_IMAGE_POLICY.to_string());
        image_policy_manifest.candidate_path = sidecar_path(
            Path::new(&image_policy_manifest.original_path),
            "clm-images-new",
        )?
        .to_string_lossy()
        .into_owned();
        validate_refresh_manifest_paths(&manifest_path, &image_policy_manifest, &runtime_root)?;

        let mut unapproved_image_candidate = image_policy_manifest.clone();
        unapproved_image_candidate.compact_image_policy = None;
        let error = validate_refresh_manifest_paths(
            &manifest_path,
            &unapproved_image_candidate,
            &runtime_root,
        )
        .expect_err("image candidate sidecar requires the exact Compact-image policy");
        assert!(
            error
                .to_string()
                .contains("accepted active-rollout sidecars")
        );

        let mut escaped_rollback = manifest.clone();
        escaped_rollback.rollback_path = temp
            .path()
            .join("unrelated-profile-file")
            .to_string_lossy()
            .into_owned();
        let error =
            validate_refresh_manifest_paths(&manifest_path, &escaped_rollback, &runtime_root)
                .expect_err("rollback path escape must be refused");
        assert!(error.to_string().contains("rollback path escaped"));

        let outside_archive = temp.path().join("outside-archive.jsonl");
        std::fs::write(&outside_archive, b"outside")?;
        let mut escaped_archive = manifest;
        escaped_archive.archive_path = outside_archive.to_string_lossy().into_owned();
        let error =
            validate_refresh_manifest_paths(&manifest_path, &escaped_archive, &runtime_root)
                .expect_err("archive path escape must be refused");
        assert!(error.to_string().contains("strict child"));
        Ok(())
    }

    #[test]
    fn failed_refresh_restores_every_previous_generation_owner() -> Result<()> {
        let temp = tempdir()?;
        let runtime_root = temp.path().join("runtime");
        let thread_id = "thread-refresh-test";
        let original = temp.path().join("rollout.jsonl");
        let previous_active = temp.path().join("rollout.jsonl.clm-displaced");
        let index = runtime_root
            .join("Data")
            .join("Indexes")
            .join(format!("{thread_id}.sqlite"));
        let previous_index = index.with_extension("sqlite.clm-disabled");
        let vault = runtime_root
            .join("Data")
            .join("Vault")
            .join("Codex")
            .join(thread_id);
        let cycle_vault = vault.with_file_name(format!("{thread_id}.clm-cycle-42"));
        let rollback = sidecar_path(&original, "clm-rollback")?;
        let cycle_rollback = sidecar_path(&rollback, "clm-cycle-42")?;
        let candidate = sidecar_path(&original, "clm-new")?;
        let staging_index = runtime_root
            .join("Data")
            .join("Indexes")
            .join(format!("{thread_id}.sqlite.clm-new"));

        for parent in [
            original.parent(),
            index.parent(),
            vault.parent(),
            cycle_vault.parent(),
        ]
        .into_iter()
        .flatten()
        {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir_all(&vault)?;
        std::fs::create_dir_all(&cycle_vault)?;
        std::fs::write(vault.join("manifest.json"), b"failed-new-vault")?;
        std::fs::write(cycle_vault.join("manifest.json"), b"previous-vault")?;
        std::fs::write(&original, b"failed-new-active")?;
        std::fs::write(&previous_active, b"previous-active")?;
        std::fs::write(&index, b"failed-new-index")?;
        std::fs::write(&previous_index, b"previous-index")?;
        std::fs::write(&rollback, b"failed-rehydrated-rollback")?;
        std::fs::write(&cycle_rollback, b"previous-rollback")?;
        std::fs::write(&candidate, b"failed-candidate")?;
        std::fs::write(&staging_index, b"failed-staging-index")?;

        let preserved = restore_previous_refresh_generation(
            &original,
            &previous_active,
            &index,
            &previous_index,
            &vault,
            &cycle_vault,
            &rollback,
            Some(&cycle_rollback),
            &runtime_root,
            thread_id,
            42,
        )?;

        assert_eq!(std::fs::read(&original)?, b"previous-active");
        assert_eq!(std::fs::read(&index)?, b"previous-index");
        assert_eq!(
            std::fs::read(vault.join("manifest.json"))?,
            b"previous-vault"
        );
        assert_eq!(std::fs::read(&rollback)?, b"previous-rollback");
        assert!(!previous_active.exists());
        assert!(!previous_index.exists());
        assert!(!cycle_vault.exists());
        assert!(!cycle_rollback.exists());
        assert_eq!(preserved.len(), 6);
        assert!(preserved.iter().all(|path| path.exists()));
        Ok(())
    }

    #[test]
    fn failed_refresh_restores_a_generation_whose_old_rollback_was_already_missing() -> Result<()> {
        let temp = tempdir()?;
        let runtime_root = temp.path().join("runtime");
        let thread_id = "thread-refresh-missing-rollback";
        let original = temp.path().join("rollout.jsonl");
        let previous_active = temp.path().join("rollout.jsonl.clm-displaced");
        let index = runtime_root
            .join("Data")
            .join("Indexes")
            .join(format!("{thread_id}.sqlite"));
        let previous_index = index.with_extension("sqlite.clm-disabled");
        let vault = runtime_root
            .join("Data")
            .join("Vault")
            .join("Codex")
            .join(thread_id);
        let cycle_vault = vault.with_file_name(format!("{thread_id}.clm-cycle-42"));
        let rollback = sidecar_path(&original, "clm-rollback")?;

        for parent in [original.parent(), index.parent(), vault.parent()]
            .into_iter()
            .flatten()
        {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir_all(&vault)?;
        std::fs::create_dir_all(&cycle_vault)?;
        std::fs::write(vault.join("manifest.json"), b"failed-new-vault")?;
        std::fs::write(cycle_vault.join("manifest.json"), b"previous-vault")?;
        std::fs::write(&original, b"failed-new-active")?;
        std::fs::write(&previous_active, b"previous-active")?;
        std::fs::write(&index, b"failed-new-index")?;
        std::fs::write(&previous_index, b"previous-index")?;
        std::fs::write(&rollback, b"failed-new-rollback")?;

        let preserved = restore_previous_refresh_generation(
            &original,
            &previous_active,
            &index,
            &previous_index,
            &vault,
            &cycle_vault,
            &rollback,
            None,
            &runtime_root,
            thread_id,
            42,
        )?;

        assert_eq!(std::fs::read(&original)?, b"previous-active");
        assert_eq!(std::fs::read(&index)?, b"previous-index");
        assert_eq!(
            std::fs::read(vault.join("manifest.json"))?,
            b"previous-vault"
        );
        assert!(!rollback.exists());
        assert!(!previous_active.exists());
        assert!(!previous_index.exists());
        assert!(!cycle_vault.exists());
        assert_eq!(preserved.len(), 4);
        assert!(preserved.iter().all(|path| path.exists()));
        Ok(())
    }
}
