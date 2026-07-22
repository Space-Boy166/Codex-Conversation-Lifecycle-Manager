use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

use crate::IndexedRollout;
use crate::ItemsView;
use crate::OptimisticResumeGate;
use crate::OptimisticResumeLimits;
use crate::SortDirection;
use crate::TurnGateDisposition;
use crate::read_rollout_thread_id;
use crate::runtime::OptimisticResumeRuntimePolicy;
use crate::runtime::RuntimeConfig;
use crate::runtime::default_codex_home;
use crate::runtime::index_path;
use crate::runtime::runtime_root_from_env;
use crate::runtime::validate_optimistic_resume_policy;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

#[cfg(windows)]
use std::process::Child;

#[cfg(windows)]
use windows_sys::Win32::Foundation::CloseHandle;

#[cfg(windows)]
use windows_sys::Win32::Foundation::HANDLE;

#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::CreateJobObjectW;

#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::JOBOBJECT_EXTENDED_LIMIT_INFORMATION;

#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::JobObjectExtendedLimitInformation;

#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::SetInformationJobObject;

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
    optimistic_policy_enabled: bool,
    optimistic_cache_status: &'static str,
    path_provided_by_client: bool,
    path_injected_by_clm: bool,
    started_at: Instant,
    started_unix_ms: u128,
    timing_log_path: Option<PathBuf>,
    cache_fingerprint: String,
    optimistic: bool,
    client_response_ms: Option<u128>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ResumeTimingRecord<'a> {
    format_version: u32,
    thread_id: &'a str,
    request_id: &'a str,
    started_unix_ms: u128,
    finished_unix_ms: u128,
    elapsed_ms: u128,
    backend_response_ms: u128,
    postprocess_ms: u128,
    status: &'a str,
    backend_turns: Option<usize>,
    original_exclude_turns: bool,
    initial_page_present: bool,
    optimistic_policy_enabled: bool,
    optimistic_cache_status: &'a str,
    path_provided_by_client: bool,
    path_injected_by_clm: bool,
    optimistic: bool,
    client_response_ms: Option<u128>,
}

#[derive(Clone, Debug)]
struct ResumeCacheContext {
    cache_root: PathBuf,
    environment_fingerprint: String,
    policy: OptimisticResumeRuntimePolicy,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ResumeCacheEnvelope {
    format_version: u32,
    thread_id: String,
    environment_fingerprint: String,
    request_fingerprint: String,
    cached_unix_ms: u128,
    result: Value,
}

#[derive(Debug)]
enum ResumeCacheLookup {
    Hit(Value),
    Miss(&'static str),
}

impl ResumeCacheLookup {
    fn status(&self) -> &'static str {
        match self {
            Self::Hit(_) => "hit",
            Self::Miss(status) => status,
        }
    }

    fn into_result(self) -> Option<Value> {
        match self {
            Self::Hit(result) => Some(result),
            Self::Miss(_) => None,
        }
    }
}

#[derive(Clone, Debug)]
struct OptimisticTimeout {
    thread_id: String,
    resume_id: Value,
    timeout_ms: u64,
}

#[derive(Clone, Debug)]
enum GateResolution {
    Ready {
        thread_id: String,
        resume_id: Value,
    },
    Failed {
        thread_id: String,
        resume_id: Value,
        reason: String,
    },
}

#[derive(Default)]
struct BackendEffects {
    gate_resolution: Option<GateResolution>,
}

#[derive(Clone, Debug)]
struct PendingEagerRead {
    thread_id: String,
}

#[derive(Clone, Debug)]
enum PendingRequest {
    Resume(PendingResume),
    EagerRead(PendingEagerRead),
}

const SKILLS_LIST_CACHE_TTL: Duration = Duration::from_secs(30);
const SKILLS_LIST_CACHE_MAX_ENTRIES: usize = 8;
const SKILLS_LIST_PENDING_TTL: Duration = Duration::from_secs(120);
const SKILLS_LIST_MAX_PENDING: usize = 64;

#[derive(Clone, Debug)]
struct CachedSkillsListResponse {
    response: Value,
    stored_at: Instant,
}

#[derive(Clone, Debug)]
struct PendingSkillsListRequest {
    cache_key: String,
    generation: u64,
    started_at: Instant,
    force_reload: bool,
    cacheable: bool,
}

#[derive(Debug, Default)]
struct SkillsListCacheState {
    entries: HashMap<String, CachedSkillsListResponse>,
    pending: HashMap<String, PendingSkillsListRequest>,
    generation: u64,
}

impl SkillsListCacheState {
    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.entries.clear();
    }

    fn begin_request(&mut self, message: &Value, now: Instant) -> Result<Option<Value>> {
        let id = message
            .get("id")
            .cloned()
            .context("skills/list request has no id")?;
        let request_key = id_key(&id)?;
        let force_reload = message
            .get("params")
            .and_then(|params| params.get("forceReload"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let cache_key = skills_list_cache_key(message)?;

        self.pending.retain(|_, pending| {
            now.saturating_duration_since(pending.started_at) <= SKILLS_LIST_PENDING_TTL
        });
        if !self.pending.contains_key(&request_key)
            && self.pending.len() >= SKILLS_LIST_MAX_PENDING
            && let Some(oldest_key) = self
                .pending
                .iter()
                .min_by_key(|(_, pending)| pending.started_at)
                .map(|(key, _)| key.clone())
        {
            self.pending.remove(&oldest_key);
        }

        let force_reload_in_flight = self
            .pending
            .values()
            .any(|pending| pending.force_reload && pending.cache_key == cache_key);

        if force_reload {
            self.invalidate();
        } else if !force_reload_in_flight && let Some(entry) = self.entries.get(&cache_key) {
            if now.duration_since(entry.stored_at) <= SKILLS_LIST_CACHE_TTL {
                let mut response = entry.response.clone();
                response
                    .as_object_mut()
                    .context("cached skills/list response is not an object")?
                    .insert("id".to_string(), id);
                return Ok(Some(response));
            }
            self.entries.remove(&cache_key);
        }

        self.pending.insert(
            request_key,
            PendingSkillsListRequest {
                cache_key,
                generation: self.generation,
                started_at: now,
                force_reload,
                cacheable: force_reload || !force_reload_in_flight,
            },
        );
        Ok(None)
    }

    fn complete_request(&mut self, response_key: &str, message: &Value, now: Instant) -> bool {
        let Some(pending) = self.pending.remove(response_key) else {
            return false;
        };
        if pending.generation != self.generation
            || !pending.cacheable
            || message.get("error").is_some()
            || message.get("result").is_none()
            || !message.is_object()
        {
            return false;
        }

        if !self.entries.contains_key(&pending.cache_key)
            && self.entries.len() >= SKILLS_LIST_CACHE_MAX_ENTRIES
            && let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.stored_at)
                .map(|(key, _)| key.clone())
        {
            self.entries.remove(&oldest_key);
        }
        self.entries.insert(
            pending.cache_key,
            CachedSkillsListResponse {
                response: message.clone(),
                stored_at: now,
            },
        );
        true
    }
}

type PendingMap = Arc<Mutex<HashMap<String, PendingRequest>>>;
type PendingTailDrainGateMap = Arc<Mutex<HashMap<String, String>>>;
type TailDrainGateMap = Arc<Mutex<HashMap<String, String>>>;
type ClientOwnedThreadSet = Arc<Mutex<HashSet<String>>>;
type SkillsListCache = Arc<Mutex<SkillsListCacheState>>;
type SharedOutput = Arc<Mutex<BufWriter<std::io::Stdout>>>;
type SharedBackendInput = Arc<Mutex<Option<Box<dyn Write + Send>>>>;
type OptimisticGateMap = Arc<Mutex<HashMap<String, OptimisticResumeGate>>>;

#[derive(Clone)]
struct ProxyState {
    index_root: PathBuf,
    pending: PendingMap,
    pending_tail_drain_gates: PendingTailDrainGateMap,
    tail_drain_gates: TailDrainGateMap,
    client_owned_threads: ClientOwnedThreadSet,
    skills_list_cache: SkillsListCache,
    resume_cache: ResumeCacheContext,
    optimistic_gates: OptimisticGateMap,
}

impl ResumeCacheContext {
    fn new(config: &RuntimeConfig, args: &[OsString]) -> Result<Self> {
        let backend = std::fs::canonicalize(&config.backend).with_context(|| {
            format!(
                "failed to resolve optimistic Resume backend {}",
                config.backend.display()
            )
        })?;
        let backend_metadata = std::fs::metadata(&backend)?;
        let backend_modified_ns = backend_metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map(|value| value.as_nanos());
        let codex_home = default_codex_home()?;
        let config_path = codex_home.join("config.toml");
        let config_sha256 = if config_path.is_file() {
            Some(sha256_bytes(&std::fs::read(&config_path)?))
        } else {
            None
        };
        let environment = json!({
            "backendPath": backend.to_string_lossy(),
            "backendBytes": backend_metadata.len(),
            "backendModifiedNs": backend_modified_ns,
            "codexHome": codex_home.to_string_lossy(),
            "configSha256": config_sha256,
            "args": args
                .iter()
                .map(|value| value.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
        });
        Ok(Self {
            cache_root: config.root.join("Data").join("ResumeCache"),
            environment_fingerprint: fingerprint_json(&environment)?,
            policy: config.optimistic_resume.clone(),
        })
    }

    #[cfg(test)]
    fn disabled(index_root: &Path) -> Self {
        Self {
            cache_root: index_root.join("ResumeCache"),
            environment_fingerprint: "test-environment".to_string(),
            policy: OptimisticResumeRuntimePolicy::Disabled,
        }
    }

    fn request_fingerprint(&self, params: &serde_json::Map<String, Value>) -> Result<String> {
        let mut stable = params.clone();
        stable.remove("initialTurnsPage");
        stable.remove("excludeTurns");
        fingerprint_json(&json!({
            "environmentFingerprint": self.environment_fingerprint,
            "params": stable,
        }))
    }

    fn load(&self, thread_id: &str, request_fingerprint: &str) -> ResumeCacheLookup {
        let path = self.cache_path(thread_id);
        if !path.is_file() {
            return ResumeCacheLookup::Miss("cache_file_missing");
        }
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) if metadata.len() <= 2 * 1024 * 1024 => metadata,
            Ok(_) => {
                eprintln!(
                    "CLM optimistic Resume cache is oversized: {}",
                    path.display()
                );
                return ResumeCacheLookup::Miss("cache_oversized");
            }
            Err(error) => {
                eprintln!(
                    "CLM optimistic Resume cache metadata failed for {}: {error}",
                    path.display()
                );
                return ResumeCacheLookup::Miss("cache_metadata_error");
            }
        };
        let _ = metadata;
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) => {
                eprintln!(
                    "CLM optimistic Resume cache open failed for {}: {error}",
                    path.display()
                );
                return ResumeCacheLookup::Miss("cache_open_error");
            }
        };
        let envelope: ResumeCacheEnvelope = match serde_json::from_reader(file) {
            Ok(envelope) => envelope,
            Err(error) => {
                eprintln!(
                    "CLM optimistic Resume cache parse failed for {}: {error}",
                    path.display()
                );
                return ResumeCacheLookup::Miss("cache_parse_error");
            }
        };
        if envelope.format_version != 1 {
            return ResumeCacheLookup::Miss("cache_format_mismatch");
        }
        if envelope.thread_id != thread_id {
            return ResumeCacheLookup::Miss("cache_thread_mismatch");
        }
        if envelope.environment_fingerprint != self.environment_fingerprint {
            return ResumeCacheLookup::Miss("cache_environment_mismatch");
        }
        if envelope.request_fingerprint != request_fingerprint {
            return ResumeCacheLookup::Miss("cache_request_mismatch");
        }
        if !valid_cached_resume_result(&envelope.result, thread_id) {
            return ResumeCacheLookup::Miss("cache_result_invalid");
        }
        ResumeCacheLookup::Hit(envelope.result)
    }

    fn store(&self, thread_id: &str, request_fingerprint: &str, result: &Value) -> Result<()> {
        let mut result = result.clone();
        let result_object = result
            .as_object_mut()
            .context("optimistic Resume cache result is not an object")?;
        result_object.remove("initialTurnsPage");
        let thread = result_object
            .get_mut("thread")
            .and_then(Value::as_object_mut)
            .context("optimistic Resume cache result has no thread object")?;
        if thread.get("id").and_then(Value::as_str) != Some(thread_id) {
            bail!("optimistic Resume cache result belongs to a different thread");
        }
        thread.insert("turns".to_string(), Value::Array(Vec::new()));
        let envelope = ResumeCacheEnvelope {
            format_version: 1,
            thread_id: thread_id.to_string(),
            environment_fingerprint: self.environment_fingerprint.clone(),
            request_fingerprint: request_fingerprint.to_string(),
            cached_unix_ms: unix_time_ms(),
            result,
        };
        std::fs::create_dir_all(&self.cache_root).with_context(|| {
            format!(
                "failed to create optimistic Resume cache root {}",
                self.cache_root.display()
            )
        })?;
        let path = self.cache_path(thread_id);
        let temporary = self.cache_root.join(format!(
            ".{thread_id}.{}.{}.tmp",
            std::process::id(),
            unix_time_ms()
        ));
        let backup = self.cache_root.join(format!(".{thread_id}.previous"));
        let mut file = std::fs::File::create(&temporary)
            .with_context(|| format!("failed to create {}", temporary.display()))?;
        serde_json::to_writer(&mut file, &envelope)?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_all()?;
        if backup.exists() {
            std::fs::remove_file(&backup)?;
        }
        if path.exists() {
            std::fs::rename(&path, &backup).with_context(|| {
                format!(
                    "failed to retain prior optimistic Resume cache {}",
                    path.display()
                )
            })?;
        }
        if let Err(error) = std::fs::rename(&temporary, &path) {
            if backup.exists() {
                let _ = std::fs::rename(&backup, &path);
            }
            let _ = std::fs::remove_file(&temporary);
            return Err(error).with_context(|| {
                format!(
                    "failed to activate optimistic Resume cache {}",
                    path.display()
                )
            });
        }
        if backup.exists() {
            std::fs::remove_file(backup)?;
        }
        Ok(())
    }

    fn cache_path(&self, thread_id: &str) -> PathBuf {
        self.cache_root.join(format!("{thread_id}.json"))
    }
}

fn valid_cached_resume_result(result: &Value, thread_id: &str) -> bool {
    let Some(result) = result.as_object() else {
        return false;
    };
    if result.contains_key("initialTurnsPage") {
        return false;
    }
    let Some(thread) = result.get("thread").and_then(Value::as_object) else {
        return false;
    };
    thread.get("id").and_then(Value::as_str) == Some(thread_id)
        && thread
            .get("turns")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
}

fn fingerprint_json(value: &Value) -> Result<String> {
    let mut canonical = Vec::new();
    write_canonical_json(value, &mut canonical)?;
    Ok(sha256_bytes(&canonical))
}

fn write_canonical_json(value: &Value, output: &mut Vec<u8>) -> Result<()> {
    match value {
        Value::Object(map) => {
            output.push(b'{');
            let sorted = map.iter().collect::<BTreeMap<_, _>>();
            for (index, (key, value)) in sorted.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                serde_json::to_writer(&mut *output, key)?;
                output.push(b':');
                write_canonical_json(value, output)?;
            }
            output.push(b'}');
        }
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_canonical_json(value, output)?;
            }
            output.push(b']');
        }
        _ => serde_json::to_writer(output, value)?,
    }
    Ok(())
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn run_proxy(args: Vec<OsString>) -> Result<i32> {
    if args.len() == 1 && args[0] == "--validate-optimistic-resume-config" {
        let root = runtime_root_from_env()?;
        validate_optimistic_resume_policy(&root)?;
        return Ok(0);
    }
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
    let resume_cache = ResumeCacheContext::new(config, args)?;
    let mut child = backend_command(config, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to launch {}", config.backend.display()))?;
    #[cfg(windows)]
    let _backend_tree = BackendProcessTree::assign(&child)?;
    let child_stdin = child.stdin.take().context("backend stdin unavailable")?;
    let child_stdout = child.stdout.take().context("backend stdout unavailable")?;
    let pending = PendingMap::default();
    let pending_tail_drain_gates = PendingTailDrainGateMap::default();
    let tail_drain_gates = TailDrainGateMap::default();
    let client_owned_threads = ClientOwnedThreadSet::default();
    let skills_list_cache = SkillsListCache::default();
    let optimistic_gates = OptimisticGateMap::default();
    let state = ProxyState {
        index_root: config.index_root(),
        pending,
        pending_tail_drain_gates,
        tail_drain_gates,
        client_owned_threads,
        skills_list_cache,
        resume_cache,
        optimistic_gates,
    };
    let backend_input: SharedBackendInput =
        Arc::new(Mutex::new(Some(Box::new(BufWriter::new(child_stdin)))));
    let output = Arc::new(Mutex::new(BufWriter::new(std::io::stdout())));
    let output_reader = Arc::clone(&output);
    let state_reader = state.clone();
    let backend_input_reader = Arc::clone(&backend_input);
    let output_thread = thread::spawn(move || -> Result<()> {
        let reader = BufReader::new(child_stdout);
        for line in reader.lines() {
            let line = line?.trim_start_matches('\u{feff}').to_string();
            let mut effects = BackendEffects::default();
            let outgoing = match serde_json::from_str::<Value>(&line) {
                Ok(message) => {
                    process_backend_message_with_effects(message, &state_reader, &mut effects)
                }
                Err(_) => Ok(Some(Value::String(line.clone()))),
            };
            if let Some(resolution) = effects.gate_resolution {
                apply_gate_resolution(
                    resolution,
                    &state_reader.optimistic_gates,
                    &backend_input_reader,
                    &output_reader,
                )?;
            }
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

    let input = std::io::stdin();
    for line in input.lock().lines() {
        let line = line?.trim_start_matches('\u{feff}').to_string();
        let message = match serde_json::from_str::<Value>(&line) {
            Ok(message) => message,
            Err(_) => {
                write_backend_raw(&backend_input, &line)?;
                continue;
            }
        };
        match process_client_message_with_optimistic(message, &state) {
            Ok(ClientRoute::Forward(message)) => write_backend_json(&backend_input, &message)?,
            Ok(ClientRoute::Respond(message)) => write_json(&output, &message)?,
            Ok(ClientRoute::ForwardAndRespond {
                forward,
                response,
                timeout,
            }) => {
                write_backend_json(&backend_input, &forward)?;
                write_json(&output, &response)?;
                spawn_optimistic_timeout(
                    timeout,
                    Arc::clone(&state.optimistic_gates),
                    Arc::clone(&output),
                );
            }
            Ok(ClientRoute::Hold) => {}
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
    backend_input.lock().expect("backend input poisoned").take();
    let status = child.wait()?;
    output_thread
        .join()
        .map_err(|_| anyhow::anyhow!("backend output thread panicked"))??;
    Ok(status.code().unwrap_or(1))
}

#[cfg(windows)]
struct BackendProcessTree {
    job: HANDLE,
}

#[cfg(windows)]
impl BackendProcessTree {
    fn assign(child: &Child) -> Result<Self> {
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("failed to create the backend containment job");
        }

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            let error = std::io::Error::last_os_error();
            unsafe {
                CloseHandle(job);
            }
            return Err(error).context("failed to configure the backend containment job");
        }

        let process_handle = child.as_raw_handle() as HANDLE;
        let assigned = unsafe { AssignProcessToJobObject(job, process_handle) };
        if assigned == 0 {
            let error = std::io::Error::last_os_error();
            unsafe {
                CloseHandle(job);
            }
            return Err(error).context("failed to assign the Codex backend to its containment job");
        }

        Ok(Self { job })
    }
}

#[cfg(windows)]
impl Drop for BackendProcessTree {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.job);
        }
    }
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
    ForwardAndRespond {
        forward: Value,
        response: Value,
        timeout: OptimisticTimeout,
    },
    Hold,
}

fn process_client_message_with_optimistic(
    mut message: Value,
    state: &ProxyState,
) -> Result<ClientRoute> {
    let ProxyState {
        index_root,
        pending,
        pending_tail_drain_gates,
        tail_drain_gates,
        client_owned_threads,
        skills_list_cache,
        resume_cache,
        optimistic_gates,
    } = state;
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
        Some("turn/start") => {
            let request_id = request_id_from_message(&message)?;
            let params = message
                .get("params")
                .context("turn/start request has no params")?;
            let thread_id = required_string(params, "threadId")?;
            let mut gates = optimistic_gates
                .lock()
                .expect("optimistic Resume gate map poisoned");
            let Some(gate) = gates.get_mut(thread_id) else {
                return Ok(ClientRoute::Forward(message));
            };
            match gate.submit_turn(message, unix_time_ms_u64())? {
                TurnGateDisposition::Queued { .. } => Ok(ClientRoute::Hold),
                TurnGateDisposition::Forward { request } => Ok(ClientRoute::Forward(request)),
                TurnGateDisposition::Reject { reason } => Ok(ClientRoute::Respond(jsonrpc_error(
                    request_id, -32074, reason,
                ))),
            }
        }
        Some("skills/list") => {
            let cached = skills_list_cache
                .lock()
                .expect("skills/list cache poisoned")
                .begin_request(&message, Instant::now())?;
            if let Some(response) = cached {
                eprintln!("CLM skills/list cache hit");
                return Ok(ClientRoute::Respond(response));
            }
            Ok(ClientRoute::Forward(message))
        }
        Some(method) if invalidates_skills_list_cache(method) => {
            skills_list_cache
                .lock()
                .expect("skills/list cache poisoned")
                .invalidate();
            Ok(ClientRoute::Forward(message))
        }
        Some("thread/list") => {
            let params = message
                .as_object_mut()
                .context("thread list request is not an object")?
                .entry("params")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .context("thread list request params are not an object")?;
            params.insert("useStateDbOnly".to_string(), Value::Bool(true));
            Ok(ClientRoute::Forward(message))
        }
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
            let started_at = Instant::now();
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
            {
                let mut gates = optimistic_gates
                    .lock()
                    .expect("optimistic Resume gate map poisoned");
                match gates.get(&thread_id).map(OptimisticResumeGate::phase) {
                    Some(crate::OptimisticResumePhase::Resuming)
                    | Some(crate::OptimisticResumePhase::Ready) => {
                        bail!("an optimistic Resume is already active for thread {thread_id}");
                    }
                    Some(crate::OptimisticResumePhase::Failed) => {
                        gates.remove(&thread_id);
                    }
                    None => {}
                }
            }
            let cache_fingerprint = resume_cache.request_fingerprint(params)?;
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
            let path_provided_by_client = params.get("path").and_then(Value::as_str).is_some();
            let path_injected_by_clm = if path_provided_by_client {
                false
            } else if let Some(path) = managed_resume_path_hint(index_root, &thread_id)? {
                params.insert(
                    "path".to_string(),
                    Value::String(path.to_string_lossy().into_owned()),
                );
                true
            } else {
                false
            };
            params.insert("excludeTurns".to_string(), Value::Bool(false));
            let optimistic_policy_enabled = resume_cache.policy.enabled_for(&thread_id);
            let optimistic_lookup = if !optimistic_policy_enabled {
                ResumeCacheLookup::Miss("policy_disabled")
            } else if !original_exclude_turns {
                ResumeCacheLookup::Miss("full_turns_requested")
            } else {
                resume_cache.load(&thread_id, &cache_fingerprint)
            };
            let optimistic_cache_status = optimistic_lookup.status();
            let optimistic_result = optimistic_lookup.into_result();
            let optimistic_response = optimistic_result
                .map(|result| {
                    build_optimistic_resume_response(
                        id.clone(),
                        result,
                        &thread_id,
                        &index,
                        initial_page.as_ref(),
                        tail_drain_gates,
                    )
                })
                .transpose()?;
            let optimistic = optimistic_response.is_some();
            let client_response_ms = optimistic.then(|| started_at.elapsed().as_millis());
            {
                let mut pending_requests = pending.lock().expect("pending map poisoned");
                if pending_requests.contains_key(&request_key) {
                    bail!("duplicate pending request id for managed resume");
                }
                pending_requests.insert(
                    request_key.clone(),
                    PendingRequest::Resume(PendingResume {
                        thread_id: thread_id.clone(),
                        original_exclude_turns,
                        initial_page,
                        optimistic_policy_enabled,
                        optimistic_cache_status,
                        path_provided_by_client,
                        path_injected_by_clm,
                        started_at,
                        started_unix_ms: unix_time_ms(),
                        timing_log_path: resume_timing_log_path(index_root),
                        cache_fingerprint: cache_fingerprint.clone(),
                        optimistic,
                        client_response_ms,
                    }),
                );
            }
            if let Some(response) = optimistic_response {
                let gate = OptimisticResumeGate::begin(
                    thread_id.clone(),
                    &id,
                    unix_time_ms_u64(),
                    OptimisticResumeLimits::default(),
                )?;
                let mut gates = optimistic_gates
                    .lock()
                    .expect("optimistic Resume gate map poisoned");
                gates.insert(thread_id.clone(), gate);
                drop(gates);
                return Ok(ClientRoute::ForwardAndRespond {
                    forward: message,
                    response,
                    timeout: OptimisticTimeout {
                        thread_id,
                        resume_id: id,
                        timeout_ms: OptimisticResumeLimits::default().timeout_ms,
                    },
                });
            }
            pending_tail_drain_gates
                .lock()
                .expect("pending tail drain gate map poisoned")
                .insert(request_key, thread_id);
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
            let thread_id = required_string(params, "threadId")?.to_string();
            let include_turns = params
                .get("includeTurns")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if include_turns && managed_index(index_root, &thread_id)?.is_some() {
                let id = message
                    .get("id")
                    .cloned()
                    .context("thread read request has no id")?;
                let request_key = id_key(&id)?;
                let mut pending_requests = pending.lock().expect("pending map poisoned");
                if pending_requests.contains_key(&request_key) {
                    bail!("duplicate pending request id for managed eager read");
                }
                message
                    .get_mut("params")
                    .and_then(Value::as_object_mut)
                    .context("thread read request params are not an object")?
                    .insert("includeTurns".to_string(), Value::Bool(false));
                pending_requests.insert(
                    request_key,
                    PendingRequest::EagerRead(PendingEagerRead { thread_id }),
                );
            }
            Ok(ClientRoute::Forward(message))
        }
        _ => Ok(ClientRoute::Forward(message)),
    }
}

#[cfg(test)]
fn process_client_message(
    message: Value,
    index_root: &Path,
    pending: &PendingMap,
    pending_tail_drain_gates: &PendingTailDrainGateMap,
    tail_drain_gates: &TailDrainGateMap,
    client_owned_threads: &ClientOwnedThreadSet,
    skills_list_cache: &SkillsListCache,
) -> Result<ClientRoute> {
    let state = ProxyState {
        index_root: index_root.to_path_buf(),
        pending: Arc::clone(pending),
        pending_tail_drain_gates: Arc::clone(pending_tail_drain_gates),
        tail_drain_gates: Arc::clone(tail_drain_gates),
        client_owned_threads: Arc::clone(client_owned_threads),
        skills_list_cache: Arc::clone(skills_list_cache),
        resume_cache: ResumeCacheContext::disabled(index_root),
        optimistic_gates: OptimisticGateMap::default(),
    };
    process_client_message_with_optimistic(message, &state)
}

fn build_optimistic_resume_response(
    id: Value,
    mut result: Value,
    thread_id: &str,
    index: &IndexedRollout,
    page_request: Option<&PageRequest>,
    tail_drain_gates: &TailDrainGateMap,
) -> Result<Value> {
    if !valid_cached_resume_result(&result, thread_id) {
        bail!("optimistic Resume cache failed its response invariant");
    }
    if let Some(page_request) = page_request {
        let page = index.list_api_turns(
            thread_id,
            page_request.limit,
            None,
            page_request.sort_direction,
            page_request.items_view,
        )?;
        let mut gates = tail_drain_gates
            .lock()
            .expect("tail drain gate map poisoned");
        if let Some(cursor) = page.next_cursor.as_ref() {
            gates.insert(thread_id.to_string(), cursor.clone());
        } else {
            gates.remove(thread_id);
        }
        drop(gates);
        result
            .as_object_mut()
            .context("optimistic Resume result is not an object")?
            .insert("initialTurnsPage".to_string(), serde_json::to_value(page)?);
    }
    Ok(json!({"id": id, "result": result}))
}

fn request_id_from_message(message: &Value) -> Result<Value> {
    message
        .get("id")
        .cloned()
        .context("JSON-RPC request has no id")
}

fn skills_list_cache_key(message: &Value) -> Result<String> {
    let mut params = message.get("params").cloned().unwrap_or_else(|| json!({}));
    let params = params
        .as_object_mut()
        .context("skills/list request params are not an object")?;
    params.remove("forceReload");
    Ok(serde_json::to_string(params)?)
}

fn invalidates_skills_list_cache(method: &str) -> bool {
    matches!(
        method,
        "skills/config/write"
            | "plugin/install"
            | "plugin/uninstall"
            | "plugin/share/save"
            | "plugin/share/updateTargets"
            | "plugin/share/checkout"
            | "plugin/share/delete"
    )
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

fn managed_resume_path_hint(index_root: &Path, thread_id: &str) -> Result<Option<PathBuf>> {
    let runtime_root = index_root
        .parent()
        .and_then(Path::parent)
        .context("managed index root is outside a CLM runtime")?;
    let manifest_path = runtime_root
        .join("Data")
        .join("Vault")
        .join("Codex")
        .join(thread_id)
        .join("manifest.json");
    if !manifest_path.is_file() {
        return Ok(None);
    }

    let manifest: Value = serde_json::from_reader(std::fs::File::open(&manifest_path)?)
        .with_context(|| format!("invalid managed manifest {}", manifest_path.display()))?;
    let manifest_thread_id = manifest
        .get("threadId")
        .and_then(Value::as_str)
        .context("managed manifest has no threadId")?;
    if manifest_thread_id != thread_id {
        bail!("managed Resume manifest belongs to a different thread");
    }

    let expected_index = std::fs::canonicalize(index_path(index_root, thread_id)?)?;
    let manifest_index = manifest
        .get("indexPath")
        .and_then(Value::as_str)
        .context("managed manifest has no indexPath")?;
    if std::fs::canonicalize(manifest_index)? != expected_index {
        bail!("managed Resume manifest points at a different history index");
    }

    let active_path = manifest
        .get("originalPath")
        .and_then(Value::as_str)
        .context("managed manifest has no originalPath")?;
    let active_path = std::fs::canonicalize(active_path)
        .with_context(|| format!("failed to resolve managed active rollout {active_path}"))?;
    if read_rollout_thread_id(&active_path)? != thread_id {
        bail!("managed Resume path hint belongs to a different thread");
    }
    Ok(Some(active_path))
}

fn process_backend_message_with_effects(
    mut message: Value,
    state: &ProxyState,
    effects: &mut BackendEffects,
) -> Result<Option<Value>> {
    let ProxyState {
        index_root,
        pending,
        pending_tail_drain_gates,
        tail_drain_gates,
        client_owned_threads,
        skills_list_cache,
        resume_cache,
        optimistic_gates: _,
    } = state;
    if let Some(id) = message.get("id").cloned() {
        let response_key = id_key(&id)?;
        let stored_skills_response = skills_list_cache
            .lock()
            .expect("skills/list cache poisoned")
            .complete_request(&response_key, &message, Instant::now());
        if stored_skills_response {
            eprintln!("CLM skills/list cache store");
        }
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
        let pending_request = pending
            .lock()
            .expect("pending map poisoned")
            .remove(&response_key);
        if let Some(pending_request) = pending_request {
            match pending_request {
                PendingRequest::Resume(pending_resume) => {
                    let backend_response_ms = pending_resume.started_at.elapsed().as_millis();
                    if message.get("error").is_some() {
                        tail_drain_gates
                            .lock()
                            .expect("tail drain gate map poisoned")
                            .remove(&pending_resume.thread_id);
                        record_resume_timing(
                            &pending_resume,
                            &response_key,
                            "backend_error",
                            None,
                            backend_response_ms,
                        );
                        if pending_resume.optimistic {
                            effects.gate_resolution = Some(GateResolution::Failed {
                                thread_id: pending_resume.thread_id,
                                resume_id: id,
                                reason: backend_error_message(&message),
                            });
                            return Ok(None);
                        }
                        return Ok(Some(message));
                    }
                    let processed = (|| -> Result<usize> {
                        let cache_result = message
                            .get("result")
                            .cloned()
                            .context("managed resume response has no result")?;
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
                        if let Err(error) = resume_cache.store(
                            &pending_resume.thread_id,
                            &pending_resume.cache_fingerprint,
                            &cache_result,
                        ) {
                            eprintln!("CLM optimistic Resume cache store failed: {error:#}");
                        }
                        if pending_resume.original_exclude_turns {
                            result
                                .get_mut("thread")
                                .and_then(Value::as_object_mut)
                                .context("managed resume response thread is not an object")?
                                .insert("turns".to_string(), Value::Array(Vec::new()));
                        }
                        if let Some(page_request) = pending_resume.initial_page.as_ref() {
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
                            result.insert(
                                "initialTurnsPage".to_string(),
                                serde_json::to_value(page)?,
                            );
                        }
                        Ok(turns.len())
                    })();
                    let turns_len = match processed {
                        Ok(turns_len) => turns_len,
                        Err(error) if pending_resume.optimistic => {
                            record_resume_timing(
                                &pending_resume,
                                &response_key,
                                "optimistic_postprocess_error",
                                None,
                                backend_response_ms,
                            );
                            effects.gate_resolution = Some(GateResolution::Failed {
                                thread_id: pending_resume.thread_id,
                                resume_id: id,
                                reason: format!("Resume post-processing failed: {error:#}"),
                            });
                            eprintln!("CLM optimistic Resume post-processing failed: {error:#}");
                            return Ok(None);
                        }
                        Err(error) => return Err(error),
                    };
                    record_resume_timing(
                        &pending_resume,
                        &response_key,
                        if pending_resume.optimistic {
                            "optimistic_backend_ready"
                        } else {
                            "ok"
                        },
                        Some(turns_len),
                        backend_response_ms,
                    );
                    if pending_resume.optimistic {
                        effects.gate_resolution = Some(GateResolution::Ready {
                            thread_id: pending_resume.thread_id,
                            resume_id: id,
                        });
                        return Ok(None);
                    }
                    return Ok(Some(message));
                }
                PendingRequest::EagerRead(pending_read) => {
                    if message.get("error").is_some() {
                        return Ok(Some(message));
                    }
                    let thread = message
                        .get_mut("result")
                        .and_then(Value::as_object_mut)
                        .context("managed read response has no result object")?
                        .get_mut("thread")
                        .and_then(Value::as_object_mut)
                        .context("managed read response has no thread object")?;
                    let returned_thread_id = thread
                        .get("id")
                        .and_then(Value::as_str)
                        .context("managed read response thread has no id")?;
                    if returned_thread_id != pending_read.thread_id {
                        bail!(
                            "managed read response returned thread {returned_thread_id}, expected {}",
                            pending_read.thread_id
                        );
                    }
                    let path = index_path(index_root, &pending_read.thread_id)?;
                    let index = IndexedRollout::open(&path)?;
                    let turns = index.read_all_api_turns(&pending_read.thread_id)?;
                    thread.insert("turns".to_string(), Value::Array(turns));
                    return Ok(Some(message));
                }
            }
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

#[cfg(test)]
fn process_backend_message(
    message: Value,
    index_root: &Path,
    pending: &PendingMap,
    pending_tail_drain_gates: &PendingTailDrainGateMap,
    tail_drain_gates: &TailDrainGateMap,
    client_owned_threads: &ClientOwnedThreadSet,
    skills_list_cache: &SkillsListCache,
) -> Result<Option<Value>> {
    let state = ProxyState {
        index_root: index_root.to_path_buf(),
        pending: Arc::clone(pending),
        pending_tail_drain_gates: Arc::clone(pending_tail_drain_gates),
        tail_drain_gates: Arc::clone(tail_drain_gates),
        client_owned_threads: Arc::clone(client_owned_threads),
        skills_list_cache: Arc::clone(skills_list_cache),
        resume_cache: ResumeCacheContext::disabled(index_root),
        optimistic_gates: OptimisticGateMap::default(),
    };
    let mut effects = BackendEffects::default();
    let response = process_backend_message_with_effects(message, &state, &mut effects)?;
    if effects.gate_resolution.is_some() {
        bail!("test helper cannot discard an optimistic Resume gate resolution");
    }
    Ok(response)
}

fn backend_error_message(message: &Value) -> String {
    message
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("backend rejected Resume")
        .to_string()
}

fn resume_timing_log_path(index_root: &Path) -> Option<PathBuf> {
    if index_root.file_name().and_then(|value| value.to_str()) != Some("Indexes") {
        return None;
    }
    let data_root = index_root.parent()?;
    if data_root.file_name().and_then(|value| value.to_str()) != Some("Data") {
        return None;
    }
    Some(data_root.parent()?.join("Logs").join("resume-timing.jsonl"))
}

fn unix_time_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn unix_time_ms_u64() -> u64 {
    u64::try_from(unix_time_ms()).unwrap_or(u64::MAX)
}

fn record_resume_timing(
    pending: &PendingResume,
    request_id: &str,
    status: &str,
    backend_turns: Option<usize>,
    backend_response_ms: u128,
) {
    let Some(path) = pending.timing_log_path.as_ref() else {
        return;
    };
    let elapsed_ms = pending.started_at.elapsed().as_millis();
    let record = ResumeTimingRecord {
        format_version: 3,
        thread_id: &pending.thread_id,
        request_id,
        started_unix_ms: pending.started_unix_ms,
        finished_unix_ms: unix_time_ms(),
        elapsed_ms,
        backend_response_ms,
        postprocess_ms: elapsed_ms.saturating_sub(backend_response_ms),
        status,
        backend_turns,
        original_exclude_turns: pending.original_exclude_turns,
        initial_page_present: pending.initial_page.is_some(),
        optimistic_policy_enabled: pending.optimistic_policy_enabled,
        optimistic_cache_status: pending.optimistic_cache_status,
        path_provided_by_client: pending.path_provided_by_client,
        path_injected_by_clm: pending.path_injected_by_clm,
        optimistic: pending.optimistic,
        client_response_ms: pending.client_response_ms,
    };
    if let Err(error) = append_resume_timing(path, &record) {
        eprintln!("CLM Resume telemetry write failed: {error:#}");
    }
}

fn append_resume_timing(path: &Path, record: &ResumeTimingRecord<'_>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create Resume telemetry root {}",
                parent.display()
            )
        })?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open Resume telemetry {}", path.display()))?;
    serde_json::to_writer(&mut file, record)?;
    file.write_all(b"\n")?;
    Ok(())
}

fn apply_gate_resolution(
    resolution: GateResolution,
    optimistic_gates: &OptimisticGateMap,
    backend_input: &SharedBackendInput,
    output: &SharedOutput,
) -> Result<()> {
    match resolution {
        GateResolution::Ready {
            thread_id,
            resume_id,
        } => {
            let mut gates = optimistic_gates
                .lock()
                .expect("optimistic Resume gate map poisoned");
            let Some(gate) = gates.get_mut(&thread_id) else {
                return Ok(());
            };
            if !gate.matches_resume(&resume_id) {
                return Ok(());
            }
            let released = match gate.complete_resume(&resume_id, unix_time_ms_u64()) {
                Ok(released) => {
                    let mut input = backend_input.lock().expect("backend input poisoned");
                    let writer = input.as_mut().context("backend input is closed")?;
                    for request in &released {
                        write_json_line(&mut **writer, request)?;
                    }
                    gates.remove(&thread_id);
                    return Ok(());
                }
                Err(error) => {
                    eprintln!("CLM optimistic Resume completed too late: {error:#}");
                    gate.expire(unix_time_ms_u64()).unwrap_or_default()
                }
            };
            gates.remove(&thread_id);
            drop(gates);
            write_queued_turn_errors(
                output,
                released,
                "Resume completed after the optimistic Send gate timed out",
            )
        }
        GateResolution::Failed {
            thread_id,
            resume_id,
            reason,
        } => {
            let mut gates = optimistic_gates
                .lock()
                .expect("optimistic Resume gate map poisoned");
            let Some(gate) = gates.get_mut(&thread_id) else {
                return Ok(());
            };
            if !gate.matches_resume(&resume_id) {
                return Ok(());
            }
            let retained = gate
                .fail_resume(&resume_id, reason.clone())
                .unwrap_or_default();
            drop(gates);
            write_queued_turn_errors(output, retained, &reason)
        }
    }
}

fn spawn_optimistic_timeout(
    timeout: OptimisticTimeout,
    optimistic_gates: OptimisticGateMap,
    output: SharedOutput,
) {
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(timeout.timeout_ms));
        let retained = {
            let mut gates = optimistic_gates
                .lock()
                .expect("optimistic Resume gate map poisoned");
            let Some(gate) = gates.get_mut(&timeout.thread_id) else {
                return;
            };
            if !gate.matches_resume(&timeout.resume_id) {
                return;
            }
            gate.expire(unix_time_ms_u64()).unwrap_or_default()
        };
        if !retained.is_empty()
            && let Err(error) = write_queued_turn_errors(
                &output,
                retained,
                "Resume timed out before the backend accepted Send",
            )
        {
            eprintln!("CLM optimistic Resume timeout response failed: {error:#}");
        }
    });
}

fn write_queued_turn_errors(
    output: &SharedOutput,
    requests: Vec<Value>,
    reason: &str,
) -> Result<()> {
    for request in requests {
        let id = request_id_from_message(&request)?;
        write_json(output, &jsonrpc_error(id, -32074, reason.to_string()))?;
    }
    Ok(())
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

fn write_backend_json(input: &SharedBackendInput, value: &Value) -> Result<()> {
    let mut input = input.lock().expect("backend input poisoned");
    let writer = input.as_mut().context("backend input is closed")?;
    write_json_line(&mut **writer, value)
}

fn write_backend_raw(input: &SharedBackendInput, value: &str) -> Result<()> {
    let mut input = input.lock().expect("backend input poisoned");
    let writer = input.as_mut().context("backend input is closed")?;
    writer.write_all(value.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn write_json_line(writer: &mut dyn Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
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

    struct CaptureWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for CaptureWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.bytes
                .lock()
                .expect("capture writer poisoned")
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

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
        std::fs::write(
            &source,
            format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "type": "session_meta",
                    "payload": {"id": thread_id, "history_mode": "paginated"}
                }))?
            ),
        )?;
        let index_root = temp.path().join("Data").join("Indexes");
        std::fs::create_dir_all(&index_root)?;
        let index_path = index_path(&index_root, thread_id)?;
        let mut index = IndexedRollout::open(&index_path)?;
        index.replace_api_projection(
            &source,
            thread_id,
            "hash",
            "codex-cli 0.144.2",
            &[api_turn(0), api_turn(1), api_turn(2)],
        )?;
        let vault = temp
            .path()
            .join("Data")
            .join("Vault")
            .join("Codex")
            .join(thread_id);
        std::fs::create_dir_all(&vault)?;
        std::fs::write(
            vault.join("manifest.json"),
            serde_json::to_vec(&json!({
                "threadId": thread_id,
                "originalPath": source,
                "indexPath": index_path,
            }))?,
        )?;

        let pending = PendingMap::default();
        let pending_tail_drain_gates = PendingTailDrainGateMap::default();
        let tail_drain_gates = TailDrainGateMap::default();
        let client_owned_threads = ClientOwnedThreadSet::default();
        let skills_list_cache = SkillsListCache::default();
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
            &index_root,
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
            &skills_list_cache,
        )?
        else {
            panic!("resume should be forwarded")
        };
        assert_eq!(rewritten["params"]["excludeTurns"], false);
        assert!(rewritten["params"].get("initialTurnsPage").is_none());
        assert_eq!(
            std::fs::canonicalize(rewritten["params"]["path"].as_str().unwrap())?,
            std::fs::canonicalize(&source)?
        );

        let backend = json!({
            "id": 9,
            "result": {
                "thread": {"id": thread_id, "turns": [api_turn(1), api_turn(2), api_turn(3)]}
            }
        });
        let response = process_backend_message(
            backend,
            &index_root,
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
            &skills_list_cache,
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
        let timing_path = temp.path().join("Logs").join("resume-timing.jsonl");
        let timing_lines = std::fs::read_to_string(timing_path)?;
        let records = timing_lines.lines().collect::<Vec<_>>();
        assert_eq!(records.len(), 1);
        let timing: Value = serde_json::from_str(records[0])?;
        assert_eq!(timing["threadId"], thread_id);
        assert_eq!(timing["requestId"], "9");
        assert_eq!(timing["status"], "ok");
        assert_eq!(timing["backendTurns"], 3);
        assert_eq!(timing["pathProvidedByClient"], false);
        assert_eq!(timing["pathInjectedByClm"], true);
        assert_eq!(timing["optimistic"], false);
        assert!(timing["clientResponseMs"].is_null());
        assert!(timing["backendResponseMs"].is_number());
        assert!(timing["postprocessMs"].is_number());
        assert!(timing["finishedUnixMs"].as_u64() >= timing["startedUnixMs"].as_u64());
        Ok(())
    }

    #[test]
    fn warm_optimistic_resume_responds_immediately_and_releases_fast_enter_once() -> Result<()> {
        let temp = tempdir()?;
        let thread_id = "00000000-0000-7000-8000-000000000888";
        let source = temp.path().join("source.jsonl");
        std::fs::write(
            &source,
            format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "type": "session_meta",
                    "payload": {"id": thread_id, "history_mode": "paginated"}
                }))?
            ),
        )?;
        let index_root = temp.path().join("Data").join("Indexes");
        std::fs::create_dir_all(&index_root)?;
        let projection_path = index_path(&index_root, thread_id)?;
        let mut index = IndexedRollout::open(&projection_path)?;
        index.replace_api_projection(
            &source,
            thread_id,
            "hash",
            "codex-cli 0.144.2",
            &[api_turn(0), api_turn(1), api_turn(2)],
        )?;
        let vault = temp
            .path()
            .join("Data")
            .join("Vault")
            .join("Codex")
            .join(thread_id);
        std::fs::create_dir_all(&vault)?;
        std::fs::write(
            vault.join("manifest.json"),
            serde_json::to_vec(&json!({
                "threadId": thread_id,
                "originalPath": source,
                "indexPath": projection_path,
            }))?,
        )?;
        let mut canaries = std::collections::BTreeSet::new();
        canaries.insert(thread_id.to_string());
        let state = ProxyState {
            index_root: index_root.clone(),
            pending: PendingMap::default(),
            pending_tail_drain_gates: PendingTailDrainGateMap::default(),
            tail_drain_gates: TailDrainGateMap::default(),
            client_owned_threads: ClientOwnedThreadSet::default(),
            skills_list_cache: SkillsListCache::default(),
            resume_cache: ResumeCacheContext {
                cache_root: temp.path().join("Data").join("ResumeCache"),
                environment_fingerprint: "test-environment".to_string(),
                policy: OptimisticResumeRuntimePolicy::Canary(canaries),
            },
            optimistic_gates: OptimisticGateMap::default(),
        };
        let resume_request = |id| {
            json!({
                "method": "thread/resume",
                "id": id,
                "params": {
                    "threadId": thread_id,
                    "excludeTurns": true,
                    "initialTurnsPage": {
                        "limit": 2,
                        "sortDirection": "desc",
                        "itemsView": "full"
                    },
                    "cwd": temp.path()
                }
            })
        };

        let ClientRoute::Forward(_) =
            process_client_message_with_optimistic(resume_request(20), &state)?
        else {
            panic!("cold optimistic Resume must use the official response")
        };
        let official_result = json!({
            "thread": {
                "id": thread_id,
                "path": source,
                "cwd": temp.path(),
                "status": {"type": "idle"},
                "turns": [api_turn(1), api_turn(2), api_turn(3)]
            },
            "model": "test-model",
            "approvalPolicy": "never",
            "sandbox": {"type": "dangerFullAccess"}
        });
        let mut cold_effects = BackendEffects::default();
        let cold_response = process_backend_message_with_effects(
            json!({"id": 20, "result": official_result.clone()}),
            &state,
            &mut cold_effects,
        )?
        .context("cold official Resume response disappeared")?;
        assert_eq!(cold_response["result"]["model"], "test-model");
        assert!(cold_effects.gate_resolution.is_none());
        assert!(state.resume_cache.cache_path(thread_id).is_file());

        let ClientRoute::ForwardAndRespond {
            forward,
            response,
            timeout: _,
        } = process_client_message_with_optimistic(resume_request(21), &state)?
        else {
            panic!("warm Resume did not take the optimistic route")
        };
        assert_eq!(forward["params"]["excludeTurns"], false);
        assert_eq!(response["id"], 21);
        assert_eq!(response["result"]["model"], "test-model");
        assert_eq!(
            response["result"]["initialTurnsPage"]["data"][0]["id"],
            "turn-3"
        );
        assert!(
            response["result"]["thread"]["turns"]
                .as_array()
                .is_some_and(Vec::is_empty)
        );

        let fast_turn = json!({
            "id": 22,
            "method": "turn/start",
            "params": {
                "threadId": thread_id,
                "input": [{"type": "text", "text": "fast enter"}]
            }
        });
        assert!(matches!(
            process_client_message_with_optimistic(fast_turn.clone(), &state)?,
            ClientRoute::Hold
        ));
        let mut warm_effects = BackendEffects::default();
        assert!(
            process_backend_message_with_effects(
                json!({"id": 21, "result": official_result}),
                &state,
                &mut warm_effects,
            )?
            .is_none()
        );
        assert!(matches!(
            warm_effects.gate_resolution,
            Some(GateResolution::Ready { .. })
        ));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let backend_input: SharedBackendInput =
            Arc::new(Mutex::new(Some(Box::new(CaptureWriter {
                bytes: Arc::clone(&captured),
            }))));
        let output = Arc::new(Mutex::new(BufWriter::new(std::io::stdout())));
        apply_gate_resolution(
            warm_effects.gate_resolution.take().unwrap(),
            &state.optimistic_gates,
            &backend_input,
            &output,
        )?;
        let captured = String::from_utf8(captured.lock().unwrap().clone())?;
        let lines = captured.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1);
        assert_eq!(serde_json::from_str::<Value>(lines[0])?, fast_turn);
        assert!(
            !state
                .optimistic_gates
                .lock()
                .expect("optimistic Resume gate map poisoned")
                .contains_key(thread_id)
        );
        Ok(())
    }

    #[test]
    fn warm_optimistic_resume_supports_desktop_follow_up_turn_paging() -> Result<()> {
        let temp = tempdir()?;
        let thread_id = "00000000-0000-7000-8000-000000000889";
        let source = temp.path().join("source.jsonl");
        std::fs::write(
            &source,
            format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "type": "session_meta",
                    "payload": {"id": thread_id, "history_mode": "paginated"}
                }))?
            ),
        )?;
        let index_root = temp.path().join("Data").join("Indexes");
        std::fs::create_dir_all(&index_root)?;
        let projection_path = index_path(&index_root, thread_id)?;
        let mut index = IndexedRollout::open(&projection_path)?;
        index.replace_api_projection(
            &source,
            thread_id,
            "hash",
            "codex-cli 0.144.2",
            &[api_turn(0), api_turn(1), api_turn(2)],
        )?;
        let vault = temp
            .path()
            .join("Data")
            .join("Vault")
            .join("Codex")
            .join(thread_id);
        std::fs::create_dir_all(&vault)?;
        std::fs::write(
            vault.join("manifest.json"),
            serde_json::to_vec(&json!({
                "threadId": thread_id,
                "originalPath": source,
                "indexPath": projection_path,
            }))?,
        )?;
        let mut canaries = std::collections::BTreeSet::new();
        canaries.insert(thread_id.to_string());
        let state = ProxyState {
            index_root: index_root.clone(),
            pending: PendingMap::default(),
            pending_tail_drain_gates: PendingTailDrainGateMap::default(),
            tail_drain_gates: TailDrainGateMap::default(),
            client_owned_threads: ClientOwnedThreadSet::default(),
            skills_list_cache: SkillsListCache::default(),
            resume_cache: ResumeCacheContext {
                cache_root: temp.path().join("Data").join("ResumeCache"),
                environment_fingerprint: "test-environment".to_string(),
                policy: OptimisticResumeRuntimePolicy::Canary(canaries),
            },
            optimistic_gates: OptimisticGateMap::default(),
        };
        let resume_request = |id| {
            json!({
                "method": "thread/resume",
                "id": id,
                "params": {
                    "threadId": thread_id,
                    "excludeTurns": true,
                    "cwd": temp.path()
                }
            })
        };
        let official_result = json!({
            "thread": {
                "id": thread_id,
                "path": source,
                "cwd": temp.path(),
                "status": {"type": "idle"},
                "turns": [api_turn(1), api_turn(2), api_turn(3)]
            },
            "model": "test-model",
            "approvalPolicy": "never",
            "sandbox": {"type": "dangerFullAccess"}
        });

        assert!(matches!(
            process_client_message_with_optimistic(resume_request(30), &state)?,
            ClientRoute::Forward(_)
        ));
        let mut cold_effects = BackendEffects::default();
        assert!(
            process_backend_message_with_effects(
                json!({"id": 30, "result": official_result.clone()}),
                &state,
                &mut cold_effects,
            )?
            .is_some()
        );

        let ClientRoute::ForwardAndRespond { response, .. } =
            process_client_message_with_optimistic(resume_request(31), &state)?
        else {
            panic!("Desktop follow-up paging Resume did not take the optimistic route")
        };
        assert_eq!(response["result"]["model"], "test-model");
        assert!(response["result"].get("initialTurnsPage").is_none());
        assert!(
            response["result"]["thread"]["turns"]
                .as_array()
                .is_some_and(Vec::is_empty)
        );

        let page = process_client_message_with_optimistic(
            json!({
                "id": 32,
                "method": "thread/turns/list",
                "params": {
                    "threadId": thread_id,
                    "cursor": null,
                    "limit": 5,
                    "sortDirection": "desc",
                    "itemsView": "full"
                }
            }),
            &state,
        )?;
        let ClientRoute::Respond(page) = page else {
            panic!("Desktop follow-up page did not use the managed index")
        };
        assert_eq!(page["result"]["data"][0]["id"], "turn-3");
        Ok(())
    }

    #[test]
    fn thread_list_uses_state_db_catalog_without_dropping_archive_filters() -> Result<()> {
        let pending = PendingMap::default();
        let pending_tail_drain_gates = PendingTailDrainGateMap::default();
        let tail_drain_gates = TailDrainGateMap::default();
        let client_owned_threads = ClientOwnedThreadSet::default();
        let skills_list_cache = SkillsListCache::default();
        let request = json!({
            "method": "thread/list",
            "id": 10,
            "params": {
                "archived": true,
                "limit": 100,
                "sourceKinds": ["vscode"],
                "useStateDbOnly": false
            }
        });
        let ClientRoute::Forward(rewritten) = process_client_message(
            request,
            std::path::Path::new("unused"),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
            &skills_list_cache,
        )?
        else {
            panic!("thread list should be forwarded")
        };
        assert_eq!(rewritten["params"]["useStateDbOnly"], true);
        assert_eq!(rewritten["params"]["archived"], true);
        assert_eq!(rewritten["params"]["limit"], 100);
        assert_eq!(rewritten["params"]["sourceKinds"], json!(["vscode"]));
        Ok(())
    }

    #[test]
    fn skills_list_success_is_reused_with_the_callers_request_id() -> Result<()> {
        let temp = tempdir()?;
        let pending = PendingMap::default();
        let pending_tail_drain_gates = PendingTailDrainGateMap::default();
        let tail_drain_gates = TailDrainGateMap::default();
        let client_owned_threads = ClientOwnedThreadSet::default();
        let skills_list_cache = SkillsListCache::default();
        let first_request = json!({
            "id": 40,
            "method": "skills/list",
            "params": {
                "cwds": ["D:\\Projects\\One", "D:\\Projects\\Two"],
                "forceReload": false
            }
        });

        let ClientRoute::Forward(forwarded) = process_client_message(
            first_request.clone(),
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
            &skills_list_cache,
        )?
        else {
            panic!("the first skills/list request must reach the backend")
        };
        assert_eq!(forwarded, first_request);

        let backend_response = json!({
            "jsonrpc": "2.0",
            "id": 40,
            "result": {"data": [{"cwd": "D:\\Projects\\One", "skills": []}]}
        });
        let forwarded_response = process_backend_message(
            backend_response.clone(),
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
            &skills_list_cache,
        )?
        .context("skills/list response should be forwarded")?;
        assert_eq!(forwarded_response, backend_response);

        let second_request = json!({
            "id": "second",
            "method": "skills/list",
            "params": {
                "cwds": ["D:\\Projects\\One", "D:\\Projects\\Two"],
                "forceReload": false
            }
        });
        let ClientRoute::Respond(cached) = process_client_message(
            second_request,
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
            &skills_list_cache,
        )?
        else {
            panic!("the second identical skills/list request should use the proxy cache")
        };
        assert_eq!(cached["id"], "second");
        assert_eq!(cached["jsonrpc"], "2.0");
        assert_eq!(cached["result"], backend_response["result"]);
        Ok(())
    }

    #[test]
    fn skills_force_reload_and_mutations_invalidate_cached_generations() -> Result<()> {
        let temp = tempdir()?;
        let pending = PendingMap::default();
        let pending_tail_drain_gates = PendingTailDrainGateMap::default();
        let tail_drain_gates = TailDrainGateMap::default();
        let client_owned_threads = ClientOwnedThreadSet::default();
        let skills_list_cache = SkillsListCache::default();

        let ordinary = json!({
            "id": 50,
            "method": "skills/list",
            "params": {"cwds": ["D:\\Projects\\One"], "forceReload": false}
        });
        let ClientRoute::Forward(_) = process_client_message(
            ordinary,
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
            &skills_list_cache,
        )?
        else {
            panic!("initial skills/list request should be forwarded")
        };
        process_backend_message(
            json!({"id": 50, "result": {"version": "old"}}),
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
            &skills_list_cache,
        )?;

        let forced = json!({
            "id": 51,
            "method": "skills/list",
            "params": {"cwds": ["D:\\Projects\\One"], "forceReload": true}
        });
        let ClientRoute::Forward(_) = process_client_message(
            forced,
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
            &skills_list_cache,
        )?
        else {
            panic!("forceReload must bypass the proxy cache")
        };
        assert!(
            skills_list_cache
                .lock()
                .expect("skills/list cache poisoned")
                .entries
                .is_empty()
        );
        process_backend_message(
            json!({"id": 51, "result": {"version": "fresh"}}),
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
            &skills_list_cache,
        )?;

        let ClientRoute::Respond(fresh) = process_client_message(
            json!({
                "id": 52,
                "method": "skills/list",
                "params": {"cwds": ["D:\\Projects\\One"], "forceReload": false}
            }),
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
            &skills_list_cache,
        )?
        else {
            panic!("a successful force reload should seed the next ordinary request")
        };
        assert_eq!(fresh["result"]["version"], "fresh");

        let ClientRoute::Forward(_) = process_client_message(
            json!({
                "id": 53,
                "method": "skills/config/write",
                "params": {"path": "unused", "enabled": false}
            }),
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
            &skills_list_cache,
        )?
        else {
            panic!("skills/config/write should be forwarded")
        };
        assert!(
            skills_list_cache
                .lock()
                .expect("skills/list cache poisoned")
                .entries
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn skills_cache_is_bounded_and_rejects_errors_expiry_and_stale_generations() -> Result<()> {
        let start = Instant::now();
        let mut cache = SkillsListCacheState::default();
        for index in 0..=SKILLS_LIST_CACHE_MAX_ENTRIES {
            let request = json!({
                "id": index,
                "method": "skills/list",
                "params": {"cwds": [format!("D:\\Projects\\{index}")]}
            });
            assert!(
                cache
                    .begin_request(&request, start + Duration::from_secs(index as u64))?
                    .is_none()
            );
            assert!(cache.complete_request(
                &id_key(&json!(index))?,
                &json!({"id": index, "result": {"index": index}}),
                start + Duration::from_secs(index as u64),
            ));
        }
        assert_eq!(cache.entries.len(), SKILLS_LIST_CACHE_MAX_ENTRIES);
        let first_key = skills_list_cache_key(&json!({
            "params": {"cwds": ["D:\\Projects\\0"]}
        }))?;
        assert!(!cache.entries.contains_key(&first_key));

        let newest_request = json!({
            "id": 90,
            "method": "skills/list",
            "params": {"cwds": [format!("D:\\Projects\\{}", SKILLS_LIST_CACHE_MAX_ENTRIES)]}
        });
        assert!(
            cache
                .begin_request(
                    &newest_request,
                    start + SKILLS_LIST_CACHE_TTL + Duration::from_secs(20),
                )?
                .is_none(),
            "an expired entry must miss"
        );

        let mut failed = SkillsListCacheState::default();
        let failed_request = json!({"id": 100, "method": "skills/list", "params": {}});
        assert!(failed.begin_request(&failed_request, start)?.is_none());
        assert!(!failed.complete_request(
            &id_key(&json!(100))?,
            &json!({"id": 100, "error": {"code": -1, "message": "fixture"}}),
            start,
        ));
        assert!(failed.entries.is_empty());

        let stale_request = json!({"id": 101, "method": "skills/list", "params": {}});
        assert!(failed.begin_request(&stale_request, start)?.is_none());
        failed.invalidate();
        assert!(!failed.complete_request(
            &id_key(&json!(101))?,
            &json!({"id": 101, "result": {"data": []}}),
            start,
        ));
        assert!(failed.entries.is_empty());

        let mut pending_only = SkillsListCacheState::default();
        for index in 0..=SKILLS_LIST_MAX_PENDING {
            let request = json!({
                "id": 1000 + index,
                "method": "skills/list",
                "params": {"cwds": [format!("D:\\Pending\\{index}")]}
            });
            assert!(pending_only.begin_request(&request, start)?.is_none());
        }
        assert_eq!(pending_only.pending.len(), SKILLS_LIST_MAX_PENDING);

        let mut concurrent = SkillsListCacheState::default();
        let seed = json!({
            "id": 200,
            "method": "skills/list",
            "params": {"cwds": ["D:\\Concurrent"]}
        });
        assert!(concurrent.begin_request(&seed, start)?.is_none());
        assert!(concurrent.complete_request(
            &id_key(&json!(200))?,
            &json!({"id": 200, "result": {"version": "old"}}),
            start,
        ));
        let forced = json!({
            "id": 201,
            "method": "skills/list",
            "params": {"cwds": ["D:\\Concurrent"], "forceReload": true}
        });
        assert!(concurrent.begin_request(&forced, start)?.is_none());
        let ordinary_during_reload = json!({
            "id": 202,
            "method": "skills/list",
            "params": {"cwds": ["D:\\Concurrent"], "forceReload": false}
        });
        assert!(
            concurrent
                .begin_request(&ordinary_during_reload, start)?
                .is_none(),
            "ordinary requests must not receive stale data during force reload"
        );
        assert!(!concurrent.complete_request(
            &id_key(&json!(202))?,
            &json!({"id": 202, "result": {"version": "stale-race"}}),
            start,
        ));
        assert!(concurrent.complete_request(
            &id_key(&json!(201))?,
            &json!({"id": 201, "result": {"version": "fresh"}}),
            start,
        ));
        let cached = concurrent
            .begin_request(
                &json!({
                    "id": 203,
                    "method": "skills/list",
                    "params": {"cwds": ["D:\\Concurrent"]}
                }),
                start,
            )?
            .context("force reload result should win a concurrent ordinary response")?;
        assert_eq!(cached["result"]["version"], "fresh");
        Ok(())
    }

    #[test]
    fn eager_read_uses_full_projection_while_fork_stays_blocked() -> Result<()> {
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
        let skills_list_cache = SkillsListCache::default();

        let ClientRoute::Respond(fork) = process_client_message(
            json!({"method": "thread/fork", "id": 4, "params": {"threadId": thread_id}}),
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
            &skills_list_cache,
        )?
        else {
            panic!("managed fork must be blocked")
        };
        assert_eq!(fork["error"]["code"], -32070);

        let ClientRoute::Forward(read) = process_client_message(
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
            &skills_list_cache,
        )?
        else {
            panic!("managed eager read must request metadata from the backend")
        };
        assert_eq!(read["params"]["includeTurns"], false);
        let duplicate_error = match process_client_message(
            json!({
                "method": "thread/resume",
                "id": 5,
                "params": {
                    "threadId": thread_id,
                    "excludeTurns": false
                }
            }),
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
            &skills_list_cache,
        ) {
            Ok(_) => panic!("duplicate pending request id should fail closed"),
            Err(error) => error,
        };
        assert!(
            duplicate_error
                .to_string()
                .contains("duplicate pending request id")
        );
        {
            let pending_requests = pending.lock().expect("pending map poisoned");
            assert!(matches!(
                pending_requests.get("5"),
                Some(PendingRequest::EagerRead(pending_read))
                    if pending_read.thread_id == thread_id
            ));
        }
        let read_response = process_backend_message(
            json!({
                "id": 5,
                "result": {
                    "thread": {
                        "id": thread_id,
                        "name": "managed thread",
                        "turns": []
                    }
                }
            }),
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
            &skills_list_cache,
        )?
        .context("managed eager read response should be projected")?;
        assert_eq!(
            read_response["result"]["thread"]["turns"][0]["id"],
            "turn-0"
        );

        let unmanaged_read = json!({
            "method": "thread/read",
            "id": 6,
            "params": {
                "threadId": "00000000-0000-7000-8000-000000000799",
                "includeTurns": true
            }
        });
        let ClientRoute::Forward(forwarded_unmanaged) = process_client_message(
            unmanaged_read.clone(),
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
            &skills_list_cache,
        )?
        else {
            panic!("unmanaged eager read should remain unchanged")
        };
        assert_eq!(forwarded_unmanaged, unmanaged_read);

        let ClientRoute::Forward(_) = process_client_message(
            json!({
                "method": "thread/read",
                "id": 7,
                "params": {"threadId": thread_id, "includeTurns": true}
            }),
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
            &skills_list_cache,
        )?
        else {
            panic!("managed eager read should request backend metadata")
        };
        let mismatched = process_backend_message(
            json!({
                "id": 7,
                "result": {
                    "thread": {
                        "id": "00000000-0000-7000-8000-000000000999",
                        "turns": []
                    }
                }
            }),
            temp.path(),
            &pending,
            &pending_tail_drain_gates,
            &tail_drain_gates,
            &client_owned_threads,
            &skills_list_cache,
        )
        .unwrap_err();
        assert!(
            mismatched
                .to_string()
                .contains("managed read response returned thread")
        );
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
        let skills_list_cache = SkillsListCache::default();
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
            &skills_list_cache,
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
            &skills_list_cache,
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
            &skills_list_cache,
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
            &skills_list_cache,
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
        let skills_list_cache = SkillsListCache::default();
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
            &skills_list_cache,
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
            &skills_list_cache,
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
            &skills_list_cache,
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
            &skills_list_cache,
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
        let skills_list_cache = SkillsListCache::default();
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
            &skills_list_cache,
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
                    &skills_list_cache,
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
                    &skills_list_cache,
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
                &skills_list_cache,
            )?
            .is_some()
        );
        Ok(())
    }
}
