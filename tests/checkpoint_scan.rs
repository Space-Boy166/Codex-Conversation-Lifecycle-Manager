use anyhow::Result;
use conversation_lifecycle_manager::FixtureOptions;
use conversation_lifecycle_manager::generate_fixture;
use conversation_lifecycle_manager::scan_native_checkpoints;
use tempfile::tempdir;

#[test]
fn checkpoint_scan_finds_only_native_replacement_history_records() -> Result<()> {
    let temp = tempdir()?;
    let rollout = temp.path().join("rollout.jsonl");
    generate_fixture(
        &rollout,
        &FixtureOptions {
            turns: 20,
            tail_after_checkpoint: 3,
            payload_bytes: 32,
            history_mode: "legacy".to_string(),
        },
    )?;
    let scan = scan_native_checkpoints(&rollout)?;
    assert_eq!(scan.checkpoint_count, 1);
    assert!(scan.latest_checkpoint_offset.is_some());
    Ok(())
}
