use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

use crate::path_safety::remove_dir_all_scoped;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug)]
pub struct OracleProjection {
    pub thread_id: String,
    pub oracle_version: String,
    pub thread: Value,
    pub turns: Vec<Value>,
    pub resume_duration_ms: u128,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeCheckpointScan {
    pub source_path: String,
    pub source_bytes: u64,
    pub checkpoint_count: u64,
    pub latest_checkpoint_offset: Option<u64>,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeCompactionReport {
    pub thread_id: String,
    pub source_path: String,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub sha256_before: String,
    pub sha256_after: String,
    pub checkpoint_count_before: u64,
    pub checkpoint_count_after: u64,
    pub state: String,
}

#[derive(Clone, Debug)]
pub struct CodexOracle {
    backend: PathBuf,
    runtime_root: PathBuf,
    timeout: Duration,
}

impl CodexOracle {
    pub fn new(backend: PathBuf, runtime_root: PathBuf) -> Self {
        Self {
            backend,
            runtime_root,
            timeout: Duration::from_secs(600),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn project(&self, rollout: &Path) -> Result<OracleProjection> {
        if !self.backend.is_file() {
            bail!(
                "Codex oracle backend does not exist: {}",
                self.backend.display()
            );
        }
        let rollout = std::fs::canonicalize(rollout)
            .with_context(|| format!("failed to resolve {}", rollout.display()))?;
        let thread_id = read_rollout_thread_id(&rollout)?;
        let oracle_version = backend_version(&self.backend)?;
        let home = self.temporary_home()?;
        std::fs::create_dir_all(&home)?;
        std::fs::write(home.join("config.toml"), "")?;

        let result = self.project_inner(&rollout, &thread_id, &home, &oracle_version);
        let cleanup_root = self.runtime_root.join("Work").join("Oracle");
        let cleanup = remove_dir_all_scoped(&home, &cleanup_root, "Oracle temporary HOME cleanup");
        match (result, cleanup) {
            (Ok(projection), Ok(())) => Ok(projection),
            (Ok(_), Err(error)) => {
                Err(error).context("oracle succeeded but temporary home cleanup failed")
            }
            (Err(error), _) => Err(error),
        }
    }

    pub fn compact_with_native_backend(
        &self,
        rollout: &Path,
        codex_home: &Path,
        force: bool,
    ) -> Result<NativeCompactionReport> {
        let rollout = std::fs::canonicalize(rollout)
            .with_context(|| format!("failed to resolve {}", rollout.display()))?;
        let codex_home = std::fs::canonicalize(codex_home)
            .with_context(|| format!("failed to resolve {}", codex_home.display()))?;
        let thread_id = read_rollout_thread_id(&rollout)?;
        let before = scan_native_checkpoints(&rollout)?;
        let sha256_before = sha256_file(&rollout)?;
        if before.checkpoint_count > 0 && !force {
            return Ok(NativeCompactionReport {
                thread_id,
                source_path: rollout.to_string_lossy().into_owned(),
                bytes_before: before.source_bytes,
                bytes_after: before.source_bytes,
                sha256_before: sha256_before.clone(),
                sha256_after: sha256_before,
                checkpoint_count_before: before.checkpoint_count,
                checkpoint_count_after: before.checkpoint_count,
                state: "native_checkpoint_already_present".to_string(),
            });
        }
        self.compact_inner(&rollout, &thread_id, &codex_home)?;
        let after = scan_native_checkpoints(&rollout)?;
        let checkpoint_advanced = after.checkpoint_count > before.checkpoint_count
            || after.latest_checkpoint_offset > before.latest_checkpoint_offset;
        if !checkpoint_advanced {
            bail!("official compaction completed without persisting a newer native checkpoint");
        }
        let sha256_after = sha256_file(&rollout)?;
        Ok(NativeCompactionReport {
            thread_id,
            source_path: rollout.to_string_lossy().into_owned(),
            bytes_before: before.source_bytes,
            bytes_after: after.source_bytes,
            sha256_before,
            sha256_after,
            checkpoint_count_before: before.checkpoint_count,
            checkpoint_count_after: after.checkpoint_count,
            state: if force {
                "native_checkpoint_refreshed".to_string()
            } else {
                "native_checkpoint_created".to_string()
            },
        })
    }

    fn project_inner(
        &self,
        rollout: &Path,
        thread_id: &str,
        home: &Path,
        oracle_version: &str,
    ) -> Result<OracleProjection> {
        let oracle_cwd = self.runtime_root.join("Work").join("OracleCwd");
        std::fs::create_dir_all(&oracle_cwd)?;
        let mut command = Command::new(&self.backend);
        command
            .arg("app-server")
            .env("CODEX_HOME", home)
            .env_remove("CODEX_CLI_PATH")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to launch {}", self.backend.display()))?;
        let mut input = BufWriter::new(child.stdin.take().context("oracle stdin unavailable")?);
        let stdout = child.stdout.take().context("oracle stdout unavailable")?;
        let stderr = child.stderr.take().context("oracle stderr unavailable")?;
        let (line_tx, line_rx) = mpsc::channel();
        let stdout_thread = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if line_tx.send(Ok(line)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = line_tx.send(Err(error));
                        break;
                    }
                }
            }
        });
        let stderr_thread = thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut reader = BufReader::new(stderr);
            let _ = reader.read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        });

        let run = (|| -> Result<(Value, u128)> {
            write_message(
                &mut input,
                &json!({
                    "method": "initialize",
                    "id": 1,
                    "params": {
                        "clientInfo": {
                            "name": "conversation_lifecycle_manager",
                            "title": "Conversation Lifecycle Manager",
                            "version": env!("CARGO_PKG_VERSION")
                        },
                        "capabilities": {"experimentalApi": true}
                    }
                }),
            )?;
            let initialize = receive_response(&line_rx, &json!(1), self.timeout)?;
            ensure_success(&initialize, "initialize")?;
            write_message(&mut input, &json!({"method": "initialized", "params": {}}))?;
            let resume_started = Instant::now();
            write_message(
                &mut input,
                &json!({
                    "method": "thread/resume",
                    "id": 2,
                    "params": {
                        "threadId": thread_id,
                        "path": rollout,
                        "excludeTurns": false,
                        "cwd": oracle_cwd
                    }
                }),
            )?;
            let response = receive_response(&line_rx, &json!(2), self.timeout)?;
            Ok((response, resume_started.elapsed().as_millis()))
        })();

        drop(input);
        terminate(&mut child);
        let _ = stdout_thread.join();
        let stderr = stderr_thread.join().unwrap_or_default();
        let (response, resume_duration_ms) =
            run.with_context(|| format!("Codex oracle stderr:\n{stderr}"))?;
        ensure_success(&response, "thread/resume")?;
        let thread = response
            .get("result")
            .and_then(|value| value.get("thread"))
            .cloned()
            .context("oracle response has no result.thread")?;
        let returned_thread_id = thread
            .get("id")
            .and_then(Value::as_str)
            .context("oracle response thread has no id")?;
        if returned_thread_id != thread_id {
            bail!("oracle returned thread {returned_thread_id}, expected {thread_id}");
        }
        let turns = thread
            .get("turns")
            .and_then(Value::as_array)
            .cloned()
            .context("oracle response thread has no turns array")?;
        Ok(OracleProjection {
            thread_id: thread_id.to_string(),
            oracle_version: oracle_version.to_string(),
            thread,
            turns,
            resume_duration_ms,
        })
    }

    fn compact_inner(&self, rollout: &Path, thread_id: &str, codex_home: &Path) -> Result<()> {
        let mut command = Command::new(&self.backend);
        command
            .arg("app-server")
            .env("CODEX_HOME", codex_home)
            .env_remove("CODEX_CLI_PATH")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to launch {}", self.backend.display()))?;
        let mut input = BufWriter::new(
            child
                .stdin
                .take()
                .context("maintenance stdin unavailable")?,
        );
        let stdout = child
            .stdout
            .take()
            .context("maintenance stdout unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("maintenance stderr unavailable")?;
        let (line_tx, line_rx) = mpsc::channel();
        let stdout_thread = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if line_tx.send(Ok(line)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = line_tx.send(Err(error));
                        break;
                    }
                }
            }
        });
        let stderr_thread = thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut reader = BufReader::new(stderr);
            let _ = reader.read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        });

        let run = (|| -> Result<()> {
            write_message(
                &mut input,
                &json!({
                    "method": "initialize",
                    "id": 1,
                    "params": {
                        "clientInfo": {
                            "name": "conversation_lifecycle_manager",
                            "title": "Conversation Lifecycle Manager",
                            "version": env!("CARGO_PKG_VERSION")
                        },
                        "capabilities": {"experimentalApi": true}
                    }
                }),
            )?;
            ensure_success(
                &receive_response(&line_rx, &json!(1), self.timeout)?,
                "initialize",
            )?;
            write_message(&mut input, &json!({"method": "initialized", "params": {}}))?;
            write_message(
                &mut input,
                &json!({
                    "method": "thread/resume",
                    "id": 2,
                    "params": {
                        "threadId": thread_id,
                        "path": rollout,
                        "excludeTurns": true
                    }
                }),
            )?;
            ensure_success(
                &receive_response(&line_rx, &json!(2), self.timeout)?,
                "thread/resume",
            )?;
            write_message(
                &mut input,
                &json!({
                    "method": "thread/compact/start",
                    "id": 3,
                    "params": {"threadId": thread_id}
                }),
            )?;
            ensure_success(
                &receive_response(&line_rx, &json!(3), self.timeout)?,
                "thread/compact/start",
            )?;
            wait_for_compaction_completion(&line_rx, thread_id, self.timeout)
        })();

        drop(input);
        thread::sleep(Duration::from_millis(500));
        terminate(&mut child);
        let _ = stdout_thread.join();
        let stderr = stderr_thread.join().unwrap_or_default();
        run.with_context(|| format!("Codex native compaction stderr:\n{stderr}"))
    }

    fn temporary_home(&self) -> Result<PathBuf> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let root = self.runtime_root.join("Work").join("Oracle");
        Ok(root.join(format!("{}-{nonce}", std::process::id())))
    }
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)
        .with_context(|| format!("failed to open {} for hashing", path.display()))?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub fn read_rollout_thread_id(path: &Path) -> Result<String> {
    let file = File::open(path)?;
    for (line_number, line) in BufReader::new(file).lines().take(100).enumerate() {
        let line = line?;
        let value: Value = serde_json::from_str(&line)
            .with_context(|| format!("invalid JSON at rollout line {}", line_number + 1))?;
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let payload = value
            .get("payload")
            .context("session_meta has no payload")?;
        let thread_id = payload
            .get("id")
            .or_else(|| payload.get("meta").and_then(|meta| meta.get("id")))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .context("session_meta has no thread id")?;
        return Ok(thread_id.to_string());
    }
    bail!("no session_meta thread id found in first 100 rollout records")
}

pub fn scan_native_checkpoints(path: &Path) -> Result<NativeCheckpointScan> {
    let canonical = std::fs::canonicalize(path)?;
    let source_bytes = std::fs::metadata(&canonical)?.len();
    let file = File::open(&canonical)?;
    let mut reader = BufReader::new(file);
    let mut offset = 0_u64;
    let mut checkpoint_count = 0_u64;
    let mut latest_checkpoint_offset = None;
    loop {
        let line_offset = offset;
        let mut line = Vec::new();
        let bytes = reader.read_until(b'\n', &mut line)?;
        if bytes == 0 {
            break;
        }
        let has_newline = line.last() == Some(&b'\n');
        let json_bytes = trim_json_line(&line);
        if json_bytes.iter().all(u8::is_ascii_whitespace) {
            offset += bytes as u64;
            continue;
        }
        let value = match serde_json::from_slice::<Value>(json_bytes) {
            Ok(value) => value,
            Err(_) if !has_newline => break,
            Err(error) => bail!("invalid rollout JSON at byte {line_offset}: {error}"),
        };
        if value.get("type").and_then(Value::as_str) == Some("compacted")
            && value
                .get("payload")
                .and_then(|payload| payload.get("replacement_history"))
                .is_some_and(Value::is_array)
        {
            checkpoint_count += 1;
            latest_checkpoint_offset = Some(line_offset);
        }
        offset += bytes as u64;
    }
    Ok(NativeCheckpointScan {
        source_path: canonical.to_string_lossy().into_owned(),
        source_bytes,
        checkpoint_count,
        latest_checkpoint_offset,
    })
}

fn backend_version(backend: &Path) -> Result<String> {
    let mut command = Command::new(backend);
    command.arg("--version").env_remove("CODEX_CLI_PATH");
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    let output = command.output()?;
    if !output.status.success() {
        bail!("Codex backend --version failed with {}", output.status);
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn write_message(writer: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn receive_response(
    receiver: &mpsc::Receiver<std::io::Result<String>>,
    expected_id: &Value,
    timeout: Duration,
) -> Result<Value> {
    loop {
        let line = receiver
            .recv_timeout(timeout)
            .with_context(|| format!("timed out waiting for oracle response id {expected_id}"))??;
        let value: Value = serde_json::from_str(line.trim_end())
            .context("oracle emitted invalid JSON on stdout")?;
        if value.get("id") == Some(expected_id) {
            return Ok(value);
        }
    }
}

fn ensure_success(response: &Value, operation: &str) -> Result<()> {
    if let Some(error) = response.get("error") {
        bail!("oracle {operation} failed: {error}");
    }
    if response.get("result").is_none() {
        bail!("oracle {operation} response has neither result nor error");
    }
    Ok(())
}

fn wait_for_compaction_completion(
    receiver: &mpsc::Receiver<std::io::Result<String>>,
    thread_id: &str,
    timeout: Duration,
) -> Result<()> {
    loop {
        let line = receiver
            .recv_timeout(timeout)
            .context("timed out waiting for native compaction completion")??;
        let value: Value = serde_json::from_str(line.trim_end())?;
        if value.get("method").and_then(Value::as_str) != Some("turn/completed") {
            continue;
        }
        let params = value
            .get("params")
            .context("turn/completed has no params")?;
        if params.get("threadId").and_then(Value::as_str) != Some(thread_id) {
            continue;
        }
        let status = params
            .get("turn")
            .and_then(|turn| turn.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if status == "failed" {
            bail!("native compaction turn failed: {params}");
        }
        return Ok(());
    }
}

fn trim_json_line(mut line: &[u8]) -> &[u8] {
    if let Some(stripped) = line.strip_suffix(b"\n") {
        line = stripped;
    }
    if let Some(stripped) = line.strip_suffix(b"\r") {
        line = stripped;
    }
    line
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}
