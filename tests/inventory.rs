use anyhow::Result;
use conversation_lifecycle_manager::ConversationLifecycleState;
use conversation_lifecycle_manager::FixtureOptions;
use conversation_lifecycle_manager::generate_fixture;
use conversation_lifecycle_manager::scan_codex_conversations;
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
