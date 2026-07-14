use std::fs::OpenOptions;
use std::io::Write;

use anyhow::Result;
use conversation_lifecycle_manager::FixtureOptions;
use conversation_lifecycle_manager::IndexedRollout;
use conversation_lifecycle_manager::MigrationManifest;
use conversation_lifecycle_manager::build_active_candidate;
use conversation_lifecycle_manager::generate_fixture;
use conversation_lifecycle_manager::rehydrate_migration;
use tempfile::tempdir;

#[test]
fn restore_original_preserves_turns_appended_after_activation() -> Result<()> {
    let temp = tempdir()?;
    let source = temp.path().join("rollout.jsonl");
    let archive = temp.path().join("rollout-full.jsonl");
    let rollback = temp.path().join("rollout.jsonl.clm-rollback");
    let candidate = temp.path().join("rollout.jsonl.clm-new");
    let index_path = temp.path().join("thread.sqlite");
    let manifest_path = temp.path().join("manifest.json");

    generate_fixture(
        &source,
        &FixtureOptions {
            turns: 40,
            tail_after_checkpoint: 4,
            payload_bytes: 64,
            history_mode: "paginated".to_string(),
        },
    )?;
    let original_bytes = std::fs::read(&source)?;
    std::fs::copy(&source, &archive)?;

    let mut index = IndexedRollout::open(&index_path)?;
    index.sync_rollout(&source)?;
    let candidate_report = build_active_candidate(&index, &candidate)?;
    drop(index);

    std::fs::rename(&source, &rollback)?;
    std::fs::rename(&candidate, &source)?;
    let appended = b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"turn_started\",\"turn_id\":\"turn-after-activation\"}}\n";
    OpenOptions::new()
        .append(true)
        .open(&source)?
        .write_all(appended)?;

    let manifest = MigrationManifest {
        format_version: 1,
        prepared_at_unix_ms: 1,
        thread_id: candidate_report.thread_id.clone(),
        original_path: source.to_string_lossy().into_owned(),
        archive_path: archive.to_string_lossy().into_owned(),
        candidate_path: candidate.to_string_lossy().into_owned(),
        rollback_path: rollback.to_string_lossy().into_owned(),
        index_path: index_path.to_string_lossy().into_owned(),
        source_bytes: candidate_report.source_bytes,
        candidate_bytes: candidate_report.candidate_bytes,
        source_sha256: candidate_report.source_sha256,
        candidate_sha256: candidate_report.candidate_sha256,
        oracle_version: "fixture".to_string(),
        full_turns: 40,
        active_tail_turns: 4,
    };
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;

    let report = rehydrate_migration(&manifest_path, true)?;
    let mut expected = original_bytes.clone();
    expected.extend_from_slice(appended);
    assert_eq!(std::fs::read(&source)?, expected);
    assert_eq!(report.appended_bytes, appended.len() as u64);
    assert_eq!(report.restored_bytes, expected.len() as u64);
    assert!(std::path::Path::new(&report.displaced_candidate_path).is_file());
    assert!(std::path::Path::new(&report.disabled_index_path).is_file());
    assert!(!index_path.exists());
    assert_eq!(std::fs::read(&rollback)?, original_bytes);
    Ok(())
}

#[test]
fn restore_original_refuses_a_changed_active_prefix() -> Result<()> {
    let temp = tempdir()?;
    let source = temp.path().join("rollout.jsonl");
    let archive = temp.path().join("rollout-full.jsonl");
    let rollback = temp.path().join("rollout.jsonl.clm-rollback");
    let candidate = temp.path().join("rollout.jsonl.clm-new");
    let index_path = temp.path().join("thread.sqlite");
    let manifest_path = temp.path().join("manifest.json");

    generate_fixture(
        &source,
        &FixtureOptions {
            turns: 20,
            tail_after_checkpoint: 2,
            payload_bytes: 32,
            history_mode: "paginated".to_string(),
        },
    )?;
    std::fs::copy(&source, &archive)?;
    let mut index = IndexedRollout::open(&index_path)?;
    index.sync_rollout(&source)?;
    let candidate_report = build_active_candidate(&index, &candidate)?;
    drop(index);
    std::fs::rename(&source, &rollback)?;
    std::fs::rename(&candidate, &source)?;

    let mut active = std::fs::read(&source)?;
    active[0] ^= 1;
    std::fs::write(&source, active)?;
    let manifest = MigrationManifest {
        format_version: 1,
        prepared_at_unix_ms: 1,
        thread_id: candidate_report.thread_id,
        original_path: source.to_string_lossy().into_owned(),
        archive_path: archive.to_string_lossy().into_owned(),
        candidate_path: candidate.to_string_lossy().into_owned(),
        rollback_path: rollback.to_string_lossy().into_owned(),
        index_path: index_path.to_string_lossy().into_owned(),
        source_bytes: candidate_report.source_bytes,
        candidate_bytes: candidate_report.candidate_bytes,
        source_sha256: candidate_report.source_sha256,
        candidate_sha256: candidate_report.candidate_sha256,
        oracle_version: "fixture".to_string(),
        full_turns: 20,
        active_tail_turns: 2,
    };
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;

    let error = rehydrate_migration(&manifest_path, true)
        .expect_err("changed active prefix must block rehydration");
    assert!(error.to_string().contains("prefix"));
    assert!(index_path.is_file());
    Ok(())
}
