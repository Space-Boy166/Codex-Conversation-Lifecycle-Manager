use std::collections::HashMap;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

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
