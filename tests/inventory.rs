use anyhow::Result;
use conversation_lifecycle_manager::ConversationLifecycleState;
use conversation_lifecycle_manager::FixtureOptions;
use conversation_lifecycle_manager::generate_fixture;
use conversation_lifecycle_manager::scan_active_user_conversations;
use conversation_lifecycle_manager::scan_codex_conversations;
use rusqlite::Connection;
use tempfile::tempdir;

#[test]
fn inventory_discovers_titles_and_filters_small_rollouts() -> Result<()> {
    let temp = tempdir()?;
    let codex_home = temp.path().join(".codex");
    let sessions = codex_home
        .join("sessions")
        .join("2026")
        .join("07")
        .join("14");
    let runtime = temp.path().join("runtime");
    std::fs::create_dir_all(&sessions)?;
    let large = sessions.join("rollout-large.jsonl");
    let small = sessions.join("rollout-small.jsonl");
    generate_fixture(
        &large,
        &FixtureOptions {
            turns: 10,
            tail_after_checkpoint: 2,
            payload_bytes: 64,
            history_mode: "paginated".to_string(),
        },
    )?;
    generate_fixture(
        &small,
        &FixtureOptions {
            turns: 1,
            tail_after_checkpoint: 1,
            payload_bytes: 8,
            history_mode: "paginated".to_string(),
        },
    )?;
    std::fs::write(
        codex_home.join("session_index.jsonl"),
        concat!(
            "{\"id\":\"00000000-0000-7000-8000-000000000001\",",
            "\"thread_name\":\"Large conversation\",",
            "\"updated_at\":\"2026-07-14T00:00:00Z\"}\n"
        ),
    )?;

    let threshold = std::fs::metadata(&small)?.len() + 1;
    let inventory = scan_codex_conversations(&codex_home, &runtime, threshold)?;
    assert_eq!(inventory.len(), 1);
    assert_eq!(inventory[0].title, "Large conversation");
    assert_eq!(inventory[0].state, ConversationLifecycleState::Original);
    assert!(inventory[0].manifest_path.is_none());
    Ok(())
}

#[test]
fn fleet_inventory_selects_only_active_top_level_user_tasks() -> Result<()> {
    let temp = tempdir()?;
    let codex_home = temp.path().join(".codex");
    let runtime = temp.path().join("runtime");
    let sessions = codex_home.join("sessions");
    std::fs::create_dir_all(&sessions)?;
    let visible = sessions.join("visible.jsonl");
    let legacy = sessions.join("legacy.jsonl");
    let subagent = sessions.join("subagent.jsonl");
    let archived = sessions.join("archived.jsonl");
    let archived_missing = sessions.join("archived-missing.jsonl");
    for (path, id) in [
        (&visible, "thread-visible"),
        (&legacy, "thread-legacy"),
        (&subagent, "thread-subagent"),
        (&archived, "thread-archived"),
    ] {
        std::fs::write(
            path,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"C:\\\\work\"}}}}\n"
            ),
        )?;
    }

    let database = Connection::open(codex_home.join("state_5.sqlite"))?;
    database.execute_batch(
        "CREATE TABLE threads (
             id TEXT PRIMARY KEY,
             rollout_path TEXT NOT NULL,
             title TEXT NOT NULL,
             updated_at INTEGER NOT NULL,
             cwd TEXT NOT NULL,
             archived INTEGER NOT NULL,
             archived_at INTEGER,
             source TEXT NOT NULL,
             thread_source TEXT
         );
         CREATE TABLE thread_spawn_edges (
             parent_thread_id TEXT NOT NULL,
             child_thread_id TEXT PRIMARY KEY,
             status TEXT NOT NULL
         );",
    )?;
    let insert = |id: &str,
                  path: &std::path::Path,
                  title: &str,
                  archived_value: i64,
                  archived_at: Option<i64>,
                  thread_source: Option<&str>|
     -> Result<()> {
        database.execute(
            "INSERT INTO threads
             (id, rollout_path, title, updated_at, cwd, archived, archived_at, source, thread_source)
             VALUES (?1, ?2, ?3, 1, 'C:\\work', ?4, ?5, 'vscode', ?6)",
            rusqlite::params![
                id,
                path.to_string_lossy(),
                title,
                archived_value,
                archived_at,
                thread_source
            ],
        )?;
        Ok(())
    };
    insert("thread-visible", &visible, "Visible", 0, None, Some("user"))?;
    insert("thread-legacy", &legacy, "Legacy", 0, None, None)?;
    insert(
        "thread-subagent",
        &subagent,
        "Subagent",
        0,
        None,
        Some("user"),
    )?;
    insert(
        "thread-archived",
        &archived,
        "Archived",
        1,
        Some(123),
        Some("user"),
    )?;
    insert(
        "thread-archived-missing",
        &archived_missing,
        "Archived missing",
        1,
        Some(124),
        Some("user"),
    )?;
    insert(
        "thread-missing",
        &sessions.join("missing.jsonl"),
        "Missing",
        0,
        None,
        Some("user"),
    )?;
    database.execute(
        "INSERT INTO thread_spawn_edges (parent_thread_id, child_thread_id, status)
         VALUES ('thread-visible', 'thread-subagent', 'completed')",
        [],
    )?;
    drop(database);

    let report = scan_active_user_conversations(&codex_home, &runtime, 0)?;
    assert_eq!(report.database_threads, 6);
    assert_eq!(report.spawn_children, 1);
    assert_eq!(report.active_top_level_user_threads, 3);
    assert_eq!(report.archived_top_level_user_threads, 2);
    assert_eq!(report.archived_existing_rollouts, 1);
    assert_eq!(report.archive_ledger.len(), 2);
    assert_eq!(report.archive_ledger_sha256.len(), 64);
    assert_eq!(report.missing_archived_rollouts.len(), 1);
    assert_eq!(
        report.missing_archived_rollouts[0].thread_id,
        "thread-archived-missing"
    );
    assert_eq!(report.archive_ledger[0].thread_id, "thread-archived");
    assert!(report.archive_ledger[0].rollout_exists);
    assert_eq!(report.archive_ledger[0].archived_at, Some(123));
    assert_eq!(report.existing_rollouts, 2);
    assert_eq!(report.selected_rollouts, 2);
    assert_eq!(report.missing_rollouts.len(), 1);
    let ids: Vec<_> = report
        .conversations
        .iter()
        .map(|item| item.thread_id.as_str())
        .collect();
    assert!(ids.contains(&"thread-visible"));
    assert!(ids.contains(&"thread-legacy"));
    assert!(!ids.contains(&"thread-subagent"));
    assert!(!ids.contains(&"thread-archived"));

    let repeated = scan_active_user_conversations(&codex_home, &runtime, 0)?;
    assert_eq!(repeated.archive_ledger_sha256, report.archive_ledger_sha256);

    let database = Connection::open(codex_home.join("state_5.sqlite"))?;
    database.execute(
        "UPDATE threads SET archived_at = 125 WHERE id = 'thread-archived'",
        [],
    )?;
    drop(database);
    let changed = scan_active_user_conversations(&codex_home, &runtime, 0)?;
    assert_ne!(changed.archive_ledger_sha256, report.archive_ledger_sha256);
    Ok(())
}
