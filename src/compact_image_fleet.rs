use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use serde::Serialize;

use crate::CompactImageInspectionReport;
use crate::MigrationManifest;
use crate::ensure_codex_closed;
use crate::inspect_compact_images;
use crate::sha256_file;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactImageFleetStatus {
    PolicyEnabled,
    InvalidManifest,
    NonCanonicalManifest,
    ActiveCandidateMissing,
    CandidateChangedRequiresRefresh,
    StableRequiresDeepScan,
    StableNoSupportedImages,
    StableMalformedImages,
    StableImagesMissingArchive,
    StableImagesMissingIndex,
    StableImagesMissingRollback,
    StableImagesReady,
    NeedsInspection,
}

impl CompactImageFleetStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::PolicyEnabled => "policy_enabled",
            Self::InvalidManifest => "invalid_manifest",
            Self::NonCanonicalManifest => "non_canonical_manifest",
            Self::ActiveCandidateMissing => "active_candidate_missing",
            Self::CandidateChangedRequiresRefresh => "candidate_changed_requires_refresh",
            Self::StableRequiresDeepScan => "stable_requires_deep_scan",
            Self::StableNoSupportedImages => "stable_no_supported_images",
            Self::StableMalformedImages => "stable_malformed_images",
            Self::StableImagesMissingArchive => "stable_images_missing_archive",
            Self::StableImagesMissingIndex => "stable_images_missing_index",
            Self::StableImagesMissingRollback => "stable_images_missing_rollback",
            Self::StableImagesReady => "stable_images_ready",
            Self::NeedsInspection => "needs_inspection",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactImageFleetEntry {
    pub thread_id: String,
    pub manifest_path: String,
    pub active_path: Option<String>,
    pub source_bytes: Option<u64>,
    pub expected_candidate_bytes: Option<u64>,
    pub active_bytes: Option<u64>,
    pub archive_exists: bool,
    pub index_exists: bool,
    pub rollback_exists: bool,
    pub status: CompactImageFleetStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inspection: Option<CompactImageInspectionReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactImageFleetReport {
    pub runtime_root: String,
    pub deep_scan: bool,
    pub manifests_scanned: u64,
    pub status_counts: BTreeMap<String, u64>,
    pub supported_image_occurrences: u64,
    pub summed_unique_image_references: u64,
    pub inline_characters: u64,
    pub entries: Vec<CompactImageFleetEntry>,
}

pub fn scan_compact_image_fleet(
    runtime_root: &Path,
    deep_scan: bool,
    fixture_mode: bool,
) -> Result<CompactImageFleetReport> {
    if deep_scan && !fixture_mode {
        ensure_codex_closed().context(
            "deep Compact-image fleet inspection is offline-only; metadata-only scan remains available while Codex is running",
        )?;
    }

    let vault_root = runtime_root.join("Data").join("Vault").join("Codex");
    let mut vaults = Vec::new();
    for entry in std::fs::read_dir(&vault_root)
        .with_context(|| format!("failed to read {}", vault_root.display()))?
    {
        let entry = entry.context("failed to enumerate a Compact-image vault entry")?;
        if entry
            .file_type()
            .context("failed to inspect a Compact-image vault entry")?
            .is_dir()
        {
            vaults.push(entry.path());
        }
    }
    vaults.sort();

    let mut entries = Vec::new();
    for vault in vaults {
        let manifest_path = vault.join("manifest.json");
        if !manifest_path.is_file() {
            continue;
        }
        entries.push(inspect_manifest(&manifest_path, deep_scan));
    }

    let mut status_counts = BTreeMap::<String, u64>::new();
    let mut supported_image_occurrences = 0_u64;
    let mut unique_image_references = 0_u64;
    let mut inline_characters = 0_u64;
    for entry in &entries {
        *status_counts
            .entry(entry.status.as_str().to_string())
            .or_default() += 1;
        if let Some(inspection) = &entry.inspection {
            supported_image_occurrences += inspection.supported_image_occurrences;
            unique_image_references += inspection.unique_image_references;
            inline_characters += inspection.inline_characters;
        }
    }

    Ok(CompactImageFleetReport {
        runtime_root: runtime_root.to_string_lossy().into_owned(),
        deep_scan,
        manifests_scanned: u64::try_from(entries.len())?,
        status_counts,
        supported_image_occurrences,
        summed_unique_image_references: unique_image_references,
        inline_characters,
        entries,
    })
}

fn inspect_manifest(manifest_path: &Path, deep_scan: bool) -> CompactImageFleetEntry {
    let fallback_thread_id = manifest_path
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        .unwrap_or("unknown")
        .to_string();
    let manifest = File::open(manifest_path)
        .context("failed to open manifest")
        .and_then(|file| {
            serde_json::from_reader::<_, MigrationManifest>(file).context("invalid manifest JSON")
        });
    let manifest = match manifest {
        Ok(manifest) => manifest,
        Err(error) => {
            return empty_entry(
                fallback_thread_id,
                manifest_path,
                CompactImageFleetStatus::InvalidManifest,
                error.to_string(),
            );
        }
    };

    let active_path = PathBuf::from(&manifest.original_path);
    let archive_exists = Path::new(&manifest.archive_path).is_file();
    let index_exists = Path::new(&manifest.index_path).is_file();
    let rollback_exists = Path::new(&manifest.rollback_path).is_file();
    let active_bytes = std::fs::metadata(&active_path)
        .ok()
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len());
    let mut entry = CompactImageFleetEntry {
        thread_id: manifest.thread_id.clone(),
        manifest_path: manifest_path.to_string_lossy().into_owned(),
        active_path: Some(active_path.to_string_lossy().into_owned()),
        source_bytes: Some(manifest.source_bytes),
        expected_candidate_bytes: Some(manifest.candidate_bytes),
        active_bytes,
        archive_exists,
        index_exists,
        rollback_exists,
        status: CompactImageFleetStatus::NeedsInspection,
        inspection: None,
        detail: None,
    };

    if fallback_thread_id != manifest.thread_id {
        entry.status = CompactImageFleetStatus::NonCanonicalManifest;
        entry.detail = Some("manifest thread id does not match its vault directory".to_string());
        return entry;
    }
    let Some(active_bytes) = active_bytes else {
        entry.status = CompactImageFleetStatus::ActiveCandidateMissing;
        return entry;
    };
    if active_bytes != manifest.candidate_bytes {
        entry.status = CompactImageFleetStatus::CandidateChangedRequiresRefresh;
        entry.detail = Some(format!(
            "active candidate is {active_bytes} bytes; manifest expects {}",
            manifest.candidate_bytes
        ));
        return entry;
    }
    if manifest.compact_image_policy.is_some() {
        entry.status = CompactImageFleetStatus::PolicyEnabled;
        return entry;
    }
    if !deep_scan {
        entry.status = CompactImageFleetStatus::StableRequiresDeepScan;
        return entry;
    }

    let inspection = match inspect_compact_images(&active_path) {
        Ok(inspection) => inspection,
        Err(error) => {
            entry.status = CompactImageFleetStatus::NeedsInspection;
            entry.detail = Some(error.to_string());
            return entry;
        }
    };
    if inspection.source_sha256 != manifest.candidate_sha256 {
        entry.status = CompactImageFleetStatus::CandidateChangedRequiresRefresh;
        entry.detail = Some("active candidate hash no longer matches its manifest".to_string());
    } else if inspection.malformed_base64_occurrences > 0 {
        entry.status = CompactImageFleetStatus::StableMalformedImages;
    } else if inspection.supported_image_occurrences == 0 {
        entry.status = CompactImageFleetStatus::StableNoSupportedImages;
    } else if !archive_exists {
        entry.status = CompactImageFleetStatus::StableImagesMissingArchive;
    } else if !index_exists {
        entry.status = CompactImageFleetStatus::StableImagesMissingIndex;
    } else if !rollback_exists {
        entry.status = CompactImageFleetStatus::StableImagesMissingRollback;
    } else {
        match verify_full_history_owner(
            Path::new(&manifest.archive_path),
            manifest.source_bytes,
            &manifest.source_sha256,
            "full archive",
        )
        .and_then(|_| {
            verify_full_history_owner(
                Path::new(&manifest.rollback_path),
                manifest.source_bytes,
                &manifest.source_sha256,
                "same-volume rollback",
            )
        }) {
            Ok(()) => entry.status = CompactImageFleetStatus::StableImagesReady,
            Err(error) => {
                entry.status = CompactImageFleetStatus::NeedsInspection;
                entry.detail = Some(error.to_string());
            }
        }
    }
    entry.inspection = Some(inspection);
    entry
}

fn verify_full_history_owner(
    path: &Path,
    expected_bytes: u64,
    expected_sha256: &str,
    label: &str,
) -> Result<()> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("{label} is missing: {}", path.display()))?;
    if !metadata.is_file() || metadata.len() != expected_bytes {
        anyhow::bail!("{label} size does not match its manifest");
    }
    if sha256_file(path)? != expected_sha256 {
        anyhow::bail!("{label} hash does not match its manifest");
    }
    Ok(())
}

fn empty_entry(
    thread_id: String,
    manifest_path: &Path,
    status: CompactImageFleetStatus,
    detail: String,
) -> CompactImageFleetEntry {
    CompactImageFleetEntry {
        thread_id,
        manifest_path: manifest_path.to_string_lossy().into_owned(),
        active_path: None,
        source_bytes: None,
        expected_candidate_bytes: None,
        active_bytes: None,
        archive_exists: false,
        index_exists: false,
        rollback_exists: false,
        status,
        inspection: None,
        detail: Some(detail),
    }
}
