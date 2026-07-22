use std::fs::File;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use conversation_lifecycle_manager::CompactImageFleetStatus;
use conversation_lifecycle_manager::MigrationManifest;
use conversation_lifecycle_manager::scan_compact_image_fleet;
use conversation_lifecycle_manager::sha256_file;
use serde_json::json;
use tempfile::tempdir;

#[test]
fn deep_fleet_scan_classifies_candidates_without_mutating_them() -> Result<()> {
    let temp = tempdir()?;
    let runtime_root = temp.path().join("runtime");
    create_managed_thread(
        &runtime_root,
        "00000000-0000-7000-8000-000000000101",
        true,
        true,
        false,
        false,
    )?;
    create_managed_thread(
        &runtime_root,
        "00000000-0000-7000-8000-000000000102",
        false,
        true,
        false,
        false,
    )?;
    create_managed_thread(
        &runtime_root,
        "00000000-0000-7000-8000-000000000103",
        true,
        false,
        false,
        false,
    )?;
    create_managed_thread(
        &runtime_root,
        "00000000-0000-7000-8000-000000000104",
        true,
        true,
        false,
        true,
    )?;
    create_managed_thread(
        &runtime_root,
        "00000000-0000-7000-8000-000000000105",
        true,
        true,
        true,
        false,
    )?;

    let before = snapshot_active_files(&runtime_root)?;
    let report = scan_compact_image_fleet(&runtime_root, true, true)?;
    let after = snapshot_active_files(&runtime_root)?;
    assert_eq!(before, after);
    assert_eq!(report.manifests_scanned, 5);
    assert_eq!(report.status_counts["stable_images_ready"], 1);
    assert_eq!(report.status_counts["stable_no_supported_images"], 1);
    assert_eq!(report.status_counts["stable_images_missing_rollback"], 1);
    assert_eq!(
        report.status_counts["candidate_changed_requires_refresh"],
        1
    );
    assert_eq!(report.status_counts["policy_enabled"], 1);
    assert_eq!(report.supported_image_occurrences, 2);

    let ready = report
        .entries
        .iter()
        .find(|entry| entry.status == CompactImageFleetStatus::StableImagesReady)
        .expect("ready candidate");
    let inspection = ready.inspection.as_ref().expect("deep inspection");
    assert_eq!(inspection.supported_image_occurrences, 1);
    assert_eq!(inspection.unique_image_references, 1);
    Ok(())
}

#[test]
fn metadata_only_scan_does_not_parse_active_candidates() -> Result<()> {
    let temp = tempdir()?;
    let runtime_root = temp.path().join("runtime");
    create_managed_thread(
        &runtime_root,
        "00000000-0000-7000-8000-000000000201",
        true,
        true,
        false,
        false,
    )?;
    let report = scan_compact_image_fleet(&runtime_root, false, true)?;
    assert_eq!(report.manifests_scanned, 1);
    assert_eq!(report.status_counts["stable_requires_deep_scan"], 1);
    assert!(report.entries[0].inspection.is_none());
    Ok(())
}

fn create_managed_thread(
    runtime_root: &Path,
    thread_id: &str,
    include_image: bool,
    include_rollback: bool,
    policy_enabled: bool,
    append_after_manifest: bool,
) -> Result<()> {
    let vault = runtime_root
        .join("Data")
        .join("Vault")
        .join("Codex")
        .join(thread_id);
    let segments = vault.join("segments");
    std::fs::create_dir_all(&segments)?;
    let active = vault.join("active.jsonl");
    let archive = segments.join("rollout-full.jsonl");
    let rollback = vault.join("rollout.jsonl.clm-rollback");
    let index = runtime_root
        .join("Data")
        .join("Indexes")
        .join(format!("{thread_id}.sqlite"));
    std::fs::create_dir_all(index.parent().expect("index parent"))?;
    std::fs::write(&index, b"fixture index")?;

    let image = if include_image {
        json!({
            "type": "input_image",
            "image_url": format!("data:image/png;base64,{}", "QUFB".repeat(16))
        })
    } else {
        json!({"type": "input_text", "text": "no image"})
    };
    let records = vec![
        json!({
            "timestamp": "2026-01-01T00:00:00Z",
            "type": "session_meta",
            "payload": {"id": thread_id, "cwd": "D:\\Fixture"}
        }),
        json!({
            "timestamp": "2026-01-01T00:00:01Z",
            "type": "compacted",
            "payload": {
                "message": "checkpoint",
                "replacement_history": [{
                    "type": "message",
                    "role": "user",
                    "content": [image]
                }],
                "window_number": 1
            }
        }),
    ];
    write_jsonl(&active, &records)?;
    std::fs::copy(&active, &archive)?;
    if include_rollback {
        std::fs::copy(&active, &rollback)?;
    }

    let candidate_bytes = std::fs::metadata(&active)?.len();
    let candidate_sha256 = sha256_file(&active)?;
    let manifest = MigrationManifest {
        format_version: 1,
        prepared_at_unix_ms: 1,
        thread_id: thread_id.to_string(),
        original_path: active.to_string_lossy().into_owned(),
        archive_path: archive.to_string_lossy().into_owned(),
        candidate_path: vault
            .join("active.jsonl.clm-new")
            .to_string_lossy()
            .into_owned(),
        rollback_path: rollback.to_string_lossy().into_owned(),
        index_path: index.to_string_lossy().into_owned(),
        source_bytes: candidate_bytes,
        candidate_bytes,
        source_sha256: candidate_sha256.clone(),
        candidate_sha256,
        oracle_version: "fixture".to_string(),
        full_turns: 1,
        active_tail_turns: 1,
        compact_image_policy: policy_enabled.then(|| "fixture-policy".to_string()),
    };
    std::fs::write(
        vault.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    if append_after_manifest {
        let mut file = std::fs::OpenOptions::new().append(true).open(&active)?;
        serde_json::to_writer(
            &mut file,
            &json!({
                "timestamp": "2026-01-01T00:00:02Z",
                "type": "event_msg",
                "payload": {"type": "turn_started", "turn_id": "new-turn"}
            }),
        )?;
        file.write_all(b"\n")?;
    }
    Ok(())
}

fn snapshot_active_files(runtime_root: &Path) -> Result<Vec<(String, String)>> {
    let root = runtime_root.join("Data").join("Vault").join("Codex");
    let mut snapshot = Vec::new();
    for vault in std::fs::read_dir(root)? {
        let active = vault?.path().join("active.jsonl");
        snapshot.push((active.to_string_lossy().into_owned(), sha256_file(&active)?));
    }
    snapshot.sort();
    Ok(snapshot)
}

fn write_jsonl(path: &Path, values: &[serde_json::Value]) -> Result<()> {
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    for value in values {
        serde_json::to_writer(&mut writer, value)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(())
}
