use std::collections::HashSet;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use rusqlite::Transaction;
use rusqlite::params;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

use crate::ApiProjectionReport;
use crate::ApiTurnsPage;
use crate::HistoryPage;
use crate::IndexReport;
use crate::IndexedItem;
use crate::IndexedTurn;
use crate::ItemsView;
use crate::ResumeSlice;
use crate::ResumeWindow;
use crate::SortDirection;

const HEAD_HASH_LIMIT: u64 = 64 * 1024;

pub struct IndexedRollout {
    connection: Connection,
}

impl IndexedRollout {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("failed to open index {}", path.display()))?;
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )?;
        create_schema(&connection)?;
        Ok(Self { connection })
    }

    pub fn sync_rollout(&mut self, source_path: &Path) -> Result<IndexReport> {
        let canonical = std::fs::canonicalize(source_path)
            .with_context(|| format!("failed to resolve {}", source_path.display()))?;
        let canonical_text = canonical.to_string_lossy().into_owned();
        let source_length = std::fs::metadata(&canonical)?.len();
        let state = load_source_state(&self.connection)?;

        let (start_offset, mut next_ordinal, head_span, expected_head_hash) = match state {
            Some(state) => {
                if state.source_path != canonical_text {
                    bail!(
                        "index belongs to {}, not {}",
                        state.source_path,
                        canonical.display()
                    );
                }
                if source_length < state.next_offset {
                    bail!(
                        "rollout shrank from indexed offset {} to {source_length}; refusing incremental projection",
                        state.next_offset
                    );
                }
                (
                    state.next_offset,
                    state.next_ordinal,
                    state.head_span,
                    state.head_sha256,
                )
            }
            None => {
                let head_span = source_length.min(HEAD_HASH_LIMIT);
                (0, 0, head_span, hash_file_prefix(&canonical, head_span)?)
            }
        };

        let actual_head_hash = hash_file_prefix(&canonical, head_span)?;
        if actual_head_hash != expected_head_hash {
            bail!("rollout prefix changed; refusing to append to a stale index");
        }

        let mut file = File::open(&canonical)?;
        file.seek(SeekFrom::Start(start_offset))?;
        let mut reader = BufReader::new(file);
        let mut next_offset = start_offset;
        let mut lines_indexed = 0_u64;
        let transaction = self.connection.transaction()?;

        loop {
            let line_offset = next_offset;
            let mut line = Vec::new();
            let bytes_read = reader.read_until(b'\n', &mut line)?;
            if bytes_read == 0 {
                break;
            }

            let has_newline = line.last() == Some(&b'\n');
            let json_bytes = trim_line_ending(&line);
            if json_bytes.iter().all(u8::is_ascii_whitespace) {
                next_offset += bytes_read as u64;
                continue;
            }
            let value = match serde_json::from_slice::<Value>(json_bytes) {
                Ok(value) => value,
                Err(error) if !has_newline => {
                    // A live writer may expose a partial final record. Leave it for the next sync.
                    let _ = error;
                    break;
                }
                Err(error) => {
                    bail!("invalid JSONL record at byte {line_offset}: {error}");
                }
            };

            let ordinal = value
                .get("ordinal")
                .and_then(Value::as_u64)
                .unwrap_or(next_ordinal);
            if ordinal < next_ordinal {
                bail!(
                    "non-monotonic rollout ordinal {ordinal} at byte {line_offset}; expected at least {next_ordinal}"
                );
            }

            index_record(
                &transaction,
                &value,
                ordinal,
                line_offset,
                bytes_read as u64,
                json_bytes,
            )?;
            project_record(&transaction, &value, ordinal)?;

            next_ordinal = ordinal + 1;
            next_offset += bytes_read as u64;
            lines_indexed += 1;
        }

        let (thread_id, history_mode) = read_identity(&transaction)?;
        transaction.execute(
            "INSERT INTO source_state (
                 id, source_path, next_byte_offset, next_ordinal, head_span,
                 head_sha256, thread_id, history_mode
             ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                 source_path = excluded.source_path,
                 next_byte_offset = excluded.next_byte_offset,
                 next_ordinal = excluded.next_ordinal,
                 head_span = excluded.head_span,
                 head_sha256 = excluded.head_sha256,
                 thread_id = COALESCE(excluded.thread_id, source_state.thread_id),
                 history_mode = COALESCE(excluded.history_mode, source_state.history_mode)",
            params![
                canonical_text,
                to_i64(next_offset)?,
                to_i64(next_ordinal)?,
                to_i64(head_span)?,
                expected_head_hash,
                thread_id,
                history_mode,
            ],
        )?;
        transaction.commit()?;

        let (records_total, turns_total, items_total) = self.counts()?;
        let state = load_source_state(&self.connection)?.context("source state disappeared")?;
        Ok(IndexReport {
            source_path: state.source_path,
            source_length,
            start_offset,
            next_offset,
            bytes_scanned: next_offset - start_offset,
            lines_indexed,
            records_total,
            turns_total,
            items_total,
            thread_id: state.thread_id,
            lazy_turn_projection_ready: state.history_mode.as_deref() == Some("paginated"),
            history_mode: state.history_mode,
        })
    }

    pub fn list_turns(
        &self,
        page_size: usize,
        cursor: Option<&str>,
        sort_direction: SortDirection,
        items_view: ItemsView,
    ) -> Result<HistoryPage<IndexedTurn>> {
        self.ensure_paginated_projection()?;
        let page_size = page_size.clamp(1, 1_000);
        let anchor = cursor.map(parse_cursor).transpose()?;
        let comparison = match sort_direction {
            SortDirection::Asc => ">",
            SortDirection::Desc => "<",
        };
        let ordering = match sort_direction {
            SortDirection::Asc => "ASC",
            SortDirection::Desc => "DESC",
        };
        let sql = format!(
            "SELECT turn_id, rollout_ordinal, status, error_json, started_at,
                    completed_at, duration_ms, first_user_item_id, final_agent_item_id
             FROM thread_turns
             WHERE (?1 IS NULL OR rollout_ordinal {comparison} ?1)
             ORDER BY rollout_ordinal {ordering}
             LIMIT ?2"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement.query(params![anchor, (page_size + 1) as i64])?;
        let mut raw_turns = Vec::new();
        while let Some(row) = rows.next()? {
            raw_turns.push(RawTurn {
                turn_id: row.get(0)?,
                ordinal: from_i64(row.get(1)?)?,
                status: row.get(2)?,
                error_json: row.get(3)?,
                started_at: row.get(4)?,
                completed_at: row.get(5)?,
                duration_ms: row.get(6)?,
                first_user_item_id: row.get(7)?,
                final_agent_item_id: row.get(8)?,
            });
        }

        let has_more = raw_turns.len() > page_size;
        raw_turns.truncate(page_size);
        let backwards_cursor = raw_turns.first().map(|turn| make_cursor(turn.ordinal));
        let next_cursor = has_more
            .then(|| raw_turns.last().map(|turn| make_cursor(turn.ordinal)))
            .flatten();

        let mut rows_materialized = raw_turns.len();
        let mut data = Vec::with_capacity(raw_turns.len());
        for turn in raw_turns {
            let items = self.items_for_turn(&turn, items_view)?;
            rows_materialized += items.len();
            data.push(IndexedTurn {
                turn_id: turn.turn_id,
                ordinal: turn.ordinal,
                status: turn.status,
                started_at: turn.started_at,
                completed_at: turn.completed_at,
                duration_ms: turn.duration_ms,
                error: turn
                    .error_json
                    .map(|value| serde_json::from_str(&value))
                    .transpose()?,
                items,
            });
        }

        Ok(HistoryPage {
            data,
            next_cursor,
            backwards_cursor,
            rows_materialized,
        })
    }

    pub fn list_items(
        &self,
        turn_id: Option<&str>,
        page_size: usize,
        cursor: Option<&str>,
        sort_direction: SortDirection,
    ) -> Result<HistoryPage<IndexedItem>> {
        self.ensure_paginated_projection()?;
        let page_size = page_size.clamp(1, 2_000);
        let anchor = cursor.map(parse_cursor).transpose()?;
        let comparison = match sort_direction {
            SortDirection::Asc => ">",
            SortDirection::Desc => "<",
        };
        let ordering = match sort_direction {
            SortDirection::Asc => "ASC",
            SortDirection::Desc => "DESC",
        };
        let sql = format!(
            "SELECT turn_id, item_id, rollout_ordinal, item_type, created_at_ms, item_json
             FROM thread_items
             WHERE (?1 IS NULL OR turn_id = ?1)
               AND (?2 IS NULL OR rollout_ordinal {comparison} ?2)
             ORDER BY rollout_ordinal {ordering}
             LIMIT ?3"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement.query(params![turn_id, anchor, (page_size + 1) as i64])?;
        let mut data = Vec::new();
        while let Some(row) = rows.next()? {
            data.push(item_from_row(row)?);
        }
        let has_more = data.len() > page_size;
        data.truncate(page_size);
        let backwards_cursor = data.first().map(|item| make_cursor(item.ordinal));
        let next_cursor = has_more
            .then(|| data.last().map(|item| make_cursor(item.ordinal)))
            .flatten();
        let rows_materialized = data.len();
        Ok(HistoryPage {
            data,
            next_cursor,
            backwards_cursor,
            rows_materialized,
        })
    }

    pub fn load_resume_window(&self) -> Result<ResumeWindow> {
        let state = load_source_state(&self.connection)?.context("index has no source state")?;
        verify_source_identity(&state)?;
        let checkpoint = self
            .connection
            .query_row(
                "SELECT byte_offset FROM checkpoints
                 WHERE has_replacement_history = 1
                 ORDER BY rollout_ordinal DESC LIMIT 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let start_offset = checkpoint.map(from_i64).transpose()?.unwrap_or(0);
        let path = PathBuf::from(&state.source_path);
        let mut file = File::open(&path)
            .with_context(|| format!("failed to open indexed rollout {}", path.display()))?;
        file.seek(SeekFrom::Start(start_offset))?;
        let indexed_suffix_length = state
            .next_offset
            .checked_sub(start_offset)
            .context("checkpoint offset is beyond the indexed rollout boundary")?;
        let mut reader = BufReader::new(file.take(indexed_suffix_length));
        let mut records = Vec::new();
        let mut bytes_read = 0_u64;
        loop {
            let mut line = Vec::new();
            let read = reader.read_until(b'\n', &mut line)?;
            if read == 0 {
                break;
            }
            let json_bytes = trim_line_ending(&line);
            if json_bytes.iter().all(u8::is_ascii_whitespace) {
                bytes_read += read as u64;
                continue;
            }
            records.push(serde_json::from_slice(json_bytes).with_context(|| {
                format!(
                    "invalid indexed rollout record at byte {}",
                    start_offset + bytes_read
                )
            })?);
            bytes_read += read as u64;
        }
        Ok(ResumeWindow {
            source_path: state.source_path,
            start_offset,
            bytes_read,
            records_read: records.len(),
            full_scan_required: checkpoint.is_none(),
            records,
        })
    }

    pub fn resume_slice(&self) -> Result<ResumeSlice> {
        let state = load_source_state(&self.connection)?.context("index has no source state")?;
        verify_source_identity(&state)?;
        let checkpoint = self
            .connection
            .query_row(
                "SELECT byte_offset FROM checkpoints
                 WHERE has_replacement_history = 1
                 ORDER BY rollout_ordinal DESC LIMIT 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let checkpoint_offset = checkpoint.map(from_i64).transpose()?.unwrap_or(0);
        Ok(ResumeSlice {
            source_path: state.source_path,
            checkpoint_offset,
            indexed_end_offset: state.next_offset,
            full_scan_required: checkpoint.is_none(),
        })
    }

    pub fn replace_api_projection(
        &mut self,
        source_path: &Path,
        thread_id: &str,
        source_sha256: &str,
        oracle_version: &str,
        turns: &[Value],
    ) -> Result<ApiProjectionReport> {
        let canonical = std::fs::canonicalize(source_path)
            .with_context(|| format!("failed to resolve {}", source_path.display()))?;
        let prepared = prepare_api_turns(turns)?;
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM api_turns", [])?;
        transaction.execute("DELETE FROM api_projection_state", [])?;
        {
            let mut statement = transaction.prepare(
                "INSERT INTO api_turns (turn_id, turn_ordinal, turn_json, active_tail)
                 VALUES (?1, ?2, ?3, 0)",
            )?;
            for (ordinal, (turn_id, turn_json)) in prepared.iter().enumerate() {
                statement.execute(params![turn_id, to_i64(ordinal as u64)?, turn_json])?;
            }
        }
        transaction.execute(
            "INSERT INTO api_projection_state (
                 id, thread_id, source_path, source_sha256, oracle_version,
                 turns_total, active_tail_turns, ready
             ) VALUES (1, ?1, ?2, ?3, ?4, ?5, 0, 1)",
            params![
                thread_id,
                canonical.to_string_lossy(),
                source_sha256,
                oracle_version,
                to_i64(prepared.len() as u64)?,
            ],
        )?;
        let inserted = count_table(&transaction, "api_turns")?;
        if inserted != prepared.len() as u64 {
            bail!(
                "API projection count mismatch: prepared {}, inserted {inserted}",
                prepared.len()
            );
        }
        transaction.commit()?;
        self.api_projection_report(thread_id)
    }

    pub fn replace_active_tail(
        &mut self,
        thread_id: &str,
        turns: &[Value],
    ) -> Result<ApiProjectionReport> {
        self.ensure_api_projection(thread_id)?;
        let prepared = prepare_api_turns(turns)?;
        let transaction = self.connection.transaction()?;

        let existing_tail_floor = transaction.query_row(
            "SELECT MIN(turn_ordinal) FROM api_turns WHERE active_tail = 1",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        let mut matching_floor: Option<i64> = None;
        {
            let mut lookup =
                transaction.prepare("SELECT turn_ordinal FROM api_turns WHERE turn_id = ?1")?;
            for (turn_id, _) in &prepared {
                let ordinal = lookup
                    .query_row(params![turn_id], |row| row.get::<_, i64>(0))
                    .optional()?;
                matching_floor = match (matching_floor, ordinal) {
                    (Some(current), Some(value)) => Some(current.min(value)),
                    (None, Some(value)) => Some(value),
                    (current, None) => current,
                };
            }
        }
        let append_floor = transaction.query_row(
            "SELECT COALESCE(MAX(turn_ordinal) + 1, 0) FROM api_turns WHERE active_tail = 0",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let floor = matching_floor
            .or(existing_tail_floor)
            .unwrap_or(append_floor);

        transaction.execute(
            "DELETE FROM api_turns WHERE active_tail = 1 OR turn_ordinal >= ?1",
            params![floor],
        )?;
        {
            let mut insert = transaction.prepare(
                "INSERT INTO api_turns (turn_id, turn_ordinal, turn_json, active_tail)
                 VALUES (?1, ?2, ?3, 1)",
            )?;
            for (index, (turn_id, turn_json)) in prepared.iter().enumerate() {
                insert.execute(params![turn_id, floor + i64::try_from(index)?, turn_json])?;
            }
        }
        let turns_total = count_table(&transaction, "api_turns")?;
        transaction.execute(
            "UPDATE api_projection_state
             SET turns_total = ?1, active_tail_turns = ?2
             WHERE id = 1 AND thread_id = ?3",
            params![
                to_i64(turns_total)?,
                to_i64(prepared.len() as u64)?,
                thread_id
            ],
        )?;
        transaction.commit()?;
        self.api_projection_report(thread_id)
    }

    pub fn upsert_active_turn(
        &mut self,
        thread_id: &str,
        turn: &Value,
    ) -> Result<ApiProjectionReport> {
        self.ensure_api_projection(thread_id)?;
        let mut prepared = prepare_api_turns(std::slice::from_ref(turn))?;
        let (turn_id, turn_json) = prepared.pop().context("active turn disappeared")?;
        let transaction = self.connection.transaction()?;
        let existing = transaction
            .query_row(
                "SELECT turn_ordinal FROM api_turns WHERE turn_id = ?1",
                params![turn_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if let Some(ordinal) = existing {
            transaction.execute(
                "UPDATE api_turns SET turn_json = ?1, active_tail = 1
                 WHERE turn_id = ?2 AND turn_ordinal = ?3",
                params![turn_json, turn_id, ordinal],
            )?;
        } else {
            let ordinal = transaction.query_row(
                "SELECT COALESCE(MAX(turn_ordinal) + 1, 0) FROM api_turns",
                [],
                |row| row.get::<_, i64>(0),
            )?;
            transaction.execute(
                "INSERT INTO api_turns (turn_id, turn_ordinal, turn_json, active_tail)
                 VALUES (?1, ?2, ?3, 1)",
                params![turn_id, ordinal, turn_json],
            )?;
        }
        let turns_total = count_table(&transaction, "api_turns")?;
        let active_tail_turns = transaction.query_row(
            "SELECT COUNT(*) FROM api_turns WHERE active_tail = 1",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        transaction.execute(
            "UPDATE api_projection_state
             SET turns_total = ?1, active_tail_turns = ?2
             WHERE id = 1 AND thread_id = ?3",
            params![to_i64(turns_total)?, active_tail_turns, thread_id],
        )?;
        transaction.commit()?;
        self.api_projection_report(thread_id)
    }

    pub fn has_api_projection(&self, thread_id: &str) -> Result<bool> {
        Ok(self
            .connection
            .query_row(
                "SELECT ready FROM api_projection_state WHERE id = 1 AND thread_id = ?1",
                params![thread_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            == Some(1))
    }

    pub fn list_api_turns(
        &self,
        thread_id: &str,
        page_size: Option<u32>,
        cursor: Option<&str>,
        sort_direction: SortDirection,
        items_view: ItemsView,
    ) -> Result<ApiTurnsPage> {
        self.ensure_api_projection(thread_id)?;
        let page_size = page_size.unwrap_or(25).clamp(1, 100) as usize;
        let anchor = cursor.map(parse_api_cursor).transpose()?;
        let anchor_ordinal = match anchor.as_ref() {
            Some(anchor) => Some(
                self.connection
                    .query_row(
                        "SELECT turn_ordinal FROM api_turns WHERE turn_id = ?1",
                        params![anchor.turn_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()?
                    .context("invalid cursor: anchor turn is no longer present")?,
            ),
            None => None,
        };
        let comparison = match (
            sort_direction,
            anchor.as_ref().map(|value| value.include_anchor),
        ) {
            (SortDirection::Asc, Some(true)) => ">=",
            (SortDirection::Asc, Some(false)) => ">",
            (SortDirection::Desc, Some(true)) => "<=",
            (SortDirection::Desc, Some(false)) => "<",
            (_, None) => ">=",
        };
        let ordering = match sort_direction {
            SortDirection::Asc => "ASC",
            SortDirection::Desc => "DESC",
        };
        let sql = if anchor_ordinal.is_some() {
            format!(
                "SELECT turn_id, turn_json FROM api_turns
                 WHERE turn_ordinal {comparison} ?1
                 ORDER BY turn_ordinal {ordering} LIMIT ?2"
            )
        } else {
            format!(
                "SELECT turn_id, turn_json FROM api_turns
                 ORDER BY turn_ordinal {ordering} LIMIT ?1"
            )
        };
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = if let Some(anchor_ordinal) = anchor_ordinal {
            statement.query(params![anchor_ordinal, (page_size + 1) as i64])?
        } else {
            statement.query(params![(page_size + 1) as i64])?
        };
        let mut materialized = Vec::with_capacity(page_size + 1);
        while let Some(row) = rows.next()? {
            let turn_id: String = row.get(0)?;
            let turn_json: String = row.get(1)?;
            let mut turn: Value = serde_json::from_str(&turn_json)
                .with_context(|| format!("invalid stored API turn {turn_id}"))?;
            apply_api_items_view(&mut turn, items_view)?;
            materialized.push((turn_id, turn));
        }
        let has_more = materialized.len() > page_size;
        materialized.truncate(page_size);
        let backwards_cursor = materialized
            .first()
            .map(|(turn_id, _)| serialize_api_cursor(turn_id, true))
            .transpose()?;
        let next_cursor = if has_more {
            materialized
                .last()
                .map(|(turn_id, _)| serialize_api_cursor(turn_id, false))
                .transpose()?
        } else {
            None
        };
        Ok(ApiTurnsPage {
            data: materialized.into_iter().map(|(_, turn)| turn).collect(),
            next_cursor,
            backwards_cursor,
        })
    }

    pub fn api_projection_report(&self, thread_id: &str) -> Result<ApiProjectionReport> {
        self.ensure_api_projection(thread_id)?;
        self.connection
            .query_row(
                "SELECT thread_id, source_path, source_sha256, oracle_version,
                        turns_total, active_tail_turns
                 FROM api_projection_state WHERE id = 1",
                [],
                |row| {
                    Ok(ApiProjectionReport {
                        thread_id: row.get(0)?,
                        source_path: row.get(1)?,
                        source_sha256: row.get(2)?,
                        oracle_version: row.get(3)?,
                        turns_total: from_i64_sql(row.get(4)?)?,
                        active_tail_turns: from_i64_sql(row.get(5)?)?,
                    })
                },
            )
            .map_err(Into::into)
    }

    fn items_for_turn(&self, turn: &RawTurn, view: ItemsView) -> Result<Vec<IndexedItem>> {
        let (sql, first, final_agent) = match view {
            ItemsView::NotLoaded => return Ok(Vec::new()),
            ItemsView::Full => (
                "SELECT turn_id, item_id, rollout_ordinal, item_type, created_at_ms, item_json
                 FROM thread_items WHERE turn_id = ?1 ORDER BY rollout_ordinal ASC",
                None,
                None,
            ),
            ItemsView::Summary => (
                "SELECT turn_id, item_id, rollout_ordinal, item_type, created_at_ms, item_json
                 FROM thread_items
                 WHERE turn_id = ?1 AND (item_id = ?2 OR item_id = ?3)
                 ORDER BY rollout_ordinal ASC",
                turn.first_user_item_id.as_deref(),
                turn.final_agent_item_id.as_deref(),
            ),
        };
        let mut statement = self.connection.prepare(sql)?;
        let mut rows = match view {
            ItemsView::NotLoaded => unreachable!("not-loaded view returns before preparing rows"),
            ItemsView::Full => statement.query(params![turn.turn_id])?,
            ItemsView::Summary => statement.query(params![turn.turn_id, first, final_agent])?,
        };
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            let item = item_from_row(row)?;
            if !items
                .iter()
                .any(|existing: &IndexedItem| existing.item_id == item.item_id)
            {
                items.push(item);
            }
        }
        Ok(items)
    }

    fn counts(&self) -> Result<(u64, u64, u64)> {
        Ok((
            count_table(&self.connection, "rollout_records")?,
            count_table(&self.connection, "thread_turns")?,
            count_table(&self.connection, "thread_items")?,
        ))
    }

    fn ensure_paginated_projection(&self) -> Result<()> {
        let state = load_source_state(&self.connection)?.context("index has no source state")?;
        if state.history_mode.as_deref() != Some("paginated") {
            bail!(
                "lazy turn projection requires history_mode=paginated; legacy history needs the offline migration projector"
            );
        }
        Ok(())
    }

    fn ensure_api_projection(&self, thread_id: &str) -> Result<()> {
        if !self.has_api_projection(thread_id)? {
            bail!("no complete API projection for thread {thread_id}");
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiTurnCursor {
    turn_id: String,
    include_anchor: bool,
}

#[derive(Debug)]
struct SourceState {
    source_path: String,
    next_offset: u64,
    next_ordinal: u64,
    head_span: u64,
    head_sha256: String,
    thread_id: Option<String>,
    history_mode: Option<String>,
}

#[derive(Debug)]
struct RawTurn {
    turn_id: String,
    ordinal: u64,
    status: String,
    error_json: Option<String>,
    started_at: Option<i64>,
    completed_at: Option<i64>,
    duration_ms: Option<i64>,
    first_user_item_id: Option<String>,
    final_agent_item_id: Option<String>,
}

fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS source_state (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             source_path TEXT NOT NULL,
             next_byte_offset INTEGER NOT NULL,
             next_ordinal INTEGER NOT NULL,
             head_span INTEGER NOT NULL,
             head_sha256 TEXT NOT NULL,
             thread_id TEXT,
             history_mode TEXT
         );
         CREATE TABLE IF NOT EXISTS rollout_records (
             rollout_ordinal INTEGER PRIMARY KEY,
             byte_offset INTEGER NOT NULL,
             byte_length INTEGER NOT NULL,
             timestamp TEXT,
             record_type TEXT NOT NULL,
             payload_type TEXT,
             turn_id TEXT,
             item_id TEXT,
             line_sha256 TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_rollout_records_turn
             ON rollout_records(turn_id, rollout_ordinal);
         CREATE TABLE IF NOT EXISTS checkpoints (
             rollout_ordinal INTEGER PRIMARY KEY,
             byte_offset INTEGER NOT NULL,
             byte_length INTEGER NOT NULL,
             has_replacement_history INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS thread_turns (
             turn_id TEXT PRIMARY KEY,
             rollout_ordinal INTEGER NOT NULL UNIQUE,
             status TEXT NOT NULL,
             error_json TEXT,
             started_at INTEGER,
             completed_at INTEGER,
             duration_ms INTEGER,
             first_user_item_id TEXT,
             final_agent_item_id TEXT
         );
         CREATE TABLE IF NOT EXISTS thread_items (
             turn_id TEXT NOT NULL,
             item_id TEXT NOT NULL,
             rollout_ordinal INTEGER NOT NULL UNIQUE,
             item_type TEXT NOT NULL,
             created_at_ms INTEGER,
             item_json TEXT NOT NULL,
             PRIMARY KEY (turn_id, item_id)
         );
         CREATE INDEX IF NOT EXISTS idx_thread_items_turn_page
             ON thread_items(turn_id, rollout_ordinal);
         CREATE TABLE IF NOT EXISTS api_projection_state (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             thread_id TEXT NOT NULL,
             source_path TEXT NOT NULL,
             source_sha256 TEXT NOT NULL,
             oracle_version TEXT NOT NULL,
             turns_total INTEGER NOT NULL,
             active_tail_turns INTEGER NOT NULL,
             ready INTEGER NOT NULL CHECK (ready IN (0, 1))
         );
         CREATE TABLE IF NOT EXISTS api_turns (
             turn_id TEXT PRIMARY KEY,
             turn_ordinal INTEGER NOT NULL UNIQUE,
             turn_json TEXT NOT NULL,
             active_tail INTEGER NOT NULL CHECK (active_tail IN (0, 1))
         );
         CREATE INDEX IF NOT EXISTS idx_api_turns_page
             ON api_turns(turn_ordinal);
         PRAGMA user_version = 2;",
    )?;
    Ok(())
}

fn load_source_state(connection: &Connection) -> Result<Option<SourceState>> {
    connection
        .query_row(
            "SELECT source_path, next_byte_offset, next_ordinal, head_span,
                    head_sha256, thread_id, history_mode
             FROM source_state WHERE id = 1",
            [],
            |row| {
                Ok(SourceState {
                    source_path: row.get(0)?,
                    next_offset: from_i64_sql(row.get(1)?)?,
                    next_ordinal: from_i64_sql(row.get(2)?)?,
                    head_span: from_i64_sql(row.get(3)?)?,
                    head_sha256: row.get(4)?,
                    thread_id: row.get(5)?,
                    history_mode: row.get(6)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn index_record(
    transaction: &Transaction<'_>,
    value: &Value,
    ordinal: u64,
    byte_offset: u64,
    byte_length: u64,
    line: &[u8],
) -> Result<()> {
    let record_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let payload = value.get("payload");
    let payload_type = payload
        .and_then(|payload| payload.get("type"))
        .and_then(Value::as_str);
    let turn_id = payload
        .and_then(|payload| payload.get("turn_id"))
        .and_then(Value::as_str);
    let item_id = payload
        .and_then(|payload| payload.get("item"))
        .and_then(|item| item.get("id"))
        .and_then(Value::as_str);
    transaction.execute(
        "INSERT INTO rollout_records (
             rollout_ordinal, byte_offset, byte_length, timestamp, record_type,
             payload_type, turn_id, item_id, line_sha256
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            to_i64(ordinal)?,
            to_i64(byte_offset)?,
            to_i64(byte_length)?,
            value.get("timestamp").and_then(Value::as_str),
            record_type,
            payload_type,
            turn_id,
            item_id,
            format!("{:x}", Sha256::digest(line)),
        ],
    )?;

    if record_type == "compacted" {
        let has_replacement_history = payload
            .and_then(|payload| payload.get("replacement_history"))
            .is_some_and(Value::is_array);
        transaction.execute(
            "INSERT INTO checkpoints (
                 rollout_ordinal, byte_offset, byte_length, has_replacement_history
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                to_i64(ordinal)?,
                to_i64(byte_offset)?,
                to_i64(byte_length)?,
                has_replacement_history,
            ],
        )?;
    }
    Ok(())
}

fn project_record(transaction: &Transaction<'_>, value: &Value, ordinal: u64) -> Result<()> {
    let record_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let payload = value.get("payload").unwrap_or(&Value::Null);

    if record_type == "session_meta" {
        let meta = payload.get("meta").unwrap_or(payload);
        if let Some(thread_id) = meta.get("id").and_then(Value::as_str) {
            transaction.execute(
                "INSERT INTO source_state (
                     id, source_path, next_byte_offset, next_ordinal, head_span,
                     head_sha256, thread_id, history_mode
                 ) VALUES (1, '', 0, 0, 0, '', ?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET
                     thread_id = COALESCE(source_state.thread_id, excluded.thread_id),
                     history_mode = COALESCE(source_state.history_mode, excluded.history_mode)",
                params![thread_id, meta.get("history_mode").and_then(Value::as_str)],
            )?;
        }
        return Ok(());
    }

    if record_type != "event_msg" {
        return Ok(());
    }

    let event_type = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let turn_id = payload.get("turn_id").and_then(Value::as_str);
    match event_type {
        "turn_started" => {
            if let Some(turn_id) = turn_id {
                upsert_turn(
                    transaction,
                    turn_id,
                    ordinal,
                    "in_progress",
                    None,
                    payload.get("started_at").and_then(Value::as_i64),
                    None,
                    None,
                )?;
            }
        }
        "turn_complete" => {
            if let Some(turn_id) = turn_id {
                let error = payload.get("error").filter(|value| !value.is_null());
                upsert_turn(
                    transaction,
                    turn_id,
                    ordinal,
                    if error.is_some() {
                        "failed"
                    } else {
                        "completed"
                    },
                    error,
                    payload.get("started_at").and_then(Value::as_i64),
                    payload.get("completed_at").and_then(Value::as_i64),
                    payload.get("duration_ms").and_then(Value::as_i64),
                )?;
            }
        }
        "turn_aborted" => {
            if let Some(turn_id) = turn_id {
                upsert_turn(
                    transaction,
                    turn_id,
                    ordinal,
                    "interrupted",
                    None,
                    payload.get("started_at").and_then(Value::as_i64),
                    payload.get("completed_at").and_then(Value::as_i64),
                    payload.get("duration_ms").and_then(Value::as_i64),
                )?;
            }
        }
        "item_completed" => {
            if let (Some(turn_id), Some(item)) = (turn_id, payload.get("item")) {
                upsert_turn(
                    transaction,
                    turn_id,
                    ordinal,
                    "in_progress",
                    None,
                    None,
                    None,
                    None,
                )?;
                upsert_item(transaction, turn_id, ordinal, payload, item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn upsert_turn(
    transaction: &Transaction<'_>,
    turn_id: &str,
    ordinal: u64,
    status: &str,
    error: Option<&Value>,
    started_at: Option<i64>,
    completed_at: Option<i64>,
    duration_ms: Option<i64>,
) -> Result<()> {
    let error_json = error.map(serde_json::to_string).transpose()?;
    transaction.execute(
        "INSERT INTO thread_turns (
             turn_id, rollout_ordinal, status, error_json, started_at,
             completed_at, duration_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(turn_id) DO UPDATE SET
             rollout_ordinal = MIN(thread_turns.rollout_ordinal, excluded.rollout_ordinal),
             status = CASE
                 WHEN excluded.status = 'in_progress' AND thread_turns.status != 'in_progress'
                 THEN thread_turns.status ELSE excluded.status END,
             error_json = COALESCE(excluded.error_json, thread_turns.error_json),
             started_at = COALESCE(thread_turns.started_at, excluded.started_at),
             completed_at = COALESCE(excluded.completed_at, thread_turns.completed_at),
             duration_ms = COALESCE(excluded.duration_ms, thread_turns.duration_ms)",
        params![
            turn_id,
            to_i64(ordinal)?,
            status,
            error_json,
            started_at,
            completed_at,
            duration_ms,
        ],
    )?;
    Ok(())
}

fn upsert_item(
    transaction: &Transaction<'_>,
    turn_id: &str,
    ordinal: u64,
    event: &Value,
    item: &Value,
) -> Result<()> {
    let item_type = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("Unknown");
    let item_id = item
        .get("id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("{turn_id}:{ordinal}"));
    transaction.execute(
        "INSERT INTO thread_items (
             turn_id, item_id, rollout_ordinal, item_type, created_at_ms, item_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(turn_id, item_id) DO UPDATE SET
             rollout_ordinal = excluded.rollout_ordinal,
             item_type = excluded.item_type,
             created_at_ms = excluded.created_at_ms,
             item_json = excluded.item_json",
        params![
            turn_id,
            item_id,
            to_i64(ordinal)?,
            item_type,
            event.get("completed_at_ms").and_then(Value::as_i64),
            serde_json::to_string(item)?,
        ],
    )?;

    let normalized = item_type
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    if normalized == "usermessage" {
        transaction.execute(
            "UPDATE thread_turns
             SET first_user_item_id = COALESCE(first_user_item_id, ?2)
             WHERE turn_id = ?1",
            params![turn_id, item_id],
        )?;
    } else if normalized == "agentmessage" {
        transaction.execute(
            "UPDATE thread_turns SET final_agent_item_id = ?2 WHERE turn_id = ?1",
            params![turn_id, item_id],
        )?;
    }
    Ok(())
}

fn read_identity(transaction: &Transaction<'_>) -> Result<(Option<String>, Option<String>)> {
    Ok(transaction
        .query_row(
            "SELECT thread_id, history_mode FROM source_state WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .unwrap_or_default())
}

fn prepare_api_turns(turns: &[Value]) -> Result<Vec<(String, String)>> {
    let mut seen = HashSet::with_capacity(turns.len());
    let mut prepared = Vec::with_capacity(turns.len());
    for (index, turn) in turns.iter().enumerate() {
        let object = turn
            .as_object()
            .with_context(|| format!("API turn {index} is not an object"))?;
        let turn_id = object
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .with_context(|| format!("API turn {index} has no non-empty id"))?;
        if !seen.insert(turn_id.to_string()) {
            bail!("duplicate API turn id {turn_id}");
        }
        if !object.get("items").is_some_and(Value::is_array) {
            bail!("API turn {turn_id} has no items array");
        }
        prepared.push((turn_id.to_string(), serde_json::to_string(turn)?));
    }
    Ok(prepared)
}

fn apply_api_items_view(turn: &mut Value, items_view: ItemsView) -> Result<()> {
    let object = turn
        .as_object_mut()
        .context("stored API turn is not an object")?;
    let items = object
        .get_mut("items")
        .and_then(Value::as_array_mut)
        .context("stored API turn has no items array")?;
    match items_view {
        ItemsView::NotLoaded => items.clear(),
        ItemsView::Summary => {
            let first_user = items
                .iter()
                .find(|item| api_item_kind(item) == "usermessage")
                .cloned();
            let final_agent = items
                .iter()
                .rev()
                .find(|item| api_item_kind(item) == "agentmessage")
                .cloned();
            *items = match (first_user, final_agent) {
                (Some(user), Some(agent)) if api_item_id(&user) != api_item_id(&agent) => {
                    vec![user, agent]
                }
                (Some(user), _) => vec![user],
                (None, Some(agent)) => vec![agent],
                (None, None) => Vec::new(),
            };
        }
        ItemsView::Full => {}
    }
    object.insert(
        "itemsView".to_string(),
        Value::String(
            match items_view {
                ItemsView::NotLoaded => "notLoaded",
                ItemsView::Summary => "summary",
                ItemsView::Full => "full",
            }
            .to_string(),
        ),
    );
    Ok(())
}

fn api_item_kind(item: &Value) -> String {
    item.get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn api_item_id(item: &Value) -> Option<&str> {
    item.get("id").and_then(Value::as_str)
}

fn serialize_api_cursor(turn_id: &str, include_anchor: bool) -> Result<String> {
    Ok(serde_json::to_string(&ApiTurnCursor {
        turn_id: turn_id.to_string(),
        include_anchor,
    })?)
}

fn parse_api_cursor(cursor: &str) -> Result<ApiTurnCursor> {
    serde_json::from_str(cursor).with_context(|| format!("invalid cursor: {cursor}"))
}

fn item_from_row(row: &rusqlite::Row<'_>) -> Result<IndexedItem> {
    let ordinal: i64 = row.get(2)?;
    let item_json: String = row.get(5)?;
    Ok(IndexedItem {
        turn_id: row.get(0)?,
        item_id: row.get(1)?,
        ordinal: from_i64(ordinal)?,
        item_type: row.get(3)?,
        created_at_ms: row.get(4)?,
        item: serde_json::from_str(&item_json)?,
    })
}

fn count_table(connection: &Connection, table: &str) -> Result<u64> {
    let count = connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get::<_, i64>(0)
    })?;
    from_i64(count)
}

fn trim_line_ending(mut line: &[u8]) -> &[u8] {
    if let Some(stripped) = line.strip_suffix(b"\n") {
        line = stripped;
    }
    if let Some(stripped) = line.strip_suffix(b"\r") {
        line = stripped;
    }
    line
}

fn hash_file_prefix(path: &Path, span: u64) -> Result<String> {
    let mut file = File::open(path)?;
    let mut bytes = vec![0_u8; usize::try_from(span)?];
    file.read_exact(&mut bytes)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn verify_source_identity(state: &SourceState) -> Result<()> {
    let path = Path::new(&state.source_path);
    let source_length = std::fs::metadata(path)
        .with_context(|| format!("failed to inspect indexed rollout {}", path.display()))?
        .len();
    if source_length < state.next_offset {
        bail!(
            "indexed rollout shrank below byte {}; refusing resume",
            state.next_offset
        );
    }
    if hash_file_prefix(path, state.head_span)? != state.head_sha256 {
        bail!("indexed rollout prefix changed; refusing resume from a stale index");
    }
    Ok(())
}

fn make_cursor(ordinal: u64) -> String {
    format!("v1:{ordinal}")
}

fn parse_cursor(cursor: &str) -> Result<i64> {
    let value = cursor
        .strip_prefix("v1:")
        .context("unsupported cursor version")?
        .parse::<u64>()
        .context("invalid cursor ordinal")?;
    to_i64(value)
}

fn to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).context("value exceeds SQLite INTEGER range")
}

fn from_i64(value: i64) -> Result<u64> {
    u64::try_from(value).context("negative value in unsigned SQLite field")
}

fn from_i64_sql(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}
