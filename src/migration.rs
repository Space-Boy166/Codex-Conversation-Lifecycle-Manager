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
    pub restored_sha256: String,
    pub restored_bytes: u64,
    pub appended_bytes: u64,
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
    pub previous_rollback_path: String,
    pub previous_active_path: String,
    pub previous_index_path: String,
    pub state: String,
}

pub fn prepare_migration(
    rollout: &Path,
    backend: PathBuf,
    runtime_root: PathBuf,
    fixture_mode: bool,
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

    let active_bytes = std::fs::metadata(&original)
        .with_context(|| format!("missing active rollout {}", original.display()))?
        .len();
    if active_bytes < manifest.candidate_bytes {
        bail!(
            "active rollout is shorter than its activation prefix: expected at least {}, got {active_bytes}",
            manifest.candidate_bytes
        );
    }
    let active_prefix_sha256 = sha256_prefix(&original, manifest.candidate_bytes)?;
    if active_prefix_sha256 != manifest.candidate_sha256 {
        bail!("active rollout prefix no longer matches the activated candidate");
    }
    if read_rollout_thread_id(&original)? != manifest.thread_id {
        bail!("active rollout thread id does not match the activation manifest");
    }

    let appended_bytes = active_bytes - manifest.candidate_bytes;
    validate_complete_jsonl_suffix(&original, manifest.candidate_bytes, appended_bytes)?;
    let rehydrated = sidecar_path(&original, "clm-rehydrated")?;
    if rehydrated.exists() {
        bail!(
            "rehydrated candidate path already exists: {}",
            rehydrated.display()
        );
    }

    build_rehydrated_rollout(
        &archive,
        &original,
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
    let displaced = original.with_file_name(format!(
        "{}.clm-displaced-{stamp}",
        original
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

    if let Err(error) = std::fs::rename(&original, &displaced) {
        let _ = std::fs::rename(&disabled_index, &index);
        return Err(error).context("failed to displace the managed active rollout");
    }
    if let Err(error) = std::fs::rename(&rehydrated, &original) {
        let active_restore = std::fs::rename(&displaced, &original);
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

    let restored_sha256 = sha256_file(&original)?;
    Ok(MigrationRehydrateReport {
        thread_id: manifest.thread_id,
        active_path: original.to_string_lossy().into_owned(),
        displaced_candidate_path: displaced.to_string_lossy().into_owned(),
        disabled_index_path: disabled_index.to_string_lossy().into_owned(),
        restored_sha256,
        restored_bytes: expected_bytes,
        appended_bytes,
        state: "rehydrated_with_managed_candidate_retained".to_string(),
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
    if !fixture_mode {
        ensure_codex_closed()?;
    }

    let manifest_path = std::fs::canonicalize(manifest_path)
        .with_context(|| format!("failed to resolve {}", manifest_path.display()))?;
    let previous_manifest: MigrationManifest = serde_json::from_reader(File::open(&manifest_path)?)
        .context("invalid migration manifest")?;
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

    let original = PathBuf::from(&previous_manifest.original_path);
    let rollback = PathBuf::from(&previous_manifest.rollback_path);
    let index = PathBuf::from(&previous_manifest.index_path);
    if !rollback.is_file() {
        bail!(
            "refresh requires the previous same-volume rollback: {}",
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
    let vault = manifest_path
        .parent()
        .context("managed manifest has no vault directory")?
        .to_path_buf();
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
    if let Err(error) = std::fs::rename(&rollback, &cycle_rollback) {
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
        let (new_manifest_path, new_manifest) =
            prepare_migration(&original, backend, runtime_root.clone(), true)?;
        if new_manifest.thread_id != previous_manifest.thread_id {
            bail!("refreshed manifest changed the thread id");
        }
        if new_manifest.source_bytes != rehydrated.restored_bytes
            || new_manifest.source_sha256 != rehydrated.restored_sha256
        {
            bail!("refreshed manifest does not describe the rehydrated source exactly");
        }
        if new_manifest.candidate_bytes >= active_bytes_before {
            bail!(
                "new active candidate would not reduce resume cost: current {active_bytes_before} bytes, candidate {} bytes",
                new_manifest.candidate_bytes
            );
        }

        let applied = apply_migration(&new_manifest_path, true)?;
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
            || !cycle_rollback.is_file()
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
            previous_rollback_path: cycle_rollback.to_string_lossy().into_owned(),
            previous_active_path: previous_active.to_string_lossy().into_owned(),
            previous_index_path: previous_index.to_string_lossy().into_owned(),
            state: "refreshed_with_previous_generation_retained".to_string(),
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
                &cycle_rollback,
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
    cycle_rollback: &Path,
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
    std::fs::rename(cycle_rollback, rollback)
        .context("failed to restore the previous same-volume rollback")?;
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
            &cycle_rollback,
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
}
