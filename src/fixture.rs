use std::fs::File;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use serde_json::Value;
use serde_json::json;

#[derive(Clone, Debug)]
pub struct FixtureOptions {
    pub turns: usize,
    pub tail_after_checkpoint: usize,
    pub payload_bytes: usize,
    pub history_mode: String,
}

impl Default for FixtureOptions {
    fn default() -> Self {
        Self {
            turns: 100,
            tail_after_checkpoint: 10,
            payload_bytes: 256,
            history_mode: "paginated".to_string(),
        }
    }
}

pub fn generate_fixture(path: &Path, options: &FixtureOptions) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    let mut ordinal = 0_u64;
    let thread_id = "00000000-0000-7000-8000-000000000001";

    write_line(
        &mut writer,
        &mut ordinal,
        "session_meta",
        json!({
            "session_id": thread_id,
            "id": thread_id,
            "timestamp": "2026-01-01T00:00:00Z",
            "cwd": "D:\\Fixture",
            "originator": "Conversation Lifecycle Manager fixture",
            "cli_version": "0.144.2",
            "source": "exec",
            "thread_source": "user",
            "model_provider": "openai",
            "base_instructions": null,
            "history_mode": options.history_mode
        }),
    )?;

    let tail = options.tail_after_checkpoint.min(options.turns);
    let checkpoint_before = options.turns.saturating_sub(tail);
    for turn_index in 0..options.turns {
        if turn_index == checkpoint_before {
            write_line(
                &mut writer,
                &mut ordinal,
                "compacted",
                json!({
                    "message": "fixture checkpoint",
                    "replacement_history": [
                        {"type": "message", "role": "user", "content": "fixture summary"}
                    ],
                    "window_number": 1
                }),
            )?;
        }

        let turn_id = format!("turn-{turn_index:08}");
        let user_id = format!("user-{turn_index:08}");
        let agent_id = format!("agent-{turn_index:08}");
        let payload = "x".repeat(options.payload_bytes);

        write_event(
            &mut writer,
            &mut ordinal,
            "turn_started",
            json!({"turn_id": turn_id, "started_at": turn_index as i64}),
        )?;
        write_event(
            &mut writer,
            &mut ordinal,
            "item_completed",
            json!({
                "turn_id": turn_id,
                "completed_at_ms": turn_index as i64 * 1000,
                "item": {
                    "type": "UserMessage",
                    "id": user_id,
                    "content": [{"type": "text", "text": format!("user {turn_index} {payload}")}]
                }
            }),
        )?;
        write_event(
            &mut writer,
            &mut ordinal,
            "item_completed",
            json!({
                "turn_id": turn_id,
                "completed_at_ms": turn_index as i64 * 1000 + 1,
                "item": {
                    "type": "AgentMessage",
                    "id": agent_id,
                    "text": format!("agent {turn_index} {payload}")
                }
            }),
        )?;
        write_event(
            &mut writer,
            &mut ordinal,
            "turn_complete",
            json!({
                "turn_id": turn_id,
                "started_at": turn_index as i64,
                "completed_at": turn_index as i64 + 1,
                "duration_ms": 1000,
                "error": null
            }),
        )?;
    }

    writer.flush().context("failed to flush fixture")?;
    Ok(())
}

fn write_event(
    writer: &mut impl Write,
    ordinal: &mut u64,
    event_type: &str,
    fields: Value,
) -> Result<()> {
    let mut payload = fields;
    payload
        .as_object_mut()
        .expect("fixture event fields must be an object")
        .insert("type".to_string(), Value::String(event_type.to_string()));
    write_line(writer, ordinal, "event_msg", payload)
}

fn write_line(
    writer: &mut impl Write,
    ordinal: &mut u64,
    record_type: &str,
    payload: Value,
) -> Result<()> {
    let line = json!({
        "timestamp": "2026-01-01T00:00:00Z",
        "ordinal": *ordinal,
        "type": record_type,
        "payload": payload
    });
    serde_json::to_writer(&mut *writer, &line).context("failed to serialize fixture line")?;
    writer
        .write_all(b"\n")
        .context("failed to write fixture newline")?;
    *ordinal += 1;
    Ok(())
}
