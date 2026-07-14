use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ItemsView {
    NotLoaded,
    Summary,
    Full,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct IndexedItem {
    pub turn_id: String,
    pub item_id: String,
    pub ordinal: u64,
    pub item_type: String,
    pub created_at_ms: Option<i64>,
    pub item: Value,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct IndexedTurn {
    pub turn_id: String,
    pub ordinal: u64,
    pub status: String,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub duration_ms: Option<i64>,
    pub error: Option<Value>,
    pub items: Vec<IndexedItem>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct HistoryPage<T> {
    pub data: Vec<T>,
    pub next_cursor: Option<String>,
    pub backwards_cursor: Option<String>,
    pub rows_materialized: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct IndexReport {
    pub source_path: String,
    pub source_length: u64,
    pub start_offset: u64,
    pub next_offset: u64,
    pub bytes_scanned: u64,
    pub lines_indexed: u64,
    pub records_total: u64,
    pub turns_total: u64,
    pub items_total: u64,
    pub thread_id: Option<String>,
    pub history_mode: Option<String>,
    pub lazy_turn_projection_ready: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ResumeWindow {
    pub source_path: String,
    pub start_offset: u64,
    pub bytes_read: u64,
    pub records_read: usize,
    pub full_scan_required: bool,
    pub records: Vec<Value>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ApiTurnsPage {
    pub data: Vec<Value>,
    pub next_cursor: Option<String>,
    pub backwards_cursor: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ApiProjectionReport {
    pub thread_id: String,
    pub source_path: String,
    pub source_sha256: String,
    pub oracle_version: String,
    pub turns_total: u64,
    pub active_tail_turns: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResumeSlice {
    pub source_path: String,
    pub checkpoint_offset: u64,
    pub indexed_end_offset: u64,
    pub full_scan_required: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActiveCandidateReport {
    pub thread_id: String,
    pub source_path: String,
    pub candidate_path: String,
    pub source_bytes: u64,
    pub candidate_bytes: u64,
    pub checkpoint_offset: u64,
    pub source_sha256: String,
    pub candidate_sha256: String,
}
