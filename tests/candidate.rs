use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;

use anyhow::Result;
use conversation_lifecycle_manager::FixtureOptions;
use conversation_lifecycle_manager::IndexedRollout;
use conversation_lifecycle_manager::build_active_candidate;
use conversation_lifecycle_manager::generate_fixture;
use tempfile::tempdir;

#[test]
fn candidate_preserves_exact_metadata_checkpoint_and_suffix() -> Result<()> {
    let temp = tempdir()?;
    let source = temp.path().join("source.jsonl");
    let db = temp.path().join("index.sqlite");
    let candidate = temp.path().join("source.clm-new");
    generate_fixture(
        &source,
        &FixtureOptions {
            turns: 40,
            tail_after_checkpoint: 4,
            payload_bytes: 64,
            history_mode: "paginated".to_string(),
        },
    )?;
    let mut index = IndexedRollout::open(&db)?;
    index.sync_rollout(&source)?;
    let slice = index.resume_slice()?;
    let report = build_active_candidate(&index, &candidate)?;
    assert!(report.candidate_bytes < report.source_bytes / 2);

    let source_file = std::fs::File::open(&source)?;
    let mut source_reader = BufReader::new(source_file);
    let mut metadata = Vec::new();
    source_reader.read_until(b'\n', &mut metadata)?;
    source_reader.seek(SeekFrom::Start(slice.checkpoint_offset))?;
    let mut expected = metadata;
    source_reader.read_to_end(&mut expected)?;
    assert_eq!(std::fs::read(&candidate)?, expected);
    Ok(())
}

#[test]
fn candidate_refuses_a_rollout_without_native_checkpoint() -> Result<()> {
    let temp = tempdir()?;
    let source = temp.path().join("legacy.jsonl");
    let db = temp.path().join("index.sqlite");
    let candidate = temp.path().join("legacy.clm-new");
    std::fs::write(
        &source,
        concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"00000000-0000-7000-8000-000000000500\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"turn_started\",\"turn_id\":\"turn-1\"}}\n"
        ),
    )?;
    let mut index = IndexedRollout::open(&db)?;
    index.sync_rollout(&source)?;
    let error = build_active_candidate(&index, &candidate)
        .expect_err("missing native checkpoint must block compaction");
    assert!(error.to_string().contains("no native compacted item"));
    assert!(!candidate.exists());
    Ok(())
}
