use anyhow::Result;
use conversation_lifecycle_manager::OPTIMISTIC_RESUME_ENABLED_BY_DEFAULT;
use conversation_lifecycle_manager::OptimisticResumeGate;
use conversation_lifecycle_manager::OptimisticResumeLimits;
use conversation_lifecycle_manager::OptimisticResumePhase;
use conversation_lifecycle_manager::TurnGateDisposition;
use serde_json::Value;
use serde_json::json;

const THREAD_ID: &str = "00000000-0000-7000-8000-000000000901";
const _: () = assert!(!OPTIMISTIC_RESUME_ENABLED_BY_DEFAULT);

#[test]
fn fast_enter_waits_for_resume_then_releases_once() -> Result<()> {
    let mut gate = gate(100)?;
    assert_eq!(
        gate.submit_turn(turn(1, "first"), 101)?,
        TurnGateDisposition::Queued { position: 1 }
    );
    assert_eq!(gate.queued_turns(), 1);

    let released = gate.complete_resume(&json!("resume-1"), 150)?;
    assert_eq!(released, vec![turn(1, "first")]);
    assert_eq!(gate.phase(), OptimisticResumePhase::Ready);
    assert_eq!(gate.queued_turns(), 0);
    assert_eq!(gate.queued_bytes(), 0);
    assert!(gate.complete_resume(&json!("resume-1"), 151).is_err());
    Ok(())
}

#[test]
fn queued_turns_are_released_in_fifo_order() -> Result<()> {
    let mut gate = gate(100)?;
    gate.submit_turn(turn(1, "first"), 101)?;
    gate.submit_turn(turn(2, "second"), 102)?;
    assert_eq!(
        gate.complete_resume(&json!("resume-1"), 103)?,
        vec![turn(1, "first"), turn(2, "second")]
    );
    assert_eq!(
        gate.submit_turn(turn(3, "third"), 104)?,
        TurnGateDisposition::Forward {
            request: turn(3, "third")
        }
    );
    Ok(())
}

#[test]
fn wrong_resume_and_duplicate_turn_ids_never_release_the_queue() -> Result<()> {
    let mut gate = gate(100)?;
    gate.submit_turn(turn(1, "first"), 101)?;
    assert!(gate.submit_turn(turn(1, "duplicate"), 102).is_err());
    assert!(gate.complete_resume(&json!("wrong-resume"), 103).is_err());
    assert_eq!(gate.phase(), OptimisticResumePhase::Resuming);
    assert_eq!(gate.queued_turns(), 1);
    Ok(())
}

#[test]
fn resume_failure_returns_queued_turns_without_forwarding_them() -> Result<()> {
    let mut gate = gate(100)?;
    gate.submit_turn(turn(1, "first"), 101)?;
    let retained = gate.fail_resume(&json!("resume-1"), "backend rejected Resume")?;
    assert_eq!(retained, vec![turn(1, "first")]);
    assert_eq!(gate.phase(), OptimisticResumePhase::Failed);
    assert_eq!(gate.failure_reason(), Some("backend rejected Resume"));
    assert!(matches!(
        gate.submit_turn(turn(2, "second"), 102)?,
        TurnGateDisposition::Reject { .. }
    ));
    Ok(())
}

#[test]
fn timeout_retains_the_fast_turn_for_explicit_recovery() -> Result<()> {
    let mut gate = gate(100)?;
    gate.submit_turn(turn(1, "first"), 101)?;
    assert!(gate.complete_resume(&json!("resume-1"), 15_100).is_err());
    assert_eq!(gate.queued_turns(), 1);
    assert_eq!(gate.expire(15_100), Some(vec![turn(1, "first")]));
    assert_eq!(gate.phase(), OptimisticResumePhase::Failed);
    assert_eq!(gate.queued_turns(), 0);
    Ok(())
}

#[test]
fn queue_limits_and_thread_ownership_are_fail_closed() -> Result<()> {
    let limits = OptimisticResumeLimits {
        max_queued_turns: 1,
        max_queued_bytes: 1024,
        timeout_ms: 1_000,
    };
    let mut gate = OptimisticResumeGate::begin(THREAD_ID, &json!(7), 0, limits)?;
    let wrong_thread = json!({
        "id": 1,
        "method": "turn/start",
        "params": {"threadId": "different", "input": [{"type": "text", "text": "x"}]}
    });
    assert!(gate.submit_turn(wrong_thread, 1).is_err());
    gate.submit_turn(turn(1, "first"), 2)?;
    assert!(gate.submit_turn(turn(2, "second"), 3).is_err());
    Ok(())
}

#[test]
fn byte_limit_rejects_a_turn_without_partially_queueing_it() -> Result<()> {
    let limits = OptimisticResumeLimits {
        max_queued_turns: 4,
        max_queued_bytes: 32,
        timeout_ms: 1_000,
    };
    let mut gate = OptimisticResumeGate::begin(THREAD_ID, &json!(7), 0, limits)?;
    assert!(
        gate.submit_turn(turn(1, "payload larger than limit"), 1)
            .is_err()
    );
    assert_eq!(gate.queued_turns(), 0);
    assert_eq!(gate.queued_bytes(), 0);
    Ok(())
}

fn gate(started_at_ms: u64) -> Result<OptimisticResumeGate> {
    OptimisticResumeGate::begin(
        THREAD_ID,
        &json!("resume-1"),
        started_at_ms,
        OptimisticResumeLimits::default(),
    )
}

fn turn(id: u64, text: &str) -> Value {
    json!({
        "id": id,
        "method": "turn/start",
        "params": {
            "threadId": THREAD_ID,
            "input": [{"type": "text", "text": text}]
        }
    })
}
