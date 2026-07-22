use std::fs::File;
use std::io::BufWriter;
use std::io::Write;

use anyhow::Result;
use conversation_lifecycle_manager::MigrationManifest;
use conversation_lifecycle_manager::apply_compact_image_externalization;
use conversation_lifecycle_manager::inspect_compact_images;
use conversation_lifecycle_manager::prepare_compact_image_externalization;
use conversation_lifecycle_manager::rehydrate_migration;
use conversation_lifecycle_manager::sha256_file;
use conversation_lifecycle_manager::verify_compact_image_archive;
use serde_json::json;
use tempfile::tempdir;

#[test]
fn compact_images_are_externalized_once_and_original_history_restores_exactly() -> Result<()> {
    let temp = tempdir()?;
    let runtime_root = temp.path().join("runtime");
    let thread_id = "00000000-0000-7000-8000-000000000777";
    let vault = runtime_root
        .join("Data")
        .join("Vault")
        .join("Codex")
        .join(thread_id);
    std::fs::create_dir_all(&vault)?;
    let active = temp.path().join("rollout.jsonl");
    let archive = vault.join("segments").join("rollout-full.jsonl");
    std::fs::create_dir_all(archive.parent().expect("archive parent"))?;
    let rollback = temp.path().join("rollout.jsonl.clm-rollback");
    let index = runtime_root
        .join("Data")
        .join("Indexes")
        .join(format!("{thread_id}.sqlite"));
    std::fs::create_dir_all(index.parent().expect("index parent"))?;
    std::fs::write(&index, b"fixture index")?;
    let manifest_path = vault.join("manifest.json");

    let data_url = format!("data:image/png;base64,{}", "QUFB".repeat(4096));
    let session_meta = json!({
        "timestamp": "2026-01-01T00:00:00Z",
        "type": "session_meta",
        "payload": {"id": thread_id, "cwd": "D:\\Fixture"}
    });
    let dead_prefix = json!({
        "timestamp": "2026-01-01T00:00:01Z",
        "type": "response_item",
        "payload": {"type": "message", "role": "user", "content": "old detail"}
    });
    let compacted = json!({
        "timestamp": "2026-01-01T00:00:02Z",
        "type": "compacted",
        "payload": {
            "message": "fixture checkpoint",
            "replacement_history": [{
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "before image"},
                    {"type": "input_image", "image_url": data_url},
                    {"type": "input_image", "image_url": data_url}
                ]
            }],
            "window_number": 1
        }
    });
    let tail = json!({
        "timestamp": "2026-01-01T00:00:03Z",
        "type": "event_msg",
        "payload": {"type": "turn_started", "turn_id": "turn-tail"}
    });

    write_jsonl(&archive, &[&session_meta, &dead_prefix, &compacted, &tail])?;
    write_jsonl(&active, &[&session_meta, &compacted, &tail])?;
    std::fs::copy(&archive, &rollback)?;
    let source_bytes = std::fs::metadata(&archive)?.len();
    let candidate_bytes = std::fs::metadata(&active)?.len();
    let source_sha256 = sha256_file(&archive)?;
    let candidate_sha256 = sha256_file(&active)?;
    let original_history = std::fs::read(&archive)?;
    let original_candidate = std::fs::read(&active)?;
    let inspection = inspect_compact_images(&active)?;
    assert_eq!(inspection.records_scanned, 3);
    assert_eq!(inspection.compacted_records, 1);
    assert_eq!(inspection.compacted_records_with_images, 1);
    assert_eq!(inspection.input_image_occurrences, 2);
    assert_eq!(inspection.supported_image_occurrences, 2);
    assert_eq!(inspection.malformed_base64_occurrences, 0);
    assert_eq!(inspection.unique_image_references, 1);
    assert_eq!(inspection.source_sha256, sha256_file(&active)?);
    assert_eq!(std::fs::read(&active)?, original_candidate);
    let manifest = MigrationManifest {
        format_version: 1,
        prepared_at_unix_ms: 1,
        thread_id: thread_id.to_string(),
        original_path: active.to_string_lossy().into_owned(),
        archive_path: archive.to_string_lossy().into_owned(),
        candidate_path: temp
            .path()
            .join("rollout.jsonl.clm-new")
            .to_string_lossy()
            .into_owned(),
        rollback_path: rollback.to_string_lossy().into_owned(),
        index_path: index.to_string_lossy().into_owned(),
        source_bytes,
        candidate_bytes,
        source_sha256,
        candidate_sha256,
        oracle_version: "fixture".to_string(),
        full_turns: 1,
        active_tail_turns: 1,
        compact_image_policy: None,
    };
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;

    let (plan_path, plan) =
        prepare_compact_image_externalization(&manifest_path, None, runtime_root, true)?;
    assert_eq!(plan.format_version, 2);
    assert!(plan.transaction_id.starts_with("tx-"));
    assert_eq!(plan.compacted_records, 1);
    assert_eq!(plan.image_occurrences, 2);
    assert_eq!(plan.unique_images, 1);
    assert_eq!(plan.occurrences.len(), 2);
    assert_eq!(plan.occurrences[0].occurrence_ordinal, 1);
    assert_eq!(plan.occurrences[0].jsonl_record_ordinal, 2);
    assert_eq!(
        plan.occurrences[0].json_pointer,
        "/payload/replacement_history/0/content/1"
    );
    assert_eq!(
        plan.occurrences[1].json_pointer,
        "/payload/replacement_history/0/content/2"
    );
    assert!(
        std::path::Path::new(&plan.final_attachment_directory).ends_with(
            std::path::Path::new("attachments")
                .join("compact-images")
                .join(&plan.transaction_id)
        )
    );
    assert!(plan.prepared_candidate_bytes < plan.source_candidate_bytes / 4);
    let prepared = std::fs::read_to_string(&plan.prepared_candidate_path)?;
    assert!(!prepared.contains("data:image/png;base64"));
    assert!(prepared.contains("Historical image externalized by CLM"));
    assert_eq!(std::fs::read(&active)?, original_candidate);

    let report = apply_compact_image_externalization(&plan_path, true)?;
    assert_eq!(report.image_occurrences, 2);
    assert_eq!(report.unique_images, 1);
    assert!(report.active_bytes_after < report.active_bytes_before / 4);
    let active_manifest: MigrationManifest = serde_json::from_reader(File::open(&manifest_path)?)?;
    assert_eq!(
        active_manifest.compact_image_policy.as_deref(),
        Some("exact_archive_with_model_reference_v1")
    );
    assert_eq!(
        std::fs::read(&report.previous_active_path)?,
        original_candidate
    );
    let image_archive =
        verify_compact_image_archive(std::path::Path::new(&report.archive_manifest_path))?;
    assert_eq!(image_archive.format_version, 2);
    assert_eq!(image_archive.transaction_id, plan.transaction_id);
    assert_eq!(image_archive.attachments.len(), 1);
    assert_eq!(image_archive.attachments[0].occurrences, 2);
    assert_eq!(image_archive.occurrences, plan.occurrences);
    assert_eq!(
        sha256_file(std::path::Path::new(&image_archive.previous_active_path))?,
        image_archive.previous_active_sha256
    );

    let archive_manifest_path = std::path::Path::new(&report.archive_manifest_path);
    let archive_manifest_bytes = std::fs::read(archive_manifest_path)?;
    let mut damaged_manifest: serde_json::Value = serde_json::from_slice(&archive_manifest_bytes)?;
    damaged_manifest["occurrences"][0]["jsonPointer"] =
        serde_json::Value::String("/payload/replacement_history/0/content/0".to_string());
    std::fs::write(
        archive_manifest_path,
        serde_json::to_vec_pretty(&damaged_manifest)?,
    )?;
    assert!(verify_compact_image_archive(archive_manifest_path).is_err());
    std::fs::write(archive_manifest_path, archive_manifest_bytes)?;
    verify_compact_image_archive(archive_manifest_path)?;

    let restored = rehydrate_migration(&manifest_path, true)?;
    assert_eq!(restored.appended_bytes, 0);
    assert_eq!(std::fs::read(&active)?, original_history);
    assert_eq!(sha256_file(&active)?, sha256_file(&archive)?);
    Ok(())
}

fn write_jsonl(path: &std::path::Path, values: &[&serde_json::Value]) -> Result<()> {
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    for value in values {
        serde_json::to_writer(&mut writer, value)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(())
}
