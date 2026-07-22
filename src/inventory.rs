use std::collections::HashMap;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use rusqlite::Connection;
use rusqlite::OpenFlags;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

use crate::MigrationManifest;
use crate::read_rollout_thread_id;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationInventoryItem {
    pub thread_id: String,
    pub title: String,
    pub updated_at: Option<String>,
    pub project_root: Option<String>,
    pub rollout_path: String,
    pub bytes: u64,
    pub active_bytes: u64,
    pub state: ConversationLifecycleState,
    pub manifest_path: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationLifecycleState {
    Original,
    Prepared,
    LazyHistoryEnabled,
    Restored,
    NeedsInspection,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MissingFleetConversation {
    pub thread_id: String,
    pub title: String,
    pub rollout_path: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveLedgerEntry {
    pub thread_id: String,
    pub title: String,
    pub updated_at: i64,
    pub archived_at: Option<i64>,
    pub rollout_path: String,
    pub cwd: String,
    pub source: String,
    pub thread_source: Option<String>,
    pub rollout_exists: bool,
    pub rollout_bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetInventoryReport {
    pub database_threads: u64,
    pub spawn_children: u64,
    pub active_top_level_user_threads: u64,
    pub archived_top_level_user_threads: u64,
    pub archived_existing_rollouts: u64,
    pub archive_ledger_sha256: String,
    pub existing_rollouts: u64,
    pub existing_total_bytes: u64,
    pub selected_rollouts: u64,
    pub selected_total_bytes: u64,
    pub missing_archived_rollouts: Vec<MissingFleetConversation>,
    pub missing_rollouts: Vec<MissingFleetConversation>,
    pub archive_ledger: Vec<ArchiveLedgerEntry>,
    pub conversations: Vec<ConversationInventoryItem>,
}

#[derive(Debug, Deserialize)]
struct SessionIndexEntry {
    id: String,
    #[serde(default)]
    thread_name: String,
    #[serde(default)]
    updated_at: Option<String>,
}

pub fn scan_codex_conversations(
    codex_home: &Path,
    runtime_root: &Path,
    minimum_bytes: u64,
) -> Result<Vec<ConversationInventoryItem>> {
    let titles = read_session_index(&codex_home.join("session_index.jsonl"))?;
    let mut rollout_paths = Vec::new();
    collect_jsonl_files(&codex_home.join("sessions"), &mut rollout_paths)?;

    let mut items = Vec::new();
    for path in rollout_paths {
        let metadata = match std::fs::metadata(&path) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if is_clm_sidecar(&path) {
            continue;
        }
        let thread_id = match read_rollout_thread_id(&path) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let session = titles.get(&thread_id);
        let project_root = read_project_root(&path).ok().flatten();
        let manifest_path = runtime_root
            .join("Data")
            .join("Vault")
            .join("Codex")
            .join(&thread_id)
            .join("manifest.json");
        let index_path = runtime_root
            .join("Data")
            .join("Indexes")
            .join(format!("{thread_id}.sqlite"));
        let (state, manifest) = classify_state(&path, &manifest_path, &index_path);
        let effective_bytes = manifest
            .as_ref()
            .map(|value| value.source_bytes)
            .unwrap_or_else(|| metadata.len());
        if effective_bytes < minimum_bytes {
            continue;
        }
        items.push(ConversationInventoryItem {
            thread_id: thread_id.clone(),
            title: session
                .map(|entry| entry.thread_name.trim())
                .filter(|title| !title.is_empty())
                .unwrap_or(&thread_id)
                .to_string(),
            updated_at: session.and_then(|entry| entry.updated_at.clone()),
            project_root,
            rollout_path: path.to_string_lossy().into_owned(),
            bytes: effective_bytes,
            active_bytes: metadata.len(),
            state,
            manifest_path: manifest
                .as_ref()
                .map(|_| manifest_path.to_string_lossy().into_owned()),
        });
    }
    items.sort_by(|left, right| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.title.cmp(&right.title))
    });
    Ok(items)
}

/// Reads Codex's authoritative thread catalog without mutating it and returns
/// only visible, top-level user tasks. Subagent children and archived tasks are
/// deliberately excluded from fleet operations.
pub fn scan_active_user_conversations(
    codex_home: &Path,
    runtime_root: &Path,
    minimum_bytes: u64,
) -> Result<FleetInventoryReport> {
    let database_path = codex_home.join("state_5.sqlite");
    if !database_path.is_file() {
        bail!(
            "Codex state database is missing: {}",
            database_path.display()
        );
    }
    let connection = Connection::open_with_flags(
        &database_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("failed to open {} read-only", database_path.display()))?;
    let database_threads = u64::try_from(connection.query_row(
        "SELECT COUNT(*) FROM threads",
        [],
        |row| row.get::<_, i64>(0),
    )?)
    .context("thread count is negative")?;
    let spawn_children = u64::try_from(connection.query_row(
        "SELECT COUNT(*) FROM thread_spawn_edges",
        [],
        |row| row.get::<_, i64>(0),
    )?)
    .context("spawn-child count is negative")?;

    let mut archive_statement = connection.prepare(
        "SELECT t.id, t.rollout_path, t.title, t.updated_at, t.archived_at,
                t.cwd, t.source, t.thread_source
         FROM threads AS t
         LEFT JOIN thread_spawn_edges AS edge ON edge.child_thread_id = t.id
         WHERE t.archived = 1
           AND (t.thread_source = 'user' OR t.thread_source IS NULL OR t.thread_source = '')
           AND edge.child_thread_id IS NULL
         ORDER BY t.id ASC",
    )?;
    let archive_rows = archive_statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, Option<String>>(7)?,
        ))
    })?;
    let mut archive_ledger = Vec::new();
    let mut archived_existing_rollouts = 0_u64;
    let mut missing_archived_rollouts = Vec::new();
    for row in archive_rows {
        let (thread_id, rollout_path, title, updated_at, archived_at, cwd, source, thread_source) =
            row?;
        let metadata = std::fs::metadata(&rollout_path)
            .ok()
            .filter(|metadata| metadata.is_file());
        if metadata.is_some() {
            archived_existing_rollouts += 1;
        } else {
            missing_archived_rollouts.push(MissingFleetConversation {
                thread_id: thread_id.clone(),
                title: title.clone(),
                rollout_path: rollout_path.clone(),
            });
        }
        archive_ledger.push(ArchiveLedgerEntry {
            thread_id,
            title,
            updated_at,
            archived_at,
            rollout_path,
            cwd,
            source,
            thread_source,
            rollout_exists: metadata.is_some(),
            rollout_bytes: metadata.map(|value| value.len()),
        });
    }
    let archived_top_level_user_threads = archive_ledger.len() as u64;
    let archive_ledger_sha256 =
        format!("{:x}", Sha256::digest(serde_json::to_vec(&archive_ledger)?));

    let mut statement = connection.prepare(
        "SELECT t.id, t.rollout_path, t.title, t.updated_at, t.cwd
         FROM threads AS t
         LEFT JOIN thread_spawn_edges AS edge ON edge.child_thread_id = t.id
         WHERE t.archived = 0
           AND (t.thread_source = 'user' OR t.thread_source IS NULL OR t.thread_source = '')
           AND edge.child_thread_id IS NULL
         ORDER BY t.updated_at DESC, t.id ASC",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;

    let mut active_top_level_user_threads = 0_u64;
    let mut existing_rollouts = 0_u64;
    let mut existing_total_bytes = 0_u64;
    let mut missing_rollouts = Vec::new();
    let mut conversations = Vec::new();
    for row in rows {
        let (thread_id, rollout_path, title, updated_at, cwd) = row?;
        active_top_level_user_threads += 1;
        let path = PathBuf::from(&rollout_path);
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => {
                missing_rollouts.push(MissingFleetConversation {
                    thread_id,
                    title,
                    rollout_path,
                });
                continue;
            }
        };
        existing_rollouts += 1;
        existing_total_bytes = existing_total_bytes.saturating_add(metadata.len());

        let manifest_path = runtime_root
            .join("Data")
            .join("Vault")
            .join("Codex")
            .join(&thread_id)
            .join("manifest.json");
        let index_path = runtime_root
            .join("Data")
            .join("Indexes")
            .join(format!("{thread_id}.sqlite"));
        let (state, manifest) = classify_state(&path, &manifest_path, &index_path);
        let effective_bytes = manifest
            .as_ref()
            .map(|value| value.source_bytes)
            .unwrap_or_else(|| metadata.len());
        if effective_bytes < minimum_bytes {
            continue;
        }
        conversations.push(ConversationInventoryItem {
            thread_id,
            title,
            updated_at: Some(updated_at.to_string()),
            project_root: Some(cwd),
            rollout_path,
            bytes: effective_bytes,
            active_bytes: metadata.len(),
            state,
            manifest_path: manifest
                .as_ref()
                .map(|_| manifest_path.to_string_lossy().into_owned()),
        });
    }
    conversations.sort_by(|left, right| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.title.cmp(&right.title))
    });
    let selected_total_bytes = conversations
        .iter()
        .fold(0_u64, |total, item| total.saturating_add(item.bytes));
    Ok(FleetInventoryReport {
        database_threads,
        spawn_children,
        active_top_level_user_threads,
        archived_top_level_user_threads,
        archived_existing_rollouts,
        archive_ledger_sha256,
        existing_rollouts,
        existing_total_bytes,
        selected_rollouts: conversations.len() as u64,
        selected_total_bytes,
        missing_archived_rollouts,
        missing_rollouts,
        archive_ledger,
        conversations,
    })
}

fn read_session_index(path: &Path) -> Result<HashMap<String, SessionIndexEntry>> {
    let mut entries = HashMap::new();
    if !path.is_file() {
        return Ok(entries);
    }
    let reader = BufReader::new(File::open(path)?);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<SessionIndexEntry>(&line) {
            entries.insert(entry.id.clone(), entry);
        }
    }
    Ok(entries)
}

fn collect_jsonl_files(root: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_jsonl_files(&path, output)?;
        } else if file_type.is_file()
            && path.extension().and_then(|value| value.to_str()) == Some("jsonl")
        {
            output.push(path);
        }
    }
    Ok(())
}

fn is_clm_sidecar(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(|name| name.contains(".clm-"))
        .unwrap_or(false)
}

fn read_project_root(path: &Path) -> Result<Option<String>> {
    let reader = BufReader::new(File::open(path)?);
    for (line_number, line) in reader.lines().take(100).enumerate() {
        let line = line?;
        let value: Value = serde_json::from_str(&line)
            .with_context(|| format!("invalid JSON at rollout line {}", line_number + 1))?;
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        return Ok(value
            .get("payload")
            .and_then(|payload| payload.get("cwd"))
            .and_then(Value::as_str)
            .map(str::to_string));
    }
    Ok(None)
}

fn classify_state(
    rollout: &Path,
    manifest_path: &Path,
    index_path: &Path,
) -> (ConversationLifecycleState, Option<MigrationManifest>) {
    if !manifest_path.is_file() {
        return (ConversationLifecycleState::Original, None);
    }
    let manifest = File::open(manifest_path)
        .ok()
        .and_then(|file| serde_json::from_reader::<_, MigrationManifest>(file).ok());
    let Some(manifest) = manifest else {
        return (ConversationLifecycleState::NeedsInspection, None);
    };
    let rollback_exists = Path::new(&manifest.rollback_path).is_file();
    let candidate_exists = Path::new(&manifest.candidate_path).is_file();
    let active_matches = paths_equal(Path::new(&manifest.original_path), rollout);
    let state = if active_matches && rollback_exists && index_path.is_file() {
        ConversationLifecycleState::LazyHistoryEnabled
    } else if active_matches && candidate_exists && !rollback_exists {
        ConversationLifecycleState::Prepared
    } else if active_matches && !index_path.exists() {
        ConversationLifecycleState::Restored
    } else {
        ConversationLifecycleState::NeedsInspection
    };
    (state, Some(manifest))
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    fn normalized(path: &Path) -> String {
        let value = path.to_string_lossy();
        value
            .strip_prefix(r"\\?\")
            .unwrap_or(&value)
            .replace('/', "\\")
            .to_ascii_lowercase()
    }
    normalized(left) == normalized(right)
}
