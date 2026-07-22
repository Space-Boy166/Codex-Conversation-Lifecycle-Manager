use std::collections::BTreeSet;
use std::collections::VecDeque;

use anyhow::Result;
use anyhow::bail;
use serde::Serialize;
use serde_json::Value;

pub const OPTIMISTIC_RESUME_ENABLED_BY_DEFAULT: bool = false;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OptimisticResumePhase {
    Resuming,
    Ready,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OptimisticResumeLimits {
    pub max_queued_turns: usize,
    pub max_queued_bytes: u64,
    pub timeout_ms: u64,
}

impl Default for OptimisticResumeLimits {
    fn default() -> Self {
        Self {
            max_queued_turns: 4,
            max_queued_bytes: 1024 * 1024,
            timeout_ms: 15_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TurnGateDisposition {
    Queued { position: usize },
    Forward { request: Value },
    Reject { reason: String },
}

#[derive(Clone, Debug)]
struct QueuedTurn {
    request: Value,
    encoded_bytes: u64,
}

/// Bounds the race created by an optimistic Resume response. It guarantees
/// FIFO and at-most-once release inside one live proxy process while the real
/// backend Resume establishes thread ownership.
pub struct OptimisticResumeGate {
    thread_id: String,
    resume_request_key: String,
    started_at_ms: u64,
    limits: OptimisticResumeLimits,
    phase: OptimisticResumePhase,
    queued_bytes: u64,
    queued_turns: VecDeque<QueuedTurn>,
    accepted_turn_ids: BTreeSet<String>,
    failure_reason: Option<String>,
}

impl OptimisticResumeGate {
    pub fn begin(
        thread_id: impl Into<String>,
        resume_request_id: &Value,
        started_at_ms: u64,
        limits: OptimisticResumeLimits,
    ) -> Result<Self> {
        let thread_id = thread_id.into();
        if thread_id.trim().is_empty() {
            bail!("optimistic Resume gate requires a thread id");
        }
        if limits.max_queued_turns == 0 || limits.max_queued_bytes == 0 || limits.timeout_ms == 0 {
            bail!("optimistic Resume limits must be non-zero");
        }
        Ok(Self {
            thread_id,
            resume_request_key: request_key(resume_request_id)?,
            started_at_ms,
            limits,
            phase: OptimisticResumePhase::Resuming,
            queued_bytes: 0,
            queued_turns: VecDeque::new(),
            accepted_turn_ids: BTreeSet::new(),
            failure_reason: None,
        })
    }

    pub fn phase(&self) -> OptimisticResumePhase {
        self.phase
    }

    pub fn queued_turns(&self) -> usize {
        self.queued_turns.len()
    }

    pub fn queued_bytes(&self) -> u64 {
        self.queued_bytes
    }

    pub fn failure_reason(&self) -> Option<&str> {
        self.failure_reason.as_deref()
    }

    pub fn matches_resume(&self, response_id: &Value) -> bool {
        request_key(response_id)
            .map(|key| key == self.resume_request_key)
            .unwrap_or(false)
    }

    pub fn timeout_ms(&self) -> u64 {
        self.limits.timeout_ms
    }

    pub fn submit_turn(&mut self, request: Value, now_ms: u64) -> Result<TurnGateDisposition> {
        if self.phase == OptimisticResumePhase::Resuming
            && now_ms.saturating_sub(self.started_at_ms) >= self.limits.timeout_ms
        {
            return Ok(TurnGateDisposition::Reject {
                reason: "Resume timed out; expire the gate before accepting another turn"
                    .to_string(),
            });
        }
        if request.get("method").and_then(Value::as_str) != Some("turn/start") {
            bail!("optimistic Resume gate accepts only turn/start requests");
        }
        let request_thread_id = request
            .get("params")
            .and_then(|params| params.get("threadId"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("turn/start request has no threadId"))?;
        if request_thread_id != self.thread_id {
            bail!("turn/start request belongs to a different thread");
        }
        let turn_id = request
            .get("id")
            .ok_or_else(|| anyhow::anyhow!("turn/start request has no JSON-RPC id"))?;
        let turn_key = request_key(turn_id)?;
        if self.accepted_turn_ids.contains(&turn_key) {
            bail!("duplicate turn/start request id");
        }

        if self.phase == OptimisticResumePhase::Failed {
            return Ok(TurnGateDisposition::Reject {
                reason: self
                    .failure_reason
                    .clone()
                    .unwrap_or_else(|| "Resume failed".to_string()),
            });
        }
        if self.phase == OptimisticResumePhase::Ready {
            self.accepted_turn_ids.insert(turn_key);
            return Ok(TurnGateDisposition::Forward { request });
        }

        let encoded_bytes = u64::try_from(serde_json::to_vec(&request)?.len())?;
        if self.queued_turns.len() >= self.limits.max_queued_turns {
            bail!("optimistic Resume turn queue is full");
        }
        if self.queued_bytes.saturating_add(encoded_bytes) > self.limits.max_queued_bytes {
            bail!("optimistic Resume turn queue byte limit would be exceeded");
        }
        self.accepted_turn_ids.insert(turn_key);
        self.queued_bytes += encoded_bytes;
        self.queued_turns.push_back(QueuedTurn {
            request,
            encoded_bytes,
        });
        Ok(TurnGateDisposition::Queued {
            position: self.queued_turns.len(),
        })
    }

    pub fn complete_resume(&mut self, response_id: &Value, now_ms: u64) -> Result<Vec<Value>> {
        self.ensure_matching_resume(response_id)?;
        if now_ms.saturating_sub(self.started_at_ms) >= self.limits.timeout_ms {
            bail!("Resume completed after the optimistic gate timeout");
        }
        self.phase = OptimisticResumePhase::Ready;
        self.failure_reason = None;
        Ok(self.drain_queued_turns())
    }

    pub fn fail_resume(
        &mut self,
        response_id: &Value,
        reason: impl Into<String>,
    ) -> Result<Vec<Value>> {
        self.ensure_matching_resume(response_id)?;
        self.phase = OptimisticResumePhase::Failed;
        self.failure_reason = Some(reason.into());
        Ok(self.drain_queued_turns())
    }

    pub fn expire(&mut self, now_ms: u64) -> Option<Vec<Value>> {
        if self.phase != OptimisticResumePhase::Resuming
            || now_ms.saturating_sub(self.started_at_ms) < self.limits.timeout_ms
        {
            return None;
        }
        self.phase = OptimisticResumePhase::Failed;
        self.failure_reason = Some("Resume timed out".to_string());
        Some(self.drain_queued_turns())
    }

    fn ensure_matching_resume(&self, response_id: &Value) -> Result<()> {
        if self.phase != OptimisticResumePhase::Resuming {
            bail!("optimistic Resume gate is no longer waiting for Resume");
        }
        if request_key(response_id)? != self.resume_request_key {
            bail!("Resume response id does not match the active gate");
        }
        Ok(())
    }

    fn drain_queued_turns(&mut self) -> Vec<Value> {
        let mut released = Vec::with_capacity(self.queued_turns.len());
        while let Some(turn) = self.queued_turns.pop_front() {
            self.queued_bytes = self.queued_bytes.saturating_sub(turn.encoded_bytes);
            released.push(turn.request);
        }
        released
    }
}

fn request_key(value: &Value) -> Result<String> {
    match value {
        Value::String(value) => Ok(format!("s:{value}")),
        Value::Number(value) => Ok(format!("n:{value}")),
        _ => bail!("JSON-RPC id must be a string or number"),
    }
}
