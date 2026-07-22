use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Write;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

use crate::CodexOracle;
use crate::MigrationManifest;
use crate::ensure_codex_closed;
use crate::path_safety::remove_dir_all_scoped;
use crate::read_rollout_thread_id;
use crate::sha256_file;

const PLAN_NAME: &str = "compact-images.pending.json";
const ARCHIVE_MANIFEST_NAME: &str = "compact-images.json";
const PREPARED_MANIFEST_NAME: &str = "manifest.clm-images-new.json";
const TRANSACTION_FORMAT_VERSION: u32 = 2;
pub(crate) const COMPACT_IMAGE_POLICY: &str = "exact_archive_with_model_reference_v1";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactImageAttachment {
    pub reference_sha256: String,
    pub content_sha256: String,
    pub media_type: String,
    pub bytes: u64,
    pub inline_characters: u64,
    pub occurrences: u64,
    pub relative_path: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactImageOccurrence {
    pub occurrence_ordinal: u64,
    pub jsonl_record_ordinal: u64,
    pub json_pointer: String,
    pub reference_sha256: String,
    pub content_sha256: String,
    pub relative_path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactImageExternalizationPlan {
    pub format_version: u32,
    pub prepared_at_unix_ms: u128,
    pub transaction_id: String,
    pub thread_id: String,
    pub migration_manifest_path: String,
    pub source_manifest_sha256: String,
    pub active_path: String,
    pub source_candidate_bytes: u64,
    pub source_candidate_sha256: String,
    pub full_archive_path: String,
    pub full_archive_bytes: u64,
    pub full_archive_sha256: String,
    pub same_volume_rollback_path: String,
    pub history_index_path: String,
    pub prepared_candidate_path: String,
    pub prepared_candidate_bytes: u64,
    pub prepared_candidate_sha256: String,
    pub prepared_manifest_path: String,
    pub staging_attachment_directory: String,
    pub final_attachment_directory: String,
    pub compacted_records: u64,
    pub image_occurrences: u64,
    pub unique_images: u64,
    pub inline_characters_removed: u64,
    pub oracle_version: String,
    pub attachments: Vec<CompactImageAttachment>,
    pub occurrences: Vec<CompactImageOccurrence>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactImageArchiveManifest {
    pub format_version: u32,
    pub applied_at_unix_ms: u128,
    pub transaction_id: String,
    pub thread_id: String,
    pub migration_manifest_path: String,
    pub active_path: String,
    pub active_bytes: u64,
    pub active_sha256: String,
    pub previous_active_path: String,
    pub previous_active_bytes: u64,
    pub previous_active_sha256: String,
    pub previous_manifest_path: String,
    pub previous_manifest_sha256: String,
    pub full_archive_path: String,
    pub full_archive_bytes: u64,
    pub full_archive_sha256: String,
    pub same_volume_rollback_path: String,
    pub history_index_path: String,
    pub attachment_directory: String,
    pub compacted_records: u64,
    pub image_occurrences: u64,
    pub unique_images: u64,
    pub inline_characters_removed: u64,
    pub oracle_version: String,
    pub attachments: Vec<CompactImageAttachment>,
    pub occurrences: Vec<CompactImageOccurrence>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactImageExternalizationApplyReport {
    pub thread_id: String,
    pub active_path: String,
    pub active_bytes_before: u64,
    pub active_bytes_after: u64,
    pub active_bytes_reclaimed: u64,
    pub previous_active_path: String,
    pub previous_manifest_path: String,
    pub archive_manifest_path: String,
    pub image_occurrences: u64,
    pub unique_images: u64,
    pub state: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactImageInspectionReport {
    pub source_path: String,
    pub source_bytes: u64,
    pub source_sha256: String,
    pub records_scanned: u64,
    pub compacted_records: u64,
    pub compacted_records_with_images: u64,
    pub input_image_occurrences: u64,
    pub supported_image_occurrences: u64,
    pub malformed_base64_occurrences: u64,
    pub unique_image_references: u64,
    pub inline_characters: u64,
    pub estimated_decoded_bytes: u64,
}

#[derive(Default)]
struct CompactImageInspectionStats {
    input_image_occurrences: u64,
    supported_image_occurrences: u64,
    malformed_base64_occurrences: u64,
    inline_characters: u64,
    estimated_decoded_bytes: u64,
    unique_image_references: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct AttachmentAccumulator {
    attachment: CompactImageAttachment,
    final_path: PathBuf,
}

struct TransformContext<'a> {
    staging_directory: &'a Path,
    final_directory: &'a Path,
    attachments: BTreeMap<String, AttachmentAccumulator>,
    occurrences: Vec<CompactImageOccurrence>,
    compacted_records: u64,
    image_occurrences: u64,
    inline_characters_removed: u64,
}

struct DecodedImage {
    reference_sha256: String,
    content_sha256: String,
    media_type: String,
    bytes: Vec<u8>,
}

pub fn inspect_compact_images(path: &Path) -> Result<CompactImageInspectionReport> {
    let source_path = std::fs::canonicalize(path)
        .with_context(|| format!("failed to resolve {}", path.display()))?;
    let metadata_before = std::fs::metadata(&source_path)?;
    if !metadata_before.is_file() {
        bail!("Compact-image inspection source is not a file");
    }

    let mut reader = BufReader::new(File::open(&source_path)?);
    let mut line = Vec::new();
    let mut source_hasher = Sha256::new();
    let mut records_scanned = 0_u64;
    let mut compacted_records = 0_u64;
    let mut compacted_records_with_images = 0_u64;
    let mut stats = CompactImageInspectionStats::default();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            bail!("Compact-image inspection source contains an incomplete JSONL record");
        }
        source_hasher.update(&line);
        records_scanned += 1;
        let record: Value = serde_json::from_slice(trim_line_ending(&line))
            .context("Compact-image inspection source contains invalid JSONL")?;
        if record.get("type").and_then(Value::as_str) != Some("compacted") {
            continue;
        }
        compacted_records += 1;
        let supported_before = stats.supported_image_occurrences;
        if let Some(history) = record
            .get("payload")
            .and_then(|payload| payload.get("replacement_history"))
        {
            inspect_inline_images(history, &mut stats)?;
        }
        if stats.supported_image_occurrences > supported_before {
            compacted_records_with_images += 1;
        }
    }

    let metadata_after = std::fs::metadata(&source_path)?;
    let changed_length = metadata_before.len() != metadata_after.len();
    let changed_timestamp = metadata_before.modified().ok() != metadata_after.modified().ok();
    if changed_length || changed_timestamp {
        bail!("Compact-image inspection source changed while it was being scanned");
    }

    Ok(CompactImageInspectionReport {
        source_path: source_path.to_string_lossy().into_owned(),
        source_bytes: metadata_after.len(),
        source_sha256: format!("{:x}", source_hasher.finalize()),
        records_scanned,
        compacted_records,
        compacted_records_with_images,
        input_image_occurrences: stats.input_image_occurrences,
        supported_image_occurrences: stats.supported_image_occurrences,
        malformed_base64_occurrences: stats.malformed_base64_occurrences,
        unique_image_references: u64::try_from(stats.unique_image_references.len())?,
        inline_characters: stats.inline_characters,
        estimated_decoded_bytes: stats.estimated_decoded_bytes,
    })
}

pub fn prepare_compact_image_externalization(
    migration_manifest_path: &Path,
    backend: Option<PathBuf>,
    runtime_root: PathBuf,
    fixture_mode: bool,
) -> Result<(PathBuf, CompactImageExternalizationPlan)> {
    if !fixture_mode {
        ensure_codex_closed()?;
    }
    let migration_manifest_path = std::fs::canonicalize(migration_manifest_path)
        .with_context(|| format!("failed to resolve {}", migration_manifest_path.display()))?;
    let migration_manifest: MigrationManifest =
        serde_json::from_reader(File::open(&migration_manifest_path)?)
            .context("invalid migration manifest")?;
    if !fixture_mode {
        let expected_manifest = runtime_root
            .join("Data")
            .join("Vault")
            .join("Codex")
            .join(&migration_manifest.thread_id)
            .join("manifest.json");
        let expected_manifest = std::fs::canonicalize(&expected_manifest).with_context(|| {
            format!(
                "canonical managed manifest is missing: {}",
                expected_manifest.display()
            )
        })?;
        if migration_manifest_path != expected_manifest {
            bail!(
                "Compact-image preparation requires the canonical managed manifest: expected {}, got {}",
                expected_manifest.display(),
                migration_manifest_path.display()
            );
        }
    }
    let active_path = PathBuf::from(&migration_manifest.original_path);
    let archive_path = PathBuf::from(&migration_manifest.archive_path);
    let rollback_path = PathBuf::from(&migration_manifest.rollback_path);
    let index_path = PathBuf::from(&migration_manifest.index_path);
    verify_file(
        &active_path,
        migration_manifest.candidate_bytes,
        &migration_manifest.candidate_sha256,
        "active candidate",
    )
    .context(
        "Compact-image externalization currently requires an unchanged managed candidate; run a normal lossless refresh first if turns were appended",
    )?;
    verify_file(
        &archive_path,
        migration_manifest.source_bytes,
        &migration_manifest.source_sha256,
        "full archive",
    )?;
    if !rollback_path.is_file() {
        bail!(
            "Compact-image preparation requires the same-volume rollback: {}",
            rollback_path.display()
        );
    }
    verify_file(
        &rollback_path,
        migration_manifest.source_bytes,
        &migration_manifest.source_sha256,
        "same-volume rollback",
    )?;
    if !index_path.is_file() {
        bail!(
            "Compact-image preparation requires the managed history index: {}",
            index_path.display()
        );
    }

    let vault = migration_manifest_path
        .parent()
        .context("migration manifest has no vault directory")?;
    let prepared_at_unix_ms = now_unix_ms()?;
    let candidate_prefix = migration_manifest
        .candidate_sha256
        .get(..16)
        .context("candidate SHA-256 is too short for a transaction id")?;
    let transaction_id = format!("tx-{prepared_at_unix_ms}-{candidate_prefix}");
    validate_transaction_id(&transaction_id)?;
    let source_manifest_sha256 = sha256_file(&migration_manifest_path)?;
    let plan_path = vault.join(PLAN_NAME);
    let archive_manifest_path = vault.join(ARCHIVE_MANIFEST_NAME);
    let prepared_manifest_path = vault.join(PREPARED_MANIFEST_NAME);
    let staging_attachment_directory =
        vault.join(format!("attachments.clm-images-new-{transaction_id}"));
    let final_attachment_directory = vault
        .join("attachments")
        .join("compact-images")
        .join(&transaction_id);
    let prepared_candidate_path = sidecar_path(&active_path, "clm-images-new")?;
    for path in [
        &plan_path,
        &archive_manifest_path,
        &prepared_manifest_path,
        &staging_attachment_directory,
        &final_attachment_directory,
        &prepared_candidate_path,
    ] {
        if path.exists() {
            bail!(
                "Compact-image transaction path already exists: {}",
                path.display()
            );
        }
    }

    let result = (|| -> Result<CompactImageExternalizationPlan> {
        std::fs::create_dir(&staging_attachment_directory).with_context(|| {
            format!(
                "failed to create attachment staging directory {}",
                staging_attachment_directory.display()
            )
        })?;
        let mut transform = TransformContext {
            staging_directory: &staging_attachment_directory,
            final_directory: &final_attachment_directory,
            attachments: BTreeMap::new(),
            occurrences: Vec::new(),
            compacted_records: 0,
            image_occurrences: 0,
            inline_characters_removed: 0,
        };
        rewrite_candidate(&active_path, &prepared_candidate_path, &mut transform)?;
        if transform.image_occurrences == 0 {
            bail!("active candidate has no supported inline Compact images");
        }
        if read_rollout_thread_id(&prepared_candidate_path)? != migration_manifest.thread_id {
            bail!("prepared candidate changed the thread id");
        }
        let prepared_candidate_bytes = std::fs::metadata(&prepared_candidate_path)?.len();
        if prepared_candidate_bytes >= migration_manifest.candidate_bytes {
            bail!("externalized candidate did not reduce active Resume bytes");
        }
        let prepared_candidate_sha256 = sha256_file(&prepared_candidate_path)?;
        let source_after_prepare = sha256_file(&active_path)?;
        if source_after_prepare != migration_manifest.candidate_sha256 {
            bail!("active candidate changed while image externalization was being prepared");
        }

        let (oracle_version, active_tail_turns) = if let Some(backend) = backend {
            let projection =
                CodexOracle::new(backend, runtime_root).project(&prepared_candidate_path)?;
            if projection.thread_id != migration_manifest.thread_id {
                bail!("official backend returned the wrong thread id for the prepared candidate");
            }
            let projected_turns = u64::try_from(projection.turns.len())?;
            if projected_turns != migration_manifest.active_tail_turns {
                bail!(
                    "official backend changed active-tail turn count: expected {}, got {projected_turns}",
                    migration_manifest.active_tail_turns
                );
            }
            (projection.oracle_version, projected_turns)
        } else if fixture_mode {
            (
                "fixture-compact-image-externalization".to_string(),
                migration_manifest.active_tail_turns,
            )
        } else {
            bail!("official backend is required outside fixture mode");
        };

        let mut prepared_manifest = migration_manifest.clone();
        prepared_manifest.prepared_at_unix_ms = now_unix_ms()?;
        prepared_manifest.candidate_path = prepared_candidate_path.to_string_lossy().into_owned();
        prepared_manifest.candidate_bytes = prepared_candidate_bytes;
        prepared_manifest.candidate_sha256 = prepared_candidate_sha256.clone();
        prepared_manifest.oracle_version = oracle_version.clone();
        prepared_manifest.active_tail_turns = active_tail_turns;
        prepared_manifest.compact_image_policy = Some(COMPACT_IMAGE_POLICY.to_string());
        write_new_json(&prepared_manifest_path, &prepared_manifest)?;

        let attachments = transform
            .attachments
            .into_values()
            .map(|value| value.attachment)
            .collect::<Vec<_>>();
        verify_occurrence_ledger(
            &attachments,
            &transform.occurrences,
            transform.image_occurrences,
            &active_path,
        )?;
        let plan = CompactImageExternalizationPlan {
            format_version: TRANSACTION_FORMAT_VERSION,
            prepared_at_unix_ms,
            transaction_id,
            thread_id: migration_manifest.thread_id,
            migration_manifest_path: migration_manifest_path.to_string_lossy().into_owned(),
            source_manifest_sha256,
            active_path: active_path.to_string_lossy().into_owned(),
            source_candidate_bytes: migration_manifest.candidate_bytes,
            source_candidate_sha256: migration_manifest.candidate_sha256,
            full_archive_path: archive_path.to_string_lossy().into_owned(),
            full_archive_bytes: migration_manifest.source_bytes,
            full_archive_sha256: migration_manifest.source_sha256,
            same_volume_rollback_path: rollback_path.to_string_lossy().into_owned(),
            history_index_path: index_path.to_string_lossy().into_owned(),
            prepared_candidate_path: prepared_candidate_path.to_string_lossy().into_owned(),
            prepared_candidate_bytes,
            prepared_candidate_sha256,
            prepared_manifest_path: prepared_manifest_path.to_string_lossy().into_owned(),
            staging_attachment_directory: staging_attachment_directory
                .to_string_lossy()
                .into_owned(),
            final_attachment_directory: final_attachment_directory.to_string_lossy().into_owned(),
            compacted_records: transform.compacted_records,
            image_occurrences: transform.image_occurrences,
            unique_images: u64::try_from(attachments.len())?,
            inline_characters_removed: transform.inline_characters_removed,
            oracle_version,
            attachments,
            occurrences: transform.occurrences,
        };
        write_new_json(&plan_path, &plan)?;
        Ok(plan)
    })();

    match result {
        Ok(plan) => Ok((plan_path, plan)),
        Err(error) => {
            remove_file_if_exists(&prepared_candidate_path);
            remove_file_if_exists(&prepared_manifest_path);
            remove_file_if_exists(&plan_path);
            if let Err(cleanup_error) = remove_dir_all_scoped(
                &staging_attachment_directory,
                vault,
                "Compact-image staging cleanup",
            ) {
                return Err(error).context(format!(
                    "Compact-image preparation failed and staging cleanup was refused: {cleanup_error}"
                ));
            }
            Err(error)
        }
    }
}

pub fn apply_compact_image_externalization(
    plan_path: &Path,
    fixture_mode: bool,
) -> Result<CompactImageExternalizationApplyReport> {
    if !fixture_mode {
        ensure_codex_closed()?;
    }
    let plan_path = std::fs::canonicalize(plan_path)
        .with_context(|| format!("failed to resolve {}", plan_path.display()))?;
    let plan: CompactImageExternalizationPlan = serde_json::from_reader(File::open(&plan_path)?)
        .context("invalid Compact-image externalization plan")?;
    if plan.format_version != TRANSACTION_FORMAT_VERSION {
        bail!(
            "unsupported Compact-image plan format {}",
            plan.format_version
        );
    }
    validate_transaction_id(&plan.transaction_id)?;

    let migration_manifest_path = PathBuf::from(&plan.migration_manifest_path);
    let active_path = PathBuf::from(&plan.active_path);
    let prepared_candidate_path = PathBuf::from(&plan.prepared_candidate_path);
    let prepared_manifest_path = PathBuf::from(&plan.prepared_manifest_path);
    let staging_attachment_directory = PathBuf::from(&plan.staging_attachment_directory);
    let final_attachment_directory = PathBuf::from(&plan.final_attachment_directory);
    let vault = migration_manifest_path
        .parent()
        .context("migration manifest has no vault directory")?;
    let expected_attachment_directory = vault
        .join("attachments")
        .join("compact-images")
        .join(&plan.transaction_id);
    if final_attachment_directory != expected_attachment_directory {
        bail!(
            "Compact-image attachment directory escaped its thread transaction: expected {}, got {}",
            expected_attachment_directory.display(),
            final_attachment_directory.display()
        );
    }
    let archive_manifest_path = vault.join(ARCHIVE_MANIFEST_NAME);
    if archive_manifest_path.exists() || final_attachment_directory.exists() {
        bail!("Compact-image externalization is already applied or requires inspection");
    }

    let current_manifest: MigrationManifest =
        serde_json::from_reader(File::open(&migration_manifest_path)?)
            .context("invalid current migration manifest")?;
    if current_manifest.thread_id != plan.thread_id
        || current_manifest.candidate_bytes != plan.source_candidate_bytes
        || current_manifest.candidate_sha256 != plan.source_candidate_sha256
    {
        bail!("current migration manifest no longer matches the reviewed plan");
    }
    if sha256_file(&migration_manifest_path)? != plan.source_manifest_sha256 {
        bail!("current migration manifest bytes no longer match the reviewed plan");
    }
    verify_file(
        &active_path,
        plan.source_candidate_bytes,
        &plan.source_candidate_sha256,
        "active candidate",
    )?;
    verify_file(
        &prepared_candidate_path,
        plan.prepared_candidate_bytes,
        &plan.prepared_candidate_sha256,
        "prepared externalized candidate",
    )?;
    verify_file(
        Path::new(&plan.full_archive_path),
        plan.full_archive_bytes,
        &plan.full_archive_sha256,
        "full history archive",
    )?;
    verify_file(
        Path::new(&plan.same_volume_rollback_path),
        plan.full_archive_bytes,
        &plan.full_archive_sha256,
        "same-volume rollback",
    )?;
    if !Path::new(&plan.history_index_path).is_file() {
        bail!("managed history index disappeared after preparation");
    }
    let prepared_manifest: MigrationManifest =
        serde_json::from_reader(File::open(&prepared_manifest_path)?)
            .context("invalid prepared migration manifest")?;
    if prepared_manifest.thread_id != plan.thread_id
        || prepared_manifest.candidate_bytes != plan.prepared_candidate_bytes
        || prepared_manifest.candidate_sha256 != plan.prepared_candidate_sha256
    {
        bail!("prepared migration manifest does not describe the externalized candidate");
    }
    verify_staged_attachments(&plan, &staging_attachment_directory)?;
    verify_occurrence_ledger(
        &plan.attachments,
        &plan.occurrences,
        plan.image_occurrences,
        &active_path,
    )?;

    let stamp = now_unix_ms()?;
    let previous_active_path = sidecar_path(&active_path, &format!("clm-images-previous-{stamp}"))?;
    let previous_manifest_path = vault.join(format!("manifest.clm-images-previous-{stamp}.json"));
    if previous_active_path.exists() || previous_manifest_path.exists() {
        bail!("Compact-image rollback output already exists");
    }

    std::fs::create_dir_all(
        final_attachment_directory
            .parent()
            .context("final attachment directory has no parent")?,
    )?;
    let apply_result = (|| -> Result<()> {
        std::fs::rename(&staging_attachment_directory, &final_attachment_directory)
            .context("failed to activate archived Compact images")?;
        std::fs::rename(&active_path, &previous_active_path)
            .context("failed to preserve the previous active candidate")?;
        std::fs::rename(&prepared_candidate_path, &active_path)
            .context("failed to activate the externalized candidate")?;
        verify_file(
            &active_path,
            plan.prepared_candidate_bytes,
            &plan.prepared_candidate_sha256,
            "activated externalized candidate",
        )?;
        std::fs::rename(&migration_manifest_path, &previous_manifest_path)
            .context("failed to preserve the previous migration manifest")?;
        std::fs::rename(&prepared_manifest_path, &migration_manifest_path)
            .context("failed to activate the externalized migration manifest")?;

        let archive_manifest = CompactImageArchiveManifest {
            format_version: TRANSACTION_FORMAT_VERSION,
            applied_at_unix_ms: now_unix_ms()?,
            transaction_id: plan.transaction_id.clone(),
            thread_id: plan.thread_id.clone(),
            migration_manifest_path: plan.migration_manifest_path.clone(),
            active_path: plan.active_path.clone(),
            active_bytes: plan.prepared_candidate_bytes,
            active_sha256: plan.prepared_candidate_sha256.clone(),
            previous_active_path: previous_active_path.to_string_lossy().into_owned(),
            previous_active_bytes: plan.source_candidate_bytes,
            previous_active_sha256: plan.source_candidate_sha256.clone(),
            previous_manifest_path: previous_manifest_path.to_string_lossy().into_owned(),
            previous_manifest_sha256: plan.source_manifest_sha256.clone(),
            full_archive_path: plan.full_archive_path.clone(),
            full_archive_bytes: plan.full_archive_bytes,
            full_archive_sha256: plan.full_archive_sha256.clone(),
            same_volume_rollback_path: plan.same_volume_rollback_path.clone(),
            history_index_path: plan.history_index_path.clone(),
            attachment_directory: final_attachment_directory.to_string_lossy().into_owned(),
            compacted_records: plan.compacted_records,
            image_occurrences: plan.image_occurrences,
            unique_images: plan.unique_images,
            inline_characters_removed: plan.inline_characters_removed,
            oracle_version: plan.oracle_version.clone(),
            attachments: plan.attachments.clone(),
            occurrences: plan.occurrences.clone(),
        };
        write_new_json(&archive_manifest_path, &archive_manifest)?;
        verify_compact_image_archive(&archive_manifest_path)?;
        std::fs::remove_file(&plan_path).context("failed to retire the applied plan")?;
        Ok(())
    })();

    if let Err(error) = apply_result {
        let rollback = rollback_apply(
            &migration_manifest_path,
            &previous_manifest_path,
            &prepared_manifest_path,
            &active_path,
            &previous_active_path,
            &prepared_candidate_path,
            &staging_attachment_directory,
            &final_attachment_directory,
            &archive_manifest_path,
            &plan_path,
        );
        return match rollback {
            Ok(()) => Err(error).context(
                "Compact-image activation failed; the previous managed state was restored",
            ),
            Err(rollback_error) => Err(anyhow::anyhow!(
                "Compact-image activation failed ({error}); EMERGENCY rollback also failed ({rollback_error})"
            )),
        };
    }

    Ok(CompactImageExternalizationApplyReport {
        thread_id: plan.thread_id,
        active_path: plan.active_path,
        active_bytes_before: plan.source_candidate_bytes,
        active_bytes_after: plan.prepared_candidate_bytes,
        active_bytes_reclaimed: plan
            .source_candidate_bytes
            .saturating_sub(plan.prepared_candidate_bytes),
        previous_active_path: previous_active_path.to_string_lossy().into_owned(),
        previous_manifest_path: previous_manifest_path.to_string_lossy().into_owned(),
        archive_manifest_path: archive_manifest_path.to_string_lossy().into_owned(),
        image_occurrences: plan.image_occurrences,
        unique_images: plan.unique_images,
        state: "applied_with_exact_images_and_previous_candidate_retained".to_string(),
    })
}

pub fn verify_compact_image_archive(
    archive_manifest_path: &Path,
) -> Result<CompactImageArchiveManifest> {
    let archive: CompactImageArchiveManifest =
        serde_json::from_reader(File::open(archive_manifest_path)?)
            .context("invalid Compact-image archive manifest")?;
    if archive.format_version != TRANSACTION_FORMAT_VERSION {
        bail!(
            "unsupported Compact-image archive format {}",
            archive.format_version
        );
    }
    validate_transaction_id(&archive.transaction_id)?;
    let vault = archive_manifest_path
        .parent()
        .context("Compact-image archive manifest has no vault directory")?;
    let vault =
        std::fs::canonicalize(vault).context("failed to resolve Compact-image archive vault")?;
    if vault.file_name().and_then(|value| value.to_str()) != Some(&archive.thread_id) {
        bail!("Compact-image archive is stored under the wrong thread vault");
    }
    let attachment_root = std::fs::canonicalize(&archive.attachment_directory)
        .context("failed to resolve Compact-image attachment directory")?;
    let expected_attachment_root = vault
        .join("attachments")
        .join("compact-images")
        .join(&archive.transaction_id);
    let expected_attachment_root = std::fs::canonicalize(&expected_attachment_root)
        .context("failed to resolve expected Compact-image attachment directory")?;
    if attachment_root != expected_attachment_root {
        bail!("Compact-image archive attachment directory does not match its transaction");
    }
    verify_file(
        Path::new(&archive.active_path),
        archive.active_bytes,
        &archive.active_sha256,
        "externalized active candidate",
    )?;
    verify_file(
        Path::new(&archive.previous_active_path),
        archive.previous_active_bytes,
        &archive.previous_active_sha256,
        "previous active candidate",
    )?;
    verify_file(
        Path::new(&archive.previous_manifest_path),
        std::fs::metadata(&archive.previous_manifest_path)?.len(),
        &archive.previous_manifest_sha256,
        "previous migration manifest",
    )?;
    verify_file(
        Path::new(&archive.full_archive_path),
        archive.full_archive_bytes,
        &archive.full_archive_sha256,
        "full history archive",
    )?;
    verify_file(
        Path::new(&archive.same_volume_rollback_path),
        archive.full_archive_bytes,
        &archive.full_archive_sha256,
        "same-volume rollback",
    )?;
    if !Path::new(&archive.history_index_path).is_file() {
        bail!("managed history index is missing");
    }
    for attachment in &archive.attachments {
        let path = attachment_root.join(safe_attachment_file_name(&attachment.relative_path)?);
        verify_file(
            &path,
            attachment.bytes,
            &attachment.content_sha256,
            "archived Compact image",
        )?;
    }
    verify_occurrence_ledger(
        &archive.attachments,
        &archive.occurrences,
        archive.image_occurrences,
        Path::new(&archive.previous_active_path),
    )?;
    Ok(archive)
}

pub(crate) fn has_supported_inline_compact_images(path: &Path) -> Result<bool> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(false);
        }
        if line.last() != Some(&b'\n') {
            bail!("candidate contains an incomplete JSONL record");
        }
        let record: Value = serde_json::from_slice(trim_line_ending(&line))
            .context("candidate contains invalid JSONL")?;
        if record.get("type").and_then(Value::as_str) == Some("compacted")
            && record
                .get("payload")
                .and_then(|payload| payload.get("replacement_history"))
                .is_some_and(contains_supported_inline_image)
        {
            return Ok(true);
        }
    }
}

fn rewrite_candidate(
    source_path: &Path,
    output_path: &Path,
    transform: &mut TransformContext<'_>,
) -> Result<()> {
    let source = File::open(source_path)?;
    let mut reader = BufReader::new(source);
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output_path)
        .with_context(|| format!("failed to create {}", output_path.display()))?;
    let mut writer = BufWriter::new(output);
    let mut line = Vec::new();
    let mut jsonl_record_ordinal = 0_u64;
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            bail!("active candidate contains an incomplete JSONL record");
        }
        jsonl_record_ordinal += 1;
        let mut record: Value = serde_json::from_slice(trim_line_ending(&line))
            .context("active candidate contains invalid JSONL")?;
        let mut changed = false;
        if record.get("type").and_then(Value::as_str) == Some("compacted")
            && let Some(history) = record
                .get_mut("payload")
                .and_then(|payload| payload.get_mut("replacement_history"))
        {
            let before = transform.image_occurrences;
            rewrite_inline_images(
                history,
                transform,
                jsonl_record_ordinal,
                "/payload/replacement_history",
            )?;
            if transform.image_occurrences > before {
                transform.compacted_records += 1;
                changed = true;
            }
        }
        if changed {
            serde_json::to_writer(&mut writer, &record)?;
            writer.write_all(b"\n")?;
        } else {
            writer.write_all(&line)?;
        }
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}

fn rewrite_inline_images(
    value: &mut Value,
    transform: &mut TransformContext<'_>,
    jsonl_record_ordinal: u64,
    json_pointer: &str,
) -> Result<()> {
    match value {
        Value::Array(values) => {
            for (index, value) in values.iter_mut().enumerate() {
                rewrite_inline_images(
                    value,
                    transform,
                    jsonl_record_ordinal,
                    &format!("{json_pointer}/{index}"),
                )?;
            }
        }
        Value::Object(map) => {
            let is_inline_image = map.get("type").and_then(Value::as_str) == Some("input_image");
            let data_url = map
                .get("image_url")
                .and_then(Value::as_str)
                .filter(|url| url.starts_with("data:image"))
                .map(str::to_owned);
            if is_inline_image
                && let Some(data_url) = data_url
                && let Some(marker) =
                    archive_data_url(&data_url, transform, jsonl_record_ordinal, json_pointer)?
            {
                *value = json!({
                    "type": "input_text",
                    "text": marker,
                });
                return Ok(());
            }
            for (key, child) in map.iter_mut() {
                rewrite_inline_images(
                    child,
                    transform,
                    jsonl_record_ordinal,
                    &format!("{json_pointer}/{}", escape_json_pointer_segment(key)),
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn contains_supported_inline_image(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(contains_supported_inline_image),
        Value::Object(map) => {
            let supported = map.get("type").and_then(Value::as_str) == Some("input_image")
                && map
                    .get("image_url")
                    .and_then(Value::as_str)
                    .is_some_and(|url| {
                        let Some((metadata, _)) = url.split_once(',') else {
                            return false;
                        };
                        let Some(metadata) = metadata.strip_prefix("data:image/") else {
                            return false;
                        };
                        metadata
                            .split(';')
                            .skip(1)
                            .any(|part| part.eq_ignore_ascii_case("base64"))
                    });
            supported || map.values().any(contains_supported_inline_image)
        }
        _ => false,
    }
}

fn inspect_inline_images(value: &Value, stats: &mut CompactImageInspectionStats) -> Result<()> {
    match value {
        Value::Array(values) => {
            for value in values {
                inspect_inline_images(value, stats)?;
            }
        }
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("input_image") {
                stats.input_image_occurrences += 1;
                if let Some(data_url) = map.get("image_url").and_then(Value::as_str)
                    && let Some(encoded) = image_base64_payload(data_url)
                {
                    stats.inline_characters += u64::try_from(data_url.len())?;
                    match decoded_base64_len(encoded) {
                        Ok(decoded_bytes) => {
                            stats.supported_image_occurrences += 1;
                            stats.estimated_decoded_bytes += decoded_bytes;
                            stats
                                .unique_image_references
                                .insert(sha256_bytes(data_url.as_bytes()));
                        }
                        Err(_) => stats.malformed_base64_occurrences += 1,
                    }
                }
            }
            for child in map.values() {
                inspect_inline_images(child, stats)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn image_base64_payload(data_url: &str) -> Option<&str> {
    let (metadata, encoded) = data_url.split_once(',')?;
    let metadata = metadata.strip_prefix("data:")?;
    let mut metadata_parts = metadata.split(';');
    let media_type = metadata_parts.next().unwrap_or_default();
    if !media_type
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
        || !metadata_parts.any(|part| part.eq_ignore_ascii_case("base64"))
    {
        return None;
    }
    Some(encoded)
}

fn decoded_base64_len(encoded: &str) -> Result<u64> {
    let input = encoded.as_bytes();
    if input.is_empty() || !input.len().is_multiple_of(4) {
        bail!("Base64 payload length is not a non-zero multiple of four");
    }
    let mut output_len = 0_u64;
    let chunk_count = input.len() / 4;
    for (index, chunk) in input.chunks_exact(4).enumerate() {
        let is_last = index + 1 == chunk_count;
        let second = base64_value(chunk[1]).context("invalid second Base64 character")?;
        base64_value(chunk[0]).context("invalid first Base64 character")?;
        output_len += 1;

        if chunk[2] == b'=' {
            if !is_last || chunk[3] != b'=' || second & 0x0f != 0 {
                bail!("invalid Base64 double padding");
            }
            continue;
        }
        let third = base64_value(chunk[2]).context("invalid third Base64 character")?;
        output_len += 1;

        if chunk[3] == b'=' {
            if !is_last || third & 0x03 != 0 {
                bail!("invalid Base64 single padding");
            }
            continue;
        }
        base64_value(chunk[3]).context("invalid fourth Base64 character")?;
        output_len += 1;
    }
    Ok(output_len)
}

fn archive_data_url(
    data_url: &str,
    transform: &mut TransformContext<'_>,
    jsonl_record_ordinal: u64,
    json_pointer: &str,
) -> Result<Option<String>> {
    let Some(decoded) = decode_image_data_url(data_url)? else {
        return Ok(None);
    };
    let extension = extension_for_media_type(&decoded.media_type);
    let file_name = format!("{}.{}", decoded.content_sha256, extension);
    let staged_path = transform.staging_directory.join(&file_name);
    let final_path = transform.final_directory.join(&file_name);
    if staged_path.exists() {
        verify_file(
            &staged_path,
            u64::try_from(decoded.bytes.len())?,
            &decoded.content_sha256,
            "staged Compact image",
        )?;
    } else {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged_path)?;
        let mut writer = BufWriter::new(file);
        writer.write_all(&decoded.bytes)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
    }

    let inline_characters = u64::try_from(data_url.len())?;
    if let Some(existing) = transform.attachments.get_mut(&decoded.reference_sha256) {
        if existing.attachment.content_sha256 != decoded.content_sha256
            || existing.attachment.media_type != decoded.media_type
            || existing.final_path != final_path
        {
            bail!("Compact image reference hash collision");
        }
        existing.attachment.occurrences += 1;
    } else {
        transform.attachments.insert(
            decoded.reference_sha256.clone(),
            AttachmentAccumulator {
                attachment: CompactImageAttachment {
                    reference_sha256: decoded.reference_sha256.clone(),
                    content_sha256: decoded.content_sha256.clone(),
                    media_type: decoded.media_type.clone(),
                    bytes: u64::try_from(decoded.bytes.len())?,
                    inline_characters,
                    occurrences: 1,
                    relative_path: file_name.clone(),
                },
                final_path: final_path.clone(),
            },
        );
    }
    let occurrence_ordinal = transform.image_occurrences + 1;
    transform.occurrences.push(CompactImageOccurrence {
        occurrence_ordinal,
        jsonl_record_ordinal,
        json_pointer: json_pointer.to_string(),
        reference_sha256: decoded.reference_sha256.clone(),
        content_sha256: decoded.content_sha256.clone(),
        relative_path: file_name,
    });
    transform.image_occurrences += 1;
    transform.inline_characters_removed += inline_characters;
    Ok(Some(format!(
        "[Historical image externalized by CLM for Resume performance. Exact file: {}. Content SHA-256: {}. Reference SHA-256: {}. Inspect this file only when the old image is needed.]",
        final_path.display(),
        decoded.content_sha256,
        decoded.reference_sha256
    )))
}

fn decode_image_data_url(data_url: &str) -> Result<Option<DecodedImage>> {
    let Some((metadata, encoded)) = data_url.split_once(',') else {
        return Ok(None);
    };
    let Some(metadata) = metadata.strip_prefix("data:") else {
        return Ok(None);
    };
    let mut metadata_parts = metadata.split(';');
    let media_type = metadata_parts
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !media_type.starts_with("image/")
        || !metadata_parts.any(|part| part.eq_ignore_ascii_case("base64"))
    {
        return Ok(None);
    }
    let bytes = decode_base64(encoded).context("failed to decode an inline Compact image")?;
    Ok(Some(DecodedImage {
        reference_sha256: sha256_bytes(data_url.as_bytes()),
        content_sha256: sha256_bytes(&bytes),
        media_type,
        bytes,
    }))
}

fn escape_json_pointer_segment(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

fn validate_transaction_id(transaction_id: &str) -> Result<()> {
    if !transaction_id.starts_with("tx-")
        || transaction_id.len() > 96
        || transaction_id
            .chars()
            .any(|value| !value.is_ascii_alphanumeric() && value != '-')
    {
        bail!("invalid Compact-image transaction id: {transaction_id}");
    }
    Ok(())
}

fn safe_attachment_file_name(relative_path: &str) -> Result<&OsStr> {
    let mut components = Path::new(relative_path).components();
    let Some(Component::Normal(file_name)) = components.next() else {
        bail!("Compact-image attachment path is not a plain file name");
    };
    if components.next().is_some() {
        bail!("Compact-image attachment path escaped its transaction directory");
    }
    Ok(file_name)
}

fn verify_occurrence_ledger(
    attachments: &[CompactImageAttachment],
    occurrences: &[CompactImageOccurrence],
    expected_occurrences: u64,
    source_candidate_path: &Path,
) -> Result<()> {
    if u64::try_from(occurrences.len())? != expected_occurrences {
        bail!("Compact-image occurrence ledger count does not match the transaction total");
    }
    let mut attachments_by_reference = BTreeMap::new();
    for attachment in attachments {
        safe_attachment_file_name(&attachment.relative_path)?;
        if attachments_by_reference
            .insert(attachment.reference_sha256.as_str(), attachment)
            .is_some()
        {
            bail!("Compact-image attachment reference appears more than once");
        }
    }

    let mut counted_by_reference = BTreeMap::<&str, u64>::new();
    for (index, occurrence) in occurrences.iter().enumerate() {
        if occurrence.occurrence_ordinal != u64::try_from(index)? + 1 {
            bail!("Compact-image occurrence ledger ordinals are not contiguous");
        }
        if occurrence.jsonl_record_ordinal == 0
            || !occurrence
                .json_pointer
                .starts_with("/payload/replacement_history/")
        {
            bail!("Compact-image occurrence has an invalid source location");
        }
        safe_attachment_file_name(&occurrence.relative_path)?;
        let attachment = attachments_by_reference
            .get(occurrence.reference_sha256.as_str())
            .context("Compact-image occurrence references an unknown attachment")?;
        if occurrence.content_sha256 != attachment.content_sha256
            || occurrence.relative_path != attachment.relative_path
        {
            bail!("Compact-image occurrence does not match its attachment ledger entry");
        }
        *counted_by_reference
            .entry(occurrence.reference_sha256.as_str())
            .or_default() += 1;
    }
    for attachment in attachments {
        if counted_by_reference
            .get(attachment.reference_sha256.as_str())
            .copied()
            .unwrap_or_default()
            != attachment.occurrences
        {
            bail!("Compact-image attachment occurrence count does not match its source ledger");
        }
    }
    verify_occurrence_sources(source_candidate_path, occurrences)
}

fn verify_occurrence_sources(
    source_candidate_path: &Path,
    occurrences: &[CompactImageOccurrence],
) -> Result<()> {
    let mut by_record = BTreeMap::<u64, Vec<&CompactImageOccurrence>>::new();
    for occurrence in occurrences {
        by_record
            .entry(occurrence.jsonl_record_ordinal)
            .or_default()
            .push(occurrence);
    }

    let mut reader = BufReader::new(File::open(source_candidate_path)?);
    let mut line = Vec::new();
    let mut record_ordinal = 0_u64;
    let mut verified = 0_u64;
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            bail!("source candidate contains an incomplete JSONL record");
        }
        record_ordinal += 1;
        let Some(record_occurrences) = by_record.get(&record_ordinal) else {
            continue;
        };
        let record: Value = serde_json::from_slice(trim_line_ending(&line))
            .context("source candidate contains invalid JSONL")?;
        if record.get("type").and_then(Value::as_str) != Some("compacted") {
            bail!("Compact-image occurrence points outside a Compact record");
        }
        for occurrence in record_occurrences {
            let image = record
                .pointer(&occurrence.json_pointer)
                .context("Compact-image occurrence JSON pointer is missing")?;
            if image.get("type").and_then(Value::as_str) != Some("input_image") {
                bail!("Compact-image occurrence JSON pointer no longer targets an image");
            }
            let data_url = image
                .get("image_url")
                .and_then(Value::as_str)
                .context("Compact-image occurrence has no source data URL")?;
            let decoded = decode_image_data_url(data_url)?
                .context("Compact-image occurrence no longer targets a supported data URL")?;
            if decoded.reference_sha256 != occurrence.reference_sha256
                || decoded.content_sha256 != occurrence.content_sha256
            {
                bail!("Compact-image occurrence hash does not match its exact source location");
            }
            let expected_file = format!(
                "{}.{}",
                decoded.content_sha256,
                extension_for_media_type(&decoded.media_type)
            );
            if occurrence.relative_path != expected_file {
                bail!("Compact-image occurrence file name does not match its content hash");
            }
            verified += 1;
        }
    }
    if verified != u64::try_from(occurrences.len())? {
        bail!("one or more Compact-image source records disappeared");
    }
    Ok(())
}

fn verify_staged_attachments(
    plan: &CompactImageExternalizationPlan,
    staging_directory: &Path,
) -> Result<()> {
    if !staging_directory.is_dir() {
        bail!(
            "attachment staging directory is missing: {}",
            staging_directory.display()
        );
    }
    for attachment in &plan.attachments {
        let file_name = safe_attachment_file_name(&attachment.relative_path)?;
        verify_file(
            &staging_directory.join(file_name),
            attachment.bytes,
            &attachment.content_sha256,
            "staged Compact image",
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn rollback_apply(
    migration_manifest_path: &Path,
    previous_manifest_path: &Path,
    prepared_manifest_path: &Path,
    active_path: &Path,
    previous_active_path: &Path,
    prepared_candidate_path: &Path,
    staging_attachment_directory: &Path,
    final_attachment_directory: &Path,
    archive_manifest_path: &Path,
    plan_path: &Path,
) -> Result<()> {
    if archive_manifest_path.exists() {
        std::fs::remove_file(archive_manifest_path)
            .context("failed to remove the failed Compact-image archive manifest")?;
    }
    if previous_manifest_path.exists() {
        if migration_manifest_path.exists() {
            if prepared_manifest_path.exists() {
                bail!("cannot preserve the failed prepared manifest during rollback");
            }
            std::fs::rename(migration_manifest_path, prepared_manifest_path)?;
        }
        std::fs::rename(previous_manifest_path, migration_manifest_path)?;
    }
    if previous_active_path.exists() {
        if active_path.exists() {
            if prepared_candidate_path.exists() {
                bail!("cannot preserve the failed prepared candidate during rollback");
            }
            std::fs::rename(active_path, prepared_candidate_path)?;
        }
        std::fs::rename(previous_active_path, active_path)?;
    }
    if final_attachment_directory.exists() {
        if staging_attachment_directory.exists() {
            bail!("cannot restore the staged attachment directory during rollback");
        }
        std::fs::rename(final_attachment_directory, staging_attachment_directory)?;
    }
    if !plan_path.exists() {
        bail!("applied plan disappeared during rollback");
    }
    Ok(())
}

fn verify_file(path: &Path, expected_bytes: u64, expected_hash: &str, label: &str) -> Result<()> {
    let actual_bytes = std::fs::metadata(path)
        .with_context(|| format!("missing {label} file {}", path.display()))?
        .len();
    if actual_bytes != expected_bytes {
        bail!("{label} length mismatch: expected {expected_bytes}, got {actual_bytes}");
    }
    if sha256_file(path)? != expected_hash {
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

fn write_new_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.write_all(b"\n")?;
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

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn extension_for_media_type(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/avif" => "avif",
        "image/bmp" => "bmp",
        "image/svg+xml" => "svg",
        _ => "img",
    }
}

fn decode_base64(encoded: &str) -> Result<Vec<u8>> {
    let input = encoded.as_bytes();
    if input.is_empty() || !input.len().is_multiple_of(4) {
        bail!("Base64 payload length is not a non-zero multiple of four");
    }
    let mut output = Vec::with_capacity(input.len() / 4 * 3);
    let chunk_count = input.len() / 4;
    for (index, chunk) in input.chunks_exact(4).enumerate() {
        let is_last = index + 1 == chunk_count;
        let first = base64_value(chunk[0]).context("invalid first Base64 character")?;
        let second = base64_value(chunk[1]).context("invalid second Base64 character")?;
        output.push((first << 2) | (second >> 4));

        if chunk[2] == b'=' {
            if !is_last || chunk[3] != b'=' || second & 0x0f != 0 {
                bail!("invalid Base64 double padding");
            }
            continue;
        }
        let third = base64_value(chunk[2]).context("invalid third Base64 character")?;
        output.push((second << 4) | (third >> 2));

        if chunk[3] == b'=' {
            if !is_last || third & 0x03 != 0 {
                bail!("invalid Base64 single padding");
            }
            continue;
        }
        let fourth = base64_value(chunk[3]).context("invalid fourth Base64 character")?;
        output.push((third << 6) | fourth);
    }
    Ok(output)
}

fn base64_value(value: u8) -> Option<u8> {
    match value {
        b'A'..=b'Z' => Some(value - b'A'),
        b'a'..=b'z' => Some(value - b'a' + 26),
        b'0'..=b'9' => Some(value - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn now_unix_ms() -> Result<u128> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
}

fn remove_file_if_exists(path: &Path) {
    if path.is_file() {
        let _ = std::fs::remove_file(path);
    }
}
