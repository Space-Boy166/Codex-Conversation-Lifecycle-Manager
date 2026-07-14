use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::thread;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use clap::Subcommand;
use conversation_lifecycle_manager::ConversationInventoryItem;
use conversation_lifecycle_manager::ConversationLifecycleState;
use conversation_lifecycle_manager::MigrationManifest;
use conversation_lifecycle_manager::apply_migration;
use conversation_lifecycle_manager::ensure_codex_closed;
use conversation_lifecycle_manager::prepare_migration;
use conversation_lifecycle_manager::rehydrate_migration;
use conversation_lifecycle_manager::scan_codex_conversations;
use conversation_lifecycle_manager::scan_native_checkpoints;
use conversation_lifecycle_manager::sha256_file;
use serde::Deserialize;
use serde::Serialize;

const DEFAULT_MINIMUM_MIB: u64 = 64;
const STABILITY_WAIT_SECONDS: u64 = 8;

#[derive(Parser)]
#[command(
    name = "CLMSetup",
    version,
    about = "One-click lazy history setup for Codex Desktop on Windows"
)]
struct Cli {
    #[arg(long, global = true)]
    codex_home: Option<PathBuf>,
    #[arg(long, global = true)]
    runtime_root: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<SetupCommand>,
}

#[derive(Subcommand)]
enum SetupCommand {
    Scan {
        #[arg(long, default_value_t = DEFAULT_MINIMUM_MIB)]
        minimum_mib: u64,
        #[arg(long)]
        json: bool,
    },
    Enable {
        #[arg(long)]
        thread_id: Option<String>,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        no_relaunch: bool,
    },
    Restore {
        #[arg(long)]
        thread_id: Option<String>,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        no_relaunch: bool,
    },
    Doctor {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StorePackage {
    version: String,
    install_location: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct UserEnvironment {
    code_cli_path: Option<String>,
    clm_runtime_root: Option<String>,
    clm_codex_backend: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SetupState {
    format_version: u32,
    installed_at_unix_ms: u128,
    package_version: String,
    backend_path: String,
    backend_sha256: String,
    proxy_path: String,
    previous_environment: UserEnvironment,
}

#[derive(Clone, Debug)]
struct InstalledRuntime {
    backend: PathBuf,
    proxy: PathBuf,
    chatgpt: PathBuf,
    state: SetupState,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DoctorReport {
    codex_home: String,
    runtime_root: String,
    package: StorePackage,
    long_conversations: usize,
    lazy_history_enabled: usize,
    prepared: usize,
    needs_inspection: usize,
    proxy_installed: bool,
    setup_state_present: bool,
}

fn main() {
    let launched_without_arguments = std::env::args_os().len() == 1;
    let result = run(launched_without_arguments);
    if let Err(error) = result {
        eprintln!("\nCLM Setup failed:\n{error:#}");
        if launched_without_arguments {
            pause();
        }
        std::process::exit(1);
    }
}

fn run(interactive: bool) -> Result<()> {
    let cli = Cli::parse();
    let codex_home = resolve_codex_home(cli.codex_home)?;
    let runtime_root = resolve_runtime_root(cli.runtime_root)?;
    match cli.command {
        Some(SetupCommand::Scan { minimum_mib, json }) => {
            run_scan(&codex_home, &runtime_root, minimum_mib, json)
        }
        Some(SetupCommand::Enable {
            thread_id,
            yes,
            no_relaunch,
        }) => run_enable(
            &codex_home,
            &runtime_root,
            thread_id.as_deref(),
            yes,
            no_relaunch,
        ),
        Some(SetupCommand::Restore {
            thread_id,
            yes,
            no_relaunch,
        }) => run_restore(
            &codex_home,
            &runtime_root,
            thread_id.as_deref(),
            yes,
            no_relaunch,
        ),
        Some(SetupCommand::Doctor { json }) => run_doctor(&codex_home, &runtime_root, json),
        None if interactive => run_interactive(&codex_home, &runtime_root),
        None => unreachable!(),
    }
}

fn run_interactive(codex_home: &Path, runtime_root: &Path) -> Result<()> {
    println!("Conversation Lifecycle Manager");
    println!("Lazy history setup for Codex Desktop\n");
    println!("1. Enable lazy history for a long conversation");
    println!("2. Restore a conversation to its original full file");
    println!("3. Scan and show status");
    println!("4. Exit");
    let action = prompt("Choose an action")?;
    match action.trim() {
        "1" => run_enable(codex_home, runtime_root, None, false, false),
        "2" => run_restore(codex_home, runtime_root, None, false, false),
        "3" => {
            run_scan(codex_home, runtime_root, DEFAULT_MINIMUM_MIB, false)?;
            pause();
            Ok(())
        }
        "4" => Ok(()),
        _ => bail!("unknown menu choice"),
    }
}

fn run_scan(codex_home: &Path, runtime_root: &Path, minimum_mib: u64, json: bool) -> Result<()> {
    let minimum_bytes = minimum_mib.saturating_mul(1024 * 1024);
    let items = scan_codex_conversations(codex_home, runtime_root, minimum_bytes)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else {
        print_inventory(&items);
    }
    Ok(())
}

fn run_enable(
    codex_home: &Path,
    runtime_root: &Path,
    requested_thread_id: Option<&str>,
    assume_yes: bool,
    no_relaunch: bool,
) -> Result<()> {
    let items = scan_codex_conversations(codex_home, runtime_root, 0)?;
    let eligible: Vec<_> = items
        .iter()
        .filter(|item| {
            matches!(
                item.state,
                ConversationLifecycleState::Original | ConversationLifecycleState::Prepared
            ) && (requested_thread_id.is_some() || item.bytes >= DEFAULT_MINIMUM_MIB * 1024 * 1024)
        })
        .cloned()
        .collect();
    let selected = select_conversation(
        &eligible,
        requested_thread_id,
        "Select a conversation to enable",
    )?;
    if selected.state == ConversationLifecycleState::Original {
        let checkpoint = scan_native_checkpoints(Path::new(&selected.rollout_path))?;
        if checkpoint.checkpoint_count == 0 {
            bail!(
                "this conversation has no native Codex replacement-history checkpoint; CLM refuses to invent one"
            );
        }
    }
    let package = detect_store_package()?;
    println!("\nSelected: {}", selected.title);
    print_size_summary(&selected);
    println!("Codex Store: {}", package.version);
    println!("CLM will preserve a full archive and a same-volume rollback copy.");
    println!("All Codex Desktop windows must close during activation.");
    if !assume_yes && !confirm("Enable lazy history now")? {
        println!("No changes were made.");
        return Ok(());
    }

    close_codex_gracefully(&package)?;
    let required_history_bytes = if selected.state == ConversationLifecycleState::Original {
        selected.bytes
    } else {
        0
    };
    ensure_runtime_space(runtime_root, required_history_bytes)?;
    let installed = install_runtime(&package, runtime_root)?;
    let (manifest_path, manifest) = if selected.state == ConversationLifecycleState::Prepared {
        let path = selected
            .manifest_path
            .as_ref()
            .map(PathBuf::from)
            .context("prepared conversation has no activation manifest")?;
        let manifest = serde_json::from_reader::<_, MigrationManifest>(std::fs::File::open(&path)?)
            .context("prepared activation manifest is invalid")?;
        (path, manifest)
    } else {
        println!("Preparing verified archive and index. This can take a while...");
        prepare_migration(
            Path::new(&selected.rollout_path),
            installed.backend.clone(),
            runtime_root.to_path_buf(),
            false,
        )?
    };
    println!(
        "Prepared {} -> {} ({} turns).",
        format_bytes(manifest.source_bytes),
        format_bytes(manifest.candidate_bytes),
        manifest.full_turns
    );
    let applied = apply_migration(&manifest_path, false)?;
    if let Err(error) = set_clm_environment(runtime_root, &installed) {
        let restore = rehydrate_migration(&manifest_path, false);
        return match restore {
            Ok(_) => Err(error).context("environment activation failed; original history restored"),
            Err(restore_error) => Err(anyhow::anyhow!(
                "environment activation failed ({error}); automatic history restore also failed ({restore_error})"
            )),
        };
    }

    if !no_relaunch {
        launch_codex(&installed.chatgpt, runtime_root, &installed)?;
        thread::sleep(Duration::from_secs(STABILITY_WAIT_SECONDS));
        if !process_is_running("codex-clm-proxy.exe")? {
            close_codex_gracefully(&package)?;
            let restore = rehydrate_migration(&manifest_path, false);
            let env_restore = restore_user_environment(&installed.state.previous_environment);
            match (restore, env_restore) {
                (Ok(_), Ok(())) => {
                    bail!("Codex did not start through the CLM proxy; original history restored")
                }
                (history, environment) => bail!(
                    "Codex did not start through the CLM proxy; recovery results: history={history:?}, environment={environment:?}"
                ),
            }
        }
    }

    println!("\nLazy history enabled.");
    println!("Thread: {}", applied.thread_id);
    println!("Active file: {}", format_bytes(applied.active_bytes));
    println!("The full original and rollback copy remain available.");
    Ok(())
}

fn run_restore(
    codex_home: &Path,
    runtime_root: &Path,
    requested_thread_id: Option<&str>,
    assume_yes: bool,
    no_relaunch: bool,
) -> Result<()> {
    let items = scan_codex_conversations(codex_home, runtime_root, 0)?;
    let enabled: Vec<_> = items
        .iter()
        .filter(|item| item.state == ConversationLifecycleState::LazyHistoryEnabled)
        .cloned()
        .collect();
    let selected = select_conversation(
        &enabled,
        requested_thread_id,
        "Select a conversation to restore",
    )?;
    let manifest_path = selected
        .manifest_path
        .as_ref()
        .map(PathBuf::from)
        .context("selected conversation has no activation manifest")?;
    let package = detect_store_package()?;
    println!("\nSelected: {}", selected.title);
    println!("CLM will merge all post-activation turns into the full original history.");
    if !assume_yes && !confirm("Restore the original full-file layout now")? {
        println!("No changes were made.");
        return Ok(());
    }

    close_codex_gracefully(&package)?;
    let report = rehydrate_migration(&manifest_path, false)?;
    let remaining = scan_codex_conversations(codex_home, runtime_root, 0)?
        .into_iter()
        .filter(|item| item.state == ConversationLifecycleState::LazyHistoryEnabled)
        .count();
    let state = read_setup_state(runtime_root)?;
    if remaining == 0 {
        restore_user_environment(&state.previous_environment)?;
    }
    if !no_relaunch {
        launch_codex_with_environment(
            &PathBuf::from(package.install_location)
                .join("app")
                .join("ChatGPT.exe"),
            &state.previous_environment,
            remaining > 0,
            runtime_root,
            &state,
        )?;
    }
    println!("\nOriginal history restored without losing new turns.");
    println!("Restored: {}", format_bytes(report.restored_bytes));
    println!(
        "Post-activation data preserved: {}",
        format_bytes(report.appended_bytes)
    );
    Ok(())
}

fn run_doctor(codex_home: &Path, runtime_root: &Path, json: bool) -> Result<()> {
    let package = detect_store_package()?;
    let items = scan_codex_conversations(codex_home, runtime_root, 64 * 1024 * 1024)?;
    let report = DoctorReport {
        codex_home: codex_home.to_string_lossy().into_owned(),
        runtime_root: runtime_root.to_string_lossy().into_owned(),
        package,
        long_conversations: items.len(),
        lazy_history_enabled: items
            .iter()
            .filter(|item| item.state == ConversationLifecycleState::LazyHistoryEnabled)
            .count(),
        prepared: items
            .iter()
            .filter(|item| item.state == ConversationLifecycleState::Prepared)
            .count(),
        needs_inspection: items
            .iter()
            .filter(|item| item.state == ConversationLifecycleState::NeedsInspection)
            .count(),
        proxy_installed: runtime_root
            .join("bin")
            .join("codex-clm-proxy.exe")
            .is_file(),
        setup_state_present: setup_state_path(runtime_root).is_file(),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Codex Store: {}", report.package.version);
        println!("Codex home: {}", report.codex_home);
        println!("Runtime root: {}", report.runtime_root);
        println!("Long conversations: {}", report.long_conversations);
        println!("Lazy history enabled: {}", report.lazy_history_enabled);
        println!("Prepared: {}", report.prepared);
        println!("Needs inspection: {}", report.needs_inspection);
        println!("Proxy installed: {}", report.proxy_installed);
    }
    Ok(())
}

fn resolve_codex_home(value: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(value) = value {
        return Ok(value);
    }
    if let Some(value) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(value));
    }
    let profile = std::env::var_os("USERPROFILE").context("USERPROFILE is not set")?;
    Ok(PathBuf::from(profile).join(".codex"))
}

fn resolve_runtime_root(value: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(value) = value {
        return Ok(value);
    }
    if let Some(value) = std::env::var_os("CLM_RUNTIME_ROOT") {
        return Ok(PathBuf::from(value));
    }
    let local = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    Ok(PathBuf::from(local).join("ConversationLifecycleManager"))
}

fn detect_store_package() -> Result<StorePackage> {
    let script = concat!(
        "$ErrorActionPreference='Stop';",
        "[Console]::OutputEncoding=[Text.UTF8Encoding]::new();",
        "$p=Get-AppxPackage -Name OpenAI.Codex | Sort-Object Version -Descending | Select-Object -First 1;",
        "if(-not $p){throw 'OpenAI Codex Store package is not installed'};",
        "[pscustomobject]@{version=$p.Version.ToString();installLocation=$p.InstallLocation}|ConvertTo-Json -Compress"
    );
    let output = powershell(script, std::iter::empty::<(&str, &OsStr)>())?;
    serde_json::from_str(output.trim_start_matches('\u{feff}').trim())
        .context("failed to parse Codex Store package metadata")
}

fn install_runtime(package: &StorePackage, runtime_root: &Path) -> Result<InstalledRuntime> {
    ensure_codex_closed()?;
    let release_root = std::env::current_exe()?
        .parent()
        .context("CLMSetup has no parent directory")?
        .to_path_buf();
    let source_proxy = release_root.join("codex-clm-proxy.exe");
    if !source_proxy.is_file() {
        bail!(
            "release package is incomplete: {} is missing",
            source_proxy.display()
        );
    }
    let install_root = PathBuf::from(&package.install_location);
    let source_backend = install_root.join("app").join("resources").join("codex.exe");
    let chatgpt = install_root.join("app").join("ChatGPT.exe");
    if !source_backend.is_file() || !chatgpt.is_file() {
        bail!("Codex Store package layout is not supported");
    }

    let bin_root = runtime_root.join("bin");
    let backend_root = runtime_root.join("Backend").join(&package.version);
    std::fs::create_dir_all(&bin_root)?;
    std::fs::create_dir_all(&backend_root)?;
    std::fs::create_dir_all(runtime_root.join("Data"))?;
    let proxy = bin_root.join("codex-clm-proxy.exe");
    let backend = backend_root.join("codex.exe");
    install_verified_file(&source_proxy, &proxy)?;
    install_verified_file(&source_backend, &backend)?;

    let state_path = setup_state_path(runtime_root);
    let previous_environment = if state_path.is_file() {
        read_setup_state(runtime_root)?.previous_environment
    } else {
        let environment = read_user_environment()?;
        if let Some(existing) = environment
            .code_cli_path
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            && !paths_equal(Path::new(existing), &proxy)
        {
            bail!(
                "CODEX_CLI_PATH already points to another wrapper ({existing}); CLM refuses to overwrite it automatically"
            );
        }
        environment
    };
    let state = SetupState {
        format_version: 1,
        installed_at_unix_ms: SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
        package_version: package.version.clone(),
        backend_path: backend.to_string_lossy().into_owned(),
        backend_sha256: sha256_file(&backend)?,
        proxy_path: proxy.to_string_lossy().into_owned(),
        previous_environment,
    };
    write_json_atomic(&state_path, &state)?;
    Ok(InstalledRuntime {
        backend,
        proxy,
        chatgpt,
        state,
    })
}

fn install_verified_file(source: &Path, destination: &Path) -> Result<()> {
    let source_hash = sha256_file(source)?;
    if destination.is_file() && sha256_file(destination)? == source_hash {
        return Ok(());
    }
    let temporary = destination.with_extension("exe.clm-new");
    if temporary.exists() {
        std::fs::remove_file(&temporary)?;
    }
    std::fs::copy(source, &temporary)?;
    if sha256_file(&temporary)? != source_hash {
        bail!("runtime copy hash mismatch: {}", temporary.display());
    }
    if destination.exists() {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        let backup = destination.with_extension(format!("exe.previous-{stamp}"));
        std::fs::rename(destination, backup)?;
    }
    std::fs::rename(temporary, destination)?;
    Ok(())
}

fn ensure_runtime_space(runtime_root: &Path, source_bytes: u64) -> Result<()> {
    std::fs::create_dir_all(runtime_root)?;
    let reserve = source_bytes
        .saturating_add(source_bytes / 3)
        .saturating_add(512 * 1024 * 1024);
    let free = query_free_space(runtime_root)?;
    if free < reserve {
        bail!(
            "not enough free space in the CLM runtime volume: need about {}, available {}",
            format_bytes(reserve),
            format_bytes(free)
        );
    }
    Ok(())
}

fn query_free_space(path: &Path) -> Result<u64> {
    let output = powershell(
        concat!(
            "$ErrorActionPreference='Stop';",
            "$root=[IO.Path]::GetPathRoot($env:CLM_SPACE_PATH);",
            "$name=$root.TrimEnd('\\').TrimEnd(':');",
            "[string](Get-PSDrive -Name $name).Free"
        ),
        [("CLM_SPACE_PATH", path.as_os_str())],
    )?;
    output
        .trim()
        .parse()
        .context("failed to read free disk space")
}

fn read_user_environment() -> Result<UserEnvironment> {
    let output = powershell(
        concat!(
            "[Console]::OutputEncoding=[Text.UTF8Encoding]::new();",
            "[pscustomobject]@{",
            "codeCliPath=[Environment]::GetEnvironmentVariable('CODEX_CLI_PATH','User');",
            "clmRuntimeRoot=[Environment]::GetEnvironmentVariable('CLM_RUNTIME_ROOT','User');",
            "clmCodexBackend=[Environment]::GetEnvironmentVariable('CLM_CODEX_BACKEND','User')",
            "}|ConvertTo-Json -Compress"
        ),
        std::iter::empty::<(&str, &OsStr)>(),
    )?;
    serde_json::from_str(output.trim_start_matches('\u{feff}').trim())
        .context("failed to parse the current user environment")
}

fn set_clm_environment(runtime_root: &Path, installed: &InstalledRuntime) -> Result<()> {
    powershell(
        concat!(
            "$ErrorActionPreference='Stop';",
            "[Environment]::SetEnvironmentVariable('CLM_RUNTIME_ROOT',$env:CLM_SET_ROOT,'User');",
            "[Environment]::SetEnvironmentVariable('CLM_CODEX_BACKEND',$env:CLM_SET_BACKEND,'User');",
            "[Environment]::SetEnvironmentVariable('CODEX_CLI_PATH',$env:CLM_SET_PROXY,'User')"
        ),
        [
            ("CLM_SET_ROOT", runtime_root.as_os_str()),
            ("CLM_SET_BACKEND", installed.backend.as_os_str()),
            ("CLM_SET_PROXY", installed.proxy.as_os_str()),
        ],
    )?;
    Ok(())
}

fn restore_user_environment(environment: &UserEnvironment) -> Result<()> {
    let root = environment.clm_runtime_root.as_deref().unwrap_or("");
    let backend = environment.clm_codex_backend.as_deref().unwrap_or("");
    let proxy = environment.code_cli_path.as_deref().unwrap_or("");
    powershell(
        concat!(
            "$ErrorActionPreference='Stop';",
            "$root=if($env:CLM_PREV_ROOT){$env:CLM_PREV_ROOT}else{$null};",
            "$backend=if($env:CLM_PREV_BACKEND){$env:CLM_PREV_BACKEND}else{$null};",
            "$proxy=if($env:CLM_PREV_PROXY){$env:CLM_PREV_PROXY}else{$null};",
            "[Environment]::SetEnvironmentVariable('CLM_RUNTIME_ROOT',$root,'User');",
            "[Environment]::SetEnvironmentVariable('CLM_CODEX_BACKEND',$backend,'User');",
            "[Environment]::SetEnvironmentVariable('CODEX_CLI_PATH',$proxy,'User')"
        ),
        [
            ("CLM_PREV_ROOT", OsStr::new(root)),
            ("CLM_PREV_BACKEND", OsStr::new(backend)),
            ("CLM_PREV_PROXY", OsStr::new(proxy)),
        ],
    )?;
    Ok(())
}

fn close_codex_gracefully(package: &StorePackage) -> Result<()> {
    if ensure_codex_closed().is_ok() {
        return Ok(());
    }
    println!("Requesting Codex Desktop to close...");
    powershell(
        concat!(
            "$root=$env:CLM_CODEX_INSTALL;",
            "$owners=Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | Where-Object {",
            "$_.Name -eq 'ChatGPT.exe' -and $_.ExecutablePath -like ($root+'*')",
            "};",
            "foreach($owner in $owners){",
            "$p=Get-Process -Id $owner.ProcessId -ErrorAction SilentlyContinue;",
            "if($p -and $p.MainWindowHandle -ne 0){[void]$p.CloseMainWindow()}",
            "}"
        ),
        [("CLM_CODEX_INSTALL", OsStr::new(&package.install_location))],
    )?;
    for _ in 0..30 {
        if ensure_codex_closed().is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(1));
    }
    bail!("Codex is still running; close its remaining windows/processes and run CLMSetup again")
}

fn launch_codex(chatgpt: &Path, runtime_root: &Path, installed: &InstalledRuntime) -> Result<()> {
    ProcessCommand::new(chatgpt)
        .env("CLM_RUNTIME_ROOT", runtime_root)
        .env("CLM_CODEX_BACKEND", &installed.backend)
        .env("CODEX_CLI_PATH", &installed.proxy)
        .spawn()
        .with_context(|| format!("failed to launch {}", chatgpt.display()))?;
    Ok(())
}

fn launch_codex_with_environment(
    chatgpt: &Path,
    environment: &UserEnvironment,
    keep_clm: bool,
    runtime_root: &Path,
    state: &SetupState,
) -> Result<()> {
    let mut command = ProcessCommand::new(chatgpt);
    if keep_clm {
        command
            .env("CLM_RUNTIME_ROOT", runtime_root)
            .env("CLM_CODEX_BACKEND", &state.backend_path)
            .env("CODEX_CLI_PATH", &state.proxy_path);
    } else {
        apply_child_environment(
            &mut command,
            "CLM_RUNTIME_ROOT",
            &environment.clm_runtime_root,
        );
        apply_child_environment(
            &mut command,
            "CLM_CODEX_BACKEND",
            &environment.clm_codex_backend,
        );
        apply_child_environment(&mut command, "CODEX_CLI_PATH", &environment.code_cli_path);
    }
    command
        .spawn()
        .with_context(|| format!("failed to launch {}", chatgpt.display()))?;
    Ok(())
}

fn apply_child_environment(command: &mut ProcessCommand, name: &str, value: &Option<String>) {
    if let Some(value) = value {
        command.env(name, value);
    } else {
        command.env_remove(name);
    }
}

fn process_is_running(name: &str) -> Result<bool> {
    let output = ProcessCommand::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {name}"), "/FO", "CSV", "/NH"])
        .output()?;
    if !output.status.success() {
        bail!("tasklist failed while checking {name}");
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .to_ascii_lowercase()
        .contains(&name.to_ascii_lowercase()))
}

fn powershell<I, K, V>(script: &str, environment: I) -> Result<String>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let output = ProcessCommand::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .envs(environment)
        .output()
        .context("failed to start Windows PowerShell")?;
    if !output.status.success() {
        bail!(
            "PowerShell operation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn setup_state_path(runtime_root: &Path) -> PathBuf {
    runtime_root.join("Data").join("setup-state.json")
}

fn read_setup_state(runtime_root: &Path) -> Result<SetupState> {
    let path = setup_state_path(runtime_root);
    serde_json::from_reader(std::fs::File::open(&path)?)
        .with_context(|| format!("failed to read {}", path.display()))
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.clm-new");
    let bytes = serde_json::to_vec_pretty(value)?;
    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }
    let previous = path.with_extension("json.previous");
    if previous.exists() {
        std::fs::remove_file(&previous)?;
    }
    if path.exists() {
        std::fs::rename(path, &previous)?;
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        if previous.exists() {
            let _ = std::fs::rename(&previous, path);
        }
        return Err(error).context("failed to activate setup state");
    }
    Ok(())
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

fn select_conversation(
    items: &[ConversationInventoryItem],
    requested_thread_id: Option<&str>,
    heading: &str,
) -> Result<ConversationInventoryItem> {
    if let Some(thread_id) = requested_thread_id {
        return items
            .iter()
            .find(|item| item.thread_id == thread_id)
            .cloned()
            .with_context(|| format!("thread is not eligible for this action: {thread_id}"));
    }
    if items.is_empty() {
        bail!("no eligible conversations were found");
    }
    println!("\n{heading}:\n");
    for (index, item) in items.iter().enumerate() {
        println!(
            "{:>3}. {:>10}  {}",
            index + 1,
            format_bytes(item.bytes),
            truncate(&item.title, 88)
        );
    }
    let choice: usize = prompt("Enter the conversation number")?
        .trim()
        .parse()
        .context("conversation number is invalid")?;
    items
        .get(choice.saturating_sub(1))
        .cloned()
        .context("conversation number is out of range")
}

fn print_inventory(items: &[ConversationInventoryItem]) {
    if items.is_empty() {
        println!("No conversations matched the size threshold.");
        return;
    }
    for item in items {
        let size = if item.active_bytes == item.bytes {
            format_bytes(item.bytes)
        } else {
            format!(
                "{} -> {}",
                format_bytes(item.bytes),
                format_bytes(item.active_bytes)
            )
        };
        println!(
            "{:>24}  {:<20?}  {}\n                           {}",
            size,
            item.state,
            truncate(&item.title, 88),
            item.thread_id
        );
    }
}

fn print_size_summary(item: &ConversationInventoryItem) {
    if item.active_bytes == item.bytes {
        println!("History size: {}", format_bytes(item.bytes));
    } else {
        println!("Original history: {}", format_bytes(item.bytes));
        println!("Active hot file: {}", format_bytes(item.active_bytes));
    }
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}: ");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input)
}

fn confirm(label: &str) -> Result<bool> {
    Ok(matches!(
        prompt(&format!("{label} [y/N]"))?
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "y" | "yes"
    ))
}

fn pause() {
    let _ = prompt("Press Enter to close");
}

fn truncate(value: &str, maximum_chars: usize) -> String {
    let normalized = value.replace(['\r', '\n'], " ");
    if normalized.chars().count() <= maximum_chars {
        return normalized;
    }
    let mut output: String = normalized
        .chars()
        .take(maximum_chars.saturating_sub(3))
        .collect();
    output.push_str("...");
    output
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let value = bytes as f64;
    if value >= GIB {
        format!("{:.2} GiB", value / GIB)
    } else if value >= MIB {
        format!("{:.1} MiB", value / MIB)
    } else if value >= KIB {
        format!("{:.1} KiB", value / KIB)
    } else {
        format!("{bytes} B")
    }
}
