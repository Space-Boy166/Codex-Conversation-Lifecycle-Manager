use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::OsString;
use std::io::BufRead;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Write;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde_json::Value;
use serde_json::json;

use crate::IndexedRollout;
use crate::ItemsView;
use crate::SortDirection;
use crate::runtime::RuntimeConfig;
use crate::runtime::index_path;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug)]
struct PageRequest {
    limit: Option<u32>,
    sort_direction: SortDirection,
    items_view: ItemsView,
}

#[derive(Clone, Debug)]
struct PendingResume {
    thread_id: String,
    original_exclude_turns: bool,
    initial_page: Option<PageRequest>,
}

type PendingMap = Arc<Mutex<HashMap<String, PendingResume>>>;
type PendingTailDrainGateMap = Arc<Mutex<HashMap<String, String>>>;
type TailDrainGateMap = Arc<Mutex<HashMap<String, String>>>;
type ClientOwnedThreadSet = Arc<Mutex<HashSet<String>>>;
type SharedOutput = Arc<Mutex<BufWriter<std::io::Stdout>>>;

pub fn run_proxy(args: Vec<OsString>) -> Result<i32> {
    let config = RuntimeConfig::from_env()?;
    if !args.iter().any(|value| value == "app-server") {
        return run_passthrough_command(&config, &args);
    }
    run_app_server_proxy(&config, &args)
}

fn run_passthrough_command(config: &RuntimeConfig, args: &[OsString]) -> Result<i32> {
    let mut command = backend_command(config, args);
    let status = command
        .status()
        .with_context(|| format!("failed to launch {}", config.backend.display()))?;
    Ok(status.code().unwrap_or(1))
}

fn run_app_server_proxy(config: &RuntimeConfig, args: &[OsString]) -> Result<i32> {
    let mut child = backend_command(config, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to launch {}", config.backend.display()))?;
    let child_stdin = child.stdin.take().context("backend stdin unavailable")?;
    let child_stdout = child.stdout.take().context("backend stdout unavailable")?;
    let pending = PendingMap::default();
    let pending_tail_drain_gates = PendingTailDrainGateMap::default();
    let tail_drain_gates = TailDrainGateMap::default();
    let client_owned_threads = ClientOwnedThreadSet::default();
    let output = Arc::new(Mutex::new(BufWriter::new(std::io::stdout())));
    let output_reader = Arc::clone(&output);
    let pending_reader = Arc::clone(&pending);
    let pending_tail_drain_gates_reader = Arc::clone(&pending_tail_drain_gates);
    let tail_drain_gates_reader = Arc::clone(&tail_drain_gates);
    let client_owned_threads_reader = Arc::clone(&client_owned_threads);
    let index_root = config.index_root();
    let output_thread = thread::spawn(move || -> Result<()> {
        let reader = BufReader::new(child_stdout);
        for line in reader.lines() {
            let line = line?.trim_start_matches('\u{feff}').to_string();
            let outgoing = match serde_json::from_str::<Value>(&line) {
                Ok(message) => process_backend_message(
                    message,
                    &index_root,
                    &pending_reader,
                    &pending_tail_drain_gates_reader,
                    &tail_drain_gates_reader,
                    &client_owned_threads_reader,
                ),
                Err(_) => Ok(Some(Value::String(line.clone()))),
            };
            match outgoing {
                Ok(Some(Value::String(raw))) if raw == line => write_raw(&output_reader, &raw)?,
                Ok(Some(message)) => write_json(&output_reader, &message)?,
                Ok(None) => {}
                Err(error) => {
                    eprintln!("CLM proxy backend response error: {error:#}");
                    if let Some(id) = serde_json::from_str::<Value>(&line)
                        .ok()
                        .and_then(|value| value.get("id").cloned())
                    {
                        write_json(
                            &output_reader,
                            &jsonrpc_error(id, -32603, error.to_string()),
                        )?;
                    }
                }
            }
        }
        Ok(())
    });

    let mut backend_input = BufWriter::new(child_stdin);
    let input = std::io::stdin();
    for line in input.lock().lines() {
        let line = line?.trim_start_matches('\u{feff}').to_string();
        let message = match serde_json::from_str::<Value>(&line) {
            Ok(message) => message,
            Err(_) => {
                backend_input.write_all(line.as_bytes())?;
                backend_input.write_all(b"\n")?;
                backend_input.flush()?;
                continue;
            }
        };
        match process_client_message(
            message,
            &config.index_root(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
        ) {
            Ok(ClientRoute::Forward(message)) => {
                serde_json::to_writer(&mut backend_input, &message)?;
                backend_input.write_all(b"\n")?;
                backend_input.flush()?;
            }
            Ok(ClientRoute::Respond(message)) => write_json(&output, &message)?,
            Err(error) => {
                eprintln!("CLM proxy client request error: {error:#}");
                let id = serde_json::from_str::<Value>(&line)
                    .ok()
                    .and_then(|value| value.get("id").cloned())
                    .unwrap_or(Value::Null);
                write_json(&output, &jsonrpc_error(id, -32602, error.to_string()))?;
            }
        }
    }
    drop(backend_input);
    let status = child.wait()?;
    output_thread
        .join()
        .map_err(|_| anyhow::anyhow!("backend output thread panicked"))??;
    Ok(status.code().unwrap_or(1))
}

fn backend_command(config: &RuntimeConfig, args: &[OsString]) -> Command {
    let mut command = Command::new(&config.backend);
    command.args(args).env_remove("CODEX_CLI_PATH");
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

enum ClientRoute {
    Forward(Value),
    Respond(Value),
}

fn process_client_message(
    mut message: Value,
    index_root: &std::path::Path,
    pending: &PendingMap,
    pending_tail_drain_gates: &PendingTailDrainGateMap,
    tail_drain_gates: &TailDrainGateMap,
    client_owned_threads: &ClientOwnedThreadSet,
) -> Result<ClientRoute> {
    if let Some(thread_id) = message
        .get("params")
        .and_then(|params| params.get("threadId"))
        .and_then(Value::as_str)
    {
        client_owned_threads
            .lock()
            .expect("client-owned thread set poisoned")
            .insert(thread_id.to_string());
    }
    let method = message.get("method").and_then(Value::as_str);
    match method {
        Some("thread/turns/list") => {
            let id = message
                .get("id")
                .cloned()
                .context("turn page request has no id")?;
            let params = message
                .get("params")
                .context("turn page request has no params")?;
            let thread_id = required_string(params, "threadId")?;
            let cursor = optional_string(params, "cursor")?;
            // Desktop drains every older cursor after resume. Stop that loop once;
            // a later upward scroll retries the same cursor as one manual page.
            let block_automatic_drain = cursor.is_some_and(|cursor| {
                let mut gates = tail_drain_gates
                    .lock()
                    .expect("tail drain gate map poisoned");
                if gates.get(thread_id).map(String::as_str) == Some(cursor) {
                    gates.remove(thread_id);
                    true
                } else {
                    false
                }
            });
            if block_automatic_drain {
                return Ok(ClientRoute::Respond(jsonrpc_error(
                    id,
                    -32072,
                    "CLM stopped Codex's automatic full-history drain; scroll upward to load the next older page"
                        .to_string(),
                )));
            }
            let path = index_path(index_root, thread_id)?;
            if !path.exists() {
                return Ok(ClientRoute::Forward(message));
            }
            let index = IndexedRollout::open(&path)?;
            if !index.has_api_projection(thread_id)? {
                bail!("managed index is incomplete for thread {thread_id}");
            }
            let page_request = parse_page_request(params)?;
            let page = index.list_api_turns(
                thread_id,
                page_request.limit,
                cursor,
                page_request.sort_direction,
                page_request.items_view,
            )?;
            Ok(ClientRoute::Respond(json!({"id": id, "result": page})))
        }
        Some("thread/resume") => {
            let id = message
                .get("id")
                .cloned()
                .context("resume request has no id")?;
            let params = message
                .get_mut("params")
                .and_then(Value::as_object_mut)
                .context("resume request has no params object")?;
            let thread_id = params
                .get("threadId")
                .and_then(Value::as_str)
                .context("resume request has no threadId")?
                .to_string();
            let request_key = id_key(&id)?;
            tail_drain_gates
                .lock()
                .expect("tail drain gate map poisoned")
                .remove(&thread_id);
            let path = index_path(index_root, &thread_id)?;
            if !path.exists() {
                pending_tail_drain_gates
                    .lock()
                    .expect("pending tail drain gate map poisoned")
                    .insert(request_key, thread_id);
                return Ok(ClientRoute::Forward(message));
            }
            let index = IndexedRollout::open(&path)?;
            if !index.has_api_projection(&thread_id)? {
                bail!("managed index is incomplete for thread {thread_id}");
            }
            let initial_page = params
                .remove("initialTurnsPage")
                .filter(|value| !value.is_null())
                .map(|value| parse_page_request(&value))
                .transpose()?;
            let original_exclude_turns = params
                .get("excludeTurns")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            params.insert("excludeTurns".to_string(), Value::Bool(false));
            pending_tail_drain_gates
                .lock()
                .expect("pending tail drain gate map poisoned")
                .insert(request_key.clone(), thread_id.clone());
            pending.lock().expect("pending map poisoned").insert(
                request_key,
                PendingResume {
                    thread_id,
                    original_exclude_turns,
                    initial_page,
                },
            );
            Ok(ClientRoute::Forward(message))
        }
        Some("thread/fork") => {
            let id = message
                .get("id")
                .cloned()
                .context("fork request has no id")?;
            let params = message
                .get("params")
                .context("fork request has no params")?;
            let thread_id = required_string(params, "threadId")?;
            if managed_index(index_root, thread_id)?.is_some() {
                return Ok(ClientRoute::Respond(jsonrpc_error(
                    id,
                    -32070,
                    "managed lazy-history thread must be rehydrated before fork; fork was blocked to prevent a truncated child".to_string(),
                )));
            }
            Ok(ClientRoute::Forward(message))
        }
        Some("thread/read") => {
            let params = message
                .get("params")
                .context("thread read request has no params")?;
            let thread_id = required_string(params, "threadId")?;
            let include_turns = params
                .get("includeTurns")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if include_turns && managed_index(index_root, thread_id)?.is_some() {
                let id = message
                    .get("id")
                    .cloned()
                    .context("thread read request has no id")?;
                return Ok(ClientRoute::Respond(jsonrpc_error(
                    id,
                    -32071,
                    "managed lazy-history thread requires paginated thread/turns/list; eager full read was blocked".to_string(),
                )));
            }
            Ok(ClientRoute::Forward(message))
        }
        _ => Ok(ClientRoute::Forward(message)),
    }
}

fn managed_index(index_root: &std::path::Path, thread_id: &str) -> Result<Option<IndexedRollout>> {
    let path = index_path(index_root, thread_id)?;
    if !path.exists() {
        return Ok(None);
    }
    let index = IndexedRollout::open(&path)?;
    if !index.has_api_projection(thread_id)? {
        bail!("managed index is incomplete for thread {thread_id}");
    }
    Ok(Some(index))
}

fn process_backend_message(
    mut message: Value,
    index_root: &std::path::Path,
    pending: &PendingMap,
    pending_tail_drain_gates: &PendingTailDrainGateMap,
    tail_drain_gates: &TailDrainGateMap,
    client_owned_threads: &ClientOwnedThreadSet,
) -> Result<Option<Value>> {
    if let Some(id) = message.get("id").cloned() {
        let response_key = id_key(&id)?;
        let pending_tail_thread_id = pending_tail_drain_gates
            .lock()
            .expect("pending tail drain gate map poisoned")
            .remove(&response_key);
        if let Some(thread_id) = pending_tail_thread_id {
            let initial_cursor = message
                .get("result")
                .and_then(|result| result.get("initialTurnsPage"))
                .and_then(|page| page.get("nextCursor"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let mut gates = tail_drain_gates
                .lock()
                .expect("tail drain gate map poisoned");
            if let Some(cursor) = initial_cursor {
                gates.insert(thread_id, cursor);
            } else {
                gates.remove(&thread_id);
            }
        }
        let pending_resume = pending
            .lock()
            .expect("pending map poisoned")
            .remove(&response_key);
        if let Some(pending_resume) = pending_resume {
            if message.get("error").is_some() {
                tail_drain_gates
                    .lock()
                    .expect("tail drain gate map poisoned")
                    .remove(&pending_resume.thread_id);
                return Ok(Some(message));
            }
            let result = message
                .get_mut("result")
                .and_then(Value::as_object_mut)
                .context("managed resume response has no result object")?;
            let turns = result
                .get("thread")
                .and_then(|thread| thread.get("turns"))
                .and_then(Value::as_array)
                .cloned()
                .context("managed resume response has no thread.turns array")?;
            let path = index_path(index_root, &pending_resume.thread_id)?;
            let mut index = IndexedRollout::open(&path)?;
            index.replace_active_tail(&pending_resume.thread_id, &turns)?;
            if pending_resume.original_exclude_turns {
                result
                    .get_mut("thread")
                    .and_then(Value::as_object_mut)
                    .context("managed resume response thread is not an object")?
                    .insert("turns".to_string(), Value::Array(Vec::new()));
            }
            if let Some(page_request) = pending_resume.initial_page {
                let page = index.list_api_turns(
                    &pending_resume.thread_id,
                    page_request.limit,
                    None,
                    page_request.sort_direction,
                    page_request.items_view,
                )?;
                let mut gates = tail_drain_gates
                    .lock()
                    .expect("tail drain gate map poisoned");
                if let Some(cursor) = page.next_cursor.as_ref() {
                    gates.insert(pending_resume.thread_id.clone(), cursor.clone());
                } else {
                    gates.remove(&pending_resume.thread_id);
                }
                drop(gates);
                result.insert("initialTurnsPage".to_string(), serde_json::to_value(page)?);
            }
            return Ok(Some(message));
        }
    }

    if is_unowned_stream_notification(&message, client_owned_threads) {
        return Ok(None);
    }

    if matches!(
        message.get("method").and_then(Value::as_str),
        Some("turn/started" | "turn/completed")
    ) && let Some(params) = message.get("params")
        && let (Some(thread_id), Some(turn)) = (
            params.get("threadId").and_then(Value::as_str),
            params.get("turn"),
        )
    {
        let path = index_path(index_root, thread_id)?;
        if path.exists() {
            let mut index = IndexedRollout::open(&path)?;
            if index.has_api_projection(thread_id)? {
                index.upsert_active_turn(thread_id, turn)?;
            }
        }
    }
    Ok(Some(message))
}

fn is_unowned_stream_notification(
    message: &Value,
    client_owned_threads: &ClientOwnedThreadSet,
) -> bool {
    let method = message.get("method").and_then(Value::as_str);
    if !matches!(
        method,
        Some("turn/started" | "turn/completed" | "item/started" | "item/completed")
    ) {
        return false;
    }
    let Some(thread_id) = message
        .get("params")
        .and_then(|params| params.get("threadId"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    !client_owned_threads
        .lock()
        .expect("client-owned thread set poisoned")
        .contains(thread_id)
}

fn parse_page_request(value: &Value) -> Result<PageRequest> {
    let limit = value
        .get("limit")
        .and_then(Value::as_u64)
        .map(u32::try_from)
        .transpose()
        .context("page limit exceeds u32")?;
    let sort_direction = match value
        .get("sortDirection")
        .and_then(Value::as_str)
        .unwrap_or("desc")
    {
        "asc" => SortDirection::Asc,
        "desc" => SortDirection::Desc,
        other => bail!("unsupported sortDirection {other:?}"),
    };
    let items_view = match value
        .get("itemsView")
        .and_then(Value::as_str)
        .unwrap_or("summary")
    {
        "notLoaded" => ItemsView::NotLoaded,
        "summary" => ItemsView::Summary,
        "full" => ItemsView::Full,
        other => bail!("unsupported itemsView {other:?}"),
    };
    Ok(PageRequest {
        limit,
        sort_direction,
        items_view,
    })
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("request has no {field}"))
}

fn optional_string<'a>(value: &'a Value, field: &str) -> Result<Option<&'a str>> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => bail!("request field {field} is not a string or null"),
    }
}

fn id_key(id: &Value) -> Result<String> {
    Ok(serde_json::to_string(id)?)
}

fn jsonrpc_error(id: Value, code: i64, message: String) -> Value {
    json!({
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

fn write_json(output: &SharedOutput, value: &Value) -> Result<()> {
    let mut output = output.lock().expect("proxy output poisoned");
    serde_json::to_writer(&mut *output, value)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn write_raw(output: &SharedOutput, value: &str) -> Result<()> {
    let mut output = output.lock().expect("proxy output poisoned");
    output.write_all(value.as_bytes())?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn api_turn(index: usize) -> Value {
        json!({
            "id": format!("turn-{index}"),
            "status": "completed",
            "items": [
                {"type": "userMessage", "id": format!("user-{index}")},
                {"type": "agentMessage", "id": format!("agent-{index}")}
            ],
            "itemsView": "full"
        })
    }

    #[test]
    fn resume_is_rewritten_and_initial_page_is_injected() -> Result<()> {
        let temp = tempdir()?;
        let thread_id = "00000000-0000-7000-8000-000000000777";
        let source = temp.path().join("source.jsonl");
        std::fs::write(&source, "fixture\n")?;
        let index_path = index_path(temp.path(), thread_id)?;
        let mut index = IndexedRollout::open(&index_path)?;
        index.replace_api_projection(
            &source,
            thread_id,
            "hash",
            "codex-cli 0.144.2",
            &[api_turn(0), api_turn(1), api_turn(2)],
        )?;

        let pending = PendingMap::default();
        let pending_tail_drain_gates = PendingTailDrainGateMap::default();
        let tail_drain_gates = TailDrainGateMap::default();
        let client_owned_threads = ClientOwnedThreadSet::default();
        let request = json!({
            "method": "thread/resume",
            "id": 9,
            "params": {
                "threadId": thread_id,
                "excludeTurns": true,
                "initialTurnsPage": {"limit": 2, "sortDirection": "desc", "itemsView": "full"}
            }
        });
        let ClientRoute::Forward(rewritten) = process_client_message(
            request,
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
        )?
        else {
            panic!("resume should be forwarded")
        };
        assert_eq!(rewritten["params"]["excludeTurns"], false);
        assert!(rewritten["params"].get("initialTurnsPage").is_none());

        let backend = json!({
            "id": 9,
            "result": {
                "thread": {"id": thread_id, "turns": [api_turn(1), api_turn(2), api_turn(3)]}
            }
        });
        let response = process_backend_message(
            backend,
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
        )?
        .context("managed resume response should be forwarded")?;
        assert!(
            response["result"]["thread"]["turns"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            response["result"]["initialTurnsPage"]["data"][0]["id"],
            "turn-3"
        );
        assert_eq!(
            response["result"]["initialTurnsPage"]["data"][1]["id"],
            "turn-2"
        );
        Ok(())
    }

    #[test]
    fn eager_read_and_fork_are_blocked_for_managed_threads() -> Result<()> {
        let temp = tempdir()?;
        let thread_id = "00000000-0000-7000-8000-000000000778";
        let source = temp.path().join("source.jsonl");
        std::fs::write(&source, "fixture\n")?;
        let path = index_path(temp.path(), thread_id)?;
        let mut index = IndexedRollout::open(&path)?;
        index.replace_api_projection(
            &source,
            thread_id,
            "hash",
            "codex-cli 0.144.2",
            &[api_turn(0)],
        )?;
        let pending = PendingMap::default();
        let pending_tail_drain_gates = PendingTailDrainGateMap::default();
        let tail_drain_gates = TailDrainGateMap::default();
        let client_owned_threads = ClientOwnedThreadSet::default();

        let ClientRoute::Respond(fork) = process_client_message(
            json!({"method": "thread/fork", "id": 4, "params": {"threadId": thread_id}}),
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
        )?
        else {
            panic!("managed fork must be blocked")
        };
        assert_eq!(fork["error"]["code"], -32070);

        let ClientRoute::Respond(read) = process_client_message(
            json!({
                "method": "thread/read",
                "id": 5,
                "params": {"threadId": thread_id, "includeTurns": true}
            }),
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
        )?
        else {
            panic!("managed eager read must be blocked")
        };
        assert_eq!(read["error"]["code"], -32071);
        Ok(())
    }

    #[test]
    fn resume_stops_automatic_tail_drain_but_manual_page_still_loads() -> Result<()> {
        let temp = tempdir()?;
        let thread_id = "00000000-0000-7000-8000-000000000779";
        let source = temp.path().join("source.jsonl");
        std::fs::write(&source, "fixture\n")?;
        let path = index_path(temp.path(), thread_id)?;
        let mut index = IndexedRollout::open(&path)?;
        index.replace_api_projection(
            &source,
            thread_id,
            "hash",
            "codex-cli 0.144.2",
            &[
                api_turn(0),
                api_turn(1),
                api_turn(2),
                api_turn(3),
                api_turn(4),
                api_turn(5),
                api_turn(6),
            ],
        )?;

        let pending = PendingMap::default();
        let pending_tail_drain_gates = PendingTailDrainGateMap::default();
        let tail_drain_gates = TailDrainGateMap::default();
        let client_owned_threads = ClientOwnedThreadSet::default();
        let request = json!({
            "method": "thread/resume",
            "id": 10,
            "params": {
                "threadId": thread_id,
                "excludeTurns": true,
                "initialTurnsPage": {
                    "limit": 2,
                    "sortDirection": "desc",
                    "itemsView": "full"
                }
            }
        });
        let ClientRoute::Forward(_) = process_client_message(
            request,
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
        )?
        else {
            panic!("resume should be forwarded")
        };
        let backend = json!({
            "id": 10,
            "result": {
                "thread": {"id": thread_id, "turns": []}
            }
        });
        let response = process_backend_message(
            backend,
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
        )?
        .context("managed resume response should be forwarded")?;
        let cursor = response["result"]["initialTurnsPage"]["nextCursor"]
            .as_str()
            .context("initial page should expose an older cursor")?;

        let page_request = json!({
            "method": "thread/turns/list",
            "id": 11,
            "params": {
                "threadId": thread_id,
                "cursor": cursor,
                "limit": 2,
                "sortDirection": "desc",
                "itemsView": "full"
            }
        });
        let ClientRoute::Respond(blocked) = process_client_message(
            page_request.clone(),
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
        )?
        else {
            panic!("the first automatic tail request should be answered by the proxy")
        };
        assert_eq!(blocked["error"]["code"], -32072);

        let ClientRoute::Respond(manual_page) = process_client_message(
            page_request,
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
        )?
        else {
            panic!("the later manual page request should be answered by the proxy")
        };
        assert_eq!(manual_page["result"]["data"].as_array().unwrap().len(), 2);
        assert_eq!(manual_page["result"]["data"][0]["id"], "turn-4");
        assert_eq!(manual_page["result"]["data"][1]["id"], "turn-3");
        assert!(manual_page["result"]["nextCursor"].is_string());
        Ok(())
    }

    #[test]
    fn unmanaged_resume_stops_automatic_tail_drain_then_forwards_manual_page() -> Result<()> {
        let temp = tempdir()?;
        let thread_id = "00000000-0000-7000-8000-000000000780";
        let pending = PendingMap::default();
        let pending_tail_drain_gates = PendingTailDrainGateMap::default();
        let tail_drain_gates = TailDrainGateMap::default();
        let client_owned_threads = ClientOwnedThreadSet::default();
        let request = json!({
            "method": "thread/resume",
            "id": 20,
            "params": {
                "threadId": thread_id,
                "excludeTurns": true,
                "initialTurnsPage": {
                    "limit": 5,
                    "sortDirection": "desc",
                    "itemsView": "full"
                }
            }
        });

        let ClientRoute::Forward(forwarded_resume) = process_client_message(
            request.clone(),
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
        )?
        else {
            panic!("unmanaged resume should be forwarded")
        };
        assert_eq!(forwarded_resume, request);

        let backend = json!({
            "id": 20,
            "result": {
                "thread": {"id": thread_id, "turns": []},
                "initialTurnsPage": {
                    "data": [api_turn(9)],
                    "nextCursor": "legacy-cursor"
                }
            }
        });
        let forwarded_response = process_backend_message(
            backend.clone(),
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
        )?
        .context("unmanaged resume response should be forwarded")?;
        assert_eq!(forwarded_response, backend);

        let page_request = json!({
            "method": "thread/turns/list",
            "id": 21,
            "params": {
                "threadId": thread_id,
                "cursor": "legacy-cursor",
                "limit": 5,
                "sortDirection": "desc",
                "itemsView": "full"
            }
        });
        let ClientRoute::Respond(blocked) = process_client_message(
            page_request.clone(),
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
        )?
        else {
            panic!("the first unmanaged tail request should be blocked")
        };
        assert_eq!(blocked["error"]["code"], -32072);

        let ClientRoute::Forward(forwarded_page) = process_client_message(
            page_request.clone(),
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
        )?
        else {
            panic!("the later unmanaged page request should reach the official backend")
        };
        assert_eq!(forwarded_page, page_request);
        Ok(())
    }

    #[test]
    fn unowned_child_stream_events_are_dropped_without_touching_owned_threads() -> Result<()> {
        let temp = tempdir()?;
        let pending = PendingMap::default();
        let pending_tail_drain_gates = PendingTailDrainGateMap::default();
        let tail_drain_gates = TailDrainGateMap::default();
        let client_owned_threads = ClientOwnedThreadSet::default();
        let owned_thread = "00000000-0000-7000-8000-000000000781";
        let child_thread = "00000000-0000-7000-8000-000000000782";

        let ClientRoute::Forward(_) = process_client_message(
            json!({
                "method": "turn/start",
                "id": 30,
                "params": {"threadId": owned_thread, "input": []}
            }),
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
        )?
        else {
            panic!("turn start should be forwarded")
        };

        for method in [
            "turn/started",
            "turn/completed",
            "item/started",
            "item/completed",
        ] {
            let owned = json!({"method": method, "params": {"threadId": owned_thread}});
            assert!(
                process_backend_message(
                    owned,
                    temp.path(),
                    &pending,
                    &pending_tail_drain_gates,
                    &tail_drain_gates,
                    &client_owned_threads,
                )?
                .is_some(),
                "owned {method} notification must be forwarded"
            );

            let child = json!({"method": method, "params": {"threadId": child_thread}});
            assert!(
                process_backend_message(
                    child,
                    temp.path(),
                    &pending,
                    &pending_tail_drain_gates,
                    &tail_drain_gates,
                    &client_owned_threads,
                )?
                .is_none(),
                "unowned {method} notification must be dropped"
            );
        }

        let unrelated = json!({
            "method": "account/updated",
            "params": {"threadId": child_thread}
        });
        assert!(
            process_backend_message(
                unrelated,
                temp.path(),
                &pending,
                &pending_tail_drain_gates,
                &tail_drain_gates,
                &client_owned_threads,
            )?
            .is_some()
        );
        Ok(())
    }
}
