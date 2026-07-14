use std::fs::OpenOptions;
use std::io::Write;

use anyhow::Result;
use conversation_lifecycle_manager::FixtureOptions;
use conversation_lifecycle_manager::IndexedRollout;
use conversation_lifecycle_manager::ItemsView;
use conversation_lifecycle_manager::SortDirection;
use conversation_lifecycle_manager::generate_fixture;
use serde_json::json;
use tempfile::tempdir;

#[test]
fn large_history_opens_a_bounded_latest_page() -> Result<()> {
    let temp = tempdir()?;
    let rollout = temp.path().join("large.jsonl");
    let db = temp.path().join("history.sqlite");
    generate_fixture(
        &rollout,
        &FixtureOptions {
            turns: 2_000,
            tail_after_checkpoint: 5,
            payload_bytes: 512,
            history_mode: "paginated".to_string(),
        },
    )?;

    let mut index = IndexedRollout::open(&db)?;
    let report = index.sync_rollout(&rollout)?;
    assert_eq!(report.turns_total, 2_000);
    assert_eq!(report.items_total, 4_000);

    let page = index.list_turns(20, None, SortDirection::Desc, ItemsView::Summary)?;
    assert_eq!(page.data.len(), 20);
    assert!(page.next_cursor.is_some());
    assert!(page.rows_materialized <= 60);
    assert_eq!(page.data[0].items.len(), 2);

    let second = index.list_turns(
        20,
        page.next_cursor.as_deref(),
        SortDirection::Desc,
        ItemsView::Summary,
    )?;
    assert_eq!(second.data.len(), 20);
    assert!(second.data[0].ordinal < page.data[19].ordinal);
    Ok(())
}

#[test]
fn resume_reads_only_latest_checkpoint_and_suffix() -> Result<()> {
    let temp = tempdir()?;
    let rollout = temp.path().join("compacted.jsonl");
    let db = temp.path().join("history.sqlite");
    generate_fixture(
        &rollout,
        &FixtureOptions {
            turns: 5_000,
            tail_after_checkpoint: 7,
            payload_bytes: 1_024,
            history_mode: "paginated".to_string(),
        },
    )?;

    let source_size = std::fs::metadata(&rollout)?.len();
    let mut index = IndexedRollout::open(&db)?;
    index.sync_rollout(&rollout)?;
    let window = index.load_resume_window()?;

    assert!(!window.full_scan_required);
    assert_eq!(window.records_read, 1 + 7 * 4);
    assert!(window.start_offset > 0);
    assert!(window.bytes_read < source_size / 100);
    Ok(())
}

#[test]
fn second_sync_reads_only_appended_records() -> Result<()> {
    let temp = tempdir()?;
    let rollout = temp.path().join("incremental.jsonl");
    let db = temp.path().join("history.sqlite");
    generate_fixture(
        &rollout,
        &FixtureOptions {
            turns: 10,
            tail_after_checkpoint: 2,
            payload_bytes: 32,
            history_mode: "paginated".to_string(),
        },
    )?;

    let mut index = IndexedRollout::open(&db)?;
    let first = index.sync_rollout(&rollout)?;
    let no_change = index.sync_rollout(&rollout)?;
    assert_eq!(no_change.bytes_scanned, 0);
    assert_eq!(no_change.lines_indexed, 0);

    let appended = json!({
        "timestamp": "2026-01-01T00:00:01Z",
        "ordinal": first.records_total,
        "type": "event_msg",
        "payload": {
            "type": "turn_started",
            "turn_id": "turn-appended",
            "started_at": 11
        }
    });
    let mut file = OpenOptions::new().append(true).open(&rollout)?;
    let bytes = format!("{}\n", serde_json::to_string(&appended)?);
    file.write_all(bytes.as_bytes())?;
    file.flush()?;

    let incremental = index.sync_rollout(&rollout)?;
    assert_eq!(incremental.lines_indexed, 1);
    assert_eq!(incremental.bytes_scanned, bytes.len() as u64);
    assert_eq!(incremental.turns_total, 11);
    Ok(())
}

#[test]
fn legacy_history_is_never_presented_as_complete_lazy_pages() -> Result<()> {
    let temp = tempdir()?;
    let rollout = temp.path().join("legacy.jsonl");
    let db = temp.path().join("history.sqlite");
    let line = json!({
        "timestamp": "2026-01-01T00:00:00Z",
        "type": "session_meta",
        "payload": {
            "meta": {
                "id": "00000000-0000-7000-8000-000000000002",
                "history_mode": "legacy"
            }
        }
    });
    std::fs::write(&rollout, format!("{}\n", serde_json::to_string(&line)?))?;

    let mut index = IndexedRollout::open(&db)?;
    let report = index.sync_rollout(&rollout)?;
    assert!(!report.lazy_turn_projection_ready);
    let error = index
        .list_turns(20, None, SortDirection::Desc, ItemsView::Summary)
        .expect_err("legacy paging must be rejected");
    assert!(error.to_string().contains("offline migration projector"));
    Ok(())
}

#[test]
fn resume_window_stops_at_the_committed_index_boundary() -> Result<()> {
    let temp = tempdir()?;
    let rollout = temp.path().join("growing.jsonl");
    let db = temp.path().join("history.sqlite");
    generate_fixture(
        &rollout,
        &FixtureOptions {
            turns: 20,
            tail_after_checkpoint: 3,
            payload_bytes: 32,
            history_mode: "paginated".to_string(),
        },
    )?;

    let mut index = IndexedRollout::open(&db)?;
    index.sync_rollout(&rollout)?;
    let stable = index.load_resume_window()?;

    let mut file = OpenOptions::new().append(true).open(&rollout)?;
    file.write_all(br#"{"partial":"#)?;
    file.flush()?;

    let while_growing = index.load_resume_window()?;
    assert_eq!(while_growing.bytes_read, stable.bytes_read);
    assert_eq!(while_growing.records, stable.records);
    Ok(())
}
