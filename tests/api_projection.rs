use anyhow::Result;
use conversation_lifecycle_manager::IndexedRollout;
use conversation_lifecycle_manager::ItemsView;
use conversation_lifecycle_manager::SortDirection;
use serde_json::Value;
use serde_json::json;
use tempfile::tempdir;

fn turn(index: usize) -> Value {
    json!({
        "id": format!("turn-{index:03}"),
        "status": "completed",
        "items": [
            {
                "type": "userMessage",
                "id": format!("user-{index:03}"),
                "content": [{"type": "text", "text": format!("question {index}")}]
            },
            {
                "type": "commandExecution",
                "id": format!("command-{index:03}"),
                "command": "echo test"
            },
            {
                "type": "agentMessage",
                "id": format!("agent-{index:03}"),
                "text": format!("answer {index}")
            }
        ],
        "itemsView": "full"
    })
}

#[test]
fn official_cursor_and_item_views_are_preserved() -> Result<()> {
    let temp = tempdir()?;
    let source = temp.path().join("source.jsonl");
    let db = temp.path().join("index.sqlite");
    std::fs::write(&source, "fixture\n")?;
    let turns: Vec<_> = (0..12).map(turn).collect();

    let mut index = IndexedRollout::open(&db)?;
    index.replace_api_projection(
        &source,
        "00000000-0000-7000-8000-000000000123",
        "abc123",
        "codex-cli 0.144.2",
        &turns,
    )?;

    let first = index.list_api_turns(
        "00000000-0000-7000-8000-000000000123",
        Some(5),
        None,
        SortDirection::Desc,
        ItemsView::Summary,
    )?;
    assert_eq!(first.data.len(), 5);
    assert_eq!(first.data[0]["id"], "turn-011");
    assert_eq!(first.data[4]["id"], "turn-007");
    assert_eq!(first.data[0]["items"].as_array().unwrap().len(), 2);
    assert_eq!(first.data[0]["itemsView"], "summary");

    let cursor: Value = serde_json::from_str(first.next_cursor.as_deref().unwrap())?;
    assert_eq!(
        cursor,
        json!({"turnId": "turn-007", "includeAnchor": false})
    );
    let second = index.list_api_turns(
        "00000000-0000-7000-8000-000000000123",
        Some(5),
        first.next_cursor.as_deref(),
        SortDirection::Desc,
        ItemsView::Full,
    )?;
    assert_eq!(second.data[0]["id"], "turn-006");
    assert_eq!(second.data[0]["items"].as_array().unwrap().len(), 3);

    let reverse = index.list_api_turns(
        "00000000-0000-7000-8000-000000000123",
        Some(2),
        second.backwards_cursor.as_deref(),
        SortDirection::Asc,
        ItemsView::NotLoaded,
    )?;
    assert_eq!(reverse.data[0]["id"], "turn-006");
    assert!(reverse.data[0]["items"].as_array().unwrap().is_empty());
    assert_eq!(reverse.data[0]["itemsView"], "notLoaded");
    Ok(())
}

#[test]
fn explicit_full_read_materializes_every_exact_api_turn() -> Result<()> {
    let temp = tempdir()?;
    let source = temp.path().join("source.jsonl");
    let db = temp.path().join("index.sqlite");
    std::fs::write(&source, "fixture\n")?;
    let thread_id = "00000000-0000-7000-8000-000000000126";
    let turns: Vec<_> = (0..205).map(turn).collect();

    let mut index = IndexedRollout::open(&db)?;
    index.replace_api_projection(
        &source,
        thread_id,
        "full-read-hash",
        "codex-cli 0.144.2",
        &turns,
    )?;

    let materialized = index.read_all_api_turns(thread_id)?;
    assert_eq!(materialized.len(), 205);
    assert_eq!(materialized[0]["id"], "turn-000");
    assert_eq!(materialized[204]["id"], "turn-204");
    assert_eq!(materialized[37]["items"].as_array().unwrap().len(), 3);
    assert_eq!(materialized, turns);
    Ok(())
}

#[test]
fn failed_projection_replacement_rolls_back_completely() -> Result<()> {
    let temp = tempdir()?;
    let source = temp.path().join("source.jsonl");
    let db = temp.path().join("index.sqlite");
    std::fs::write(&source, "fixture\n")?;
    let thread_id = "00000000-0000-7000-8000-000000000124";

    let mut index = IndexedRollout::open(&db)?;
    index.replace_api_projection(
        &source,
        thread_id,
        "before",
        "codex-cli 0.144.2",
        &[turn(0), turn(1)],
    )?;
    let error = index
        .replace_api_projection(
            &source,
            thread_id,
            "after",
            "codex-cli 0.144.2",
            &[turn(7), turn(7)],
        )
        .expect_err("duplicate turn ids must reject the replacement");
    assert!(error.to_string().contains("duplicate API turn id"));

    let report = index.api_projection_report(thread_id)?;
    assert_eq!(report.source_sha256, "before");
    assert_eq!(report.turns_total, 2);
    let page = index.list_api_turns(
        thread_id,
        Some(10),
        None,
        SortDirection::Asc,
        ItemsView::Full,
    )?;
    assert_eq!(page.data[0]["id"], "turn-000");
    assert_eq!(page.data[1]["id"], "turn-001");
    Ok(())
}

#[test]
fn active_tail_refresh_replaces_only_the_mutable_suffix() -> Result<()> {
    let temp = tempdir()?;
    let source = temp.path().join("source.jsonl");
    let db = temp.path().join("index.sqlite");
    std::fs::write(&source, "fixture\n")?;
    let thread_id = "00000000-0000-7000-8000-000000000125";
    let mut index = IndexedRollout::open(&db)?;
    let original: Vec<_> = (0..10).map(turn).collect();
    index.replace_api_projection(&source, thread_id, "hash", "codex-cli 0.144.2", &original)?;

    index.replace_active_tail(thread_id, &[turn(7), turn(8), turn(9), turn(10)])?;
    index.replace_active_tail(thread_id, &[turn(7), turn(8), turn(11)])?;
    let report = index.api_projection_report(thread_id)?;
    assert_eq!(report.turns_total, 10);
    assert_eq!(report.active_tail_turns, 3);

    let page = index.list_api_turns(
        thread_id,
        Some(20),
        None,
        SortDirection::Asc,
        ItemsView::Full,
    )?;
    let ids: Vec<_> = page
        .data
        .iter()
        .map(|value| value["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![
            "turn-000", "turn-001", "turn-002", "turn-003", "turn-004", "turn-005", "turn-006",
            "turn-007", "turn-008", "turn-011"
        ]
    );
    Ok(())
}
