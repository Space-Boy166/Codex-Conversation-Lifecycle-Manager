use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use conversation_lifecycle_manager::CodexOracle;
use conversation_lifecycle_manager::FixtureOptions;
use conversation_lifecycle_manager::IndexedRollout;
use conversation_lifecycle_manager::ItemsView;
use conversation_lifecycle_manager::SortDirection;
use conversation_lifecycle_manager::apply_migration;
use conversation_lifecycle_manager::build_active_candidate;
use conversation_lifecycle_manager::create_native_checkpoint_offline;
use conversation_lifecycle_manager::default_codex_home;
use conversation_lifecycle_manager::default_runtime_root;
use conversation_lifecycle_manager::generate_fixture;
use conversation_lifecycle_manager::prepare_migration;
use conversation_lifecycle_manager::rehydrate_migration;
use conversation_lifecycle_manager::rollback_migration;
use conversation_lifecycle_manager::scan_native_checkpoints;
use conversation_lifecycle_manager::sha256_file;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Index {
        #[arg(long)]
        rollout: PathBuf,
        #[arg(long)]
        db: PathBuf,
    },
    Turns {
        #[arg(long)]
        db: PathBuf,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, value_enum, default_value_t = CliSort::Desc)]
        sort: CliSort,
        #[arg(long, value_enum, default_value_t = CliItemsView::Summary)]
        items: CliItemsView,
    },
    Items {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        turn_id: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, value_enum, default_value_t = CliSort::Asc)]
        sort: CliSort,
    },
    ResumeWindow {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    GenerateFixture {
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 100)]
        turns: usize,
        #[arg(long, default_value_t = 10)]
        tail_after_checkpoint: usize,
        #[arg(long, default_value_t = 256)]
        payload_bytes: usize,
        #[arg(long, default_value = "paginated")]
        history_mode: String,
    },
    OracleProject {
        #[arg(long)]
        rollout: PathBuf,
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        backend: PathBuf,
        #[arg(long)]
        runtime_root: Option<PathBuf>,
    },
    BuildCandidate {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    PrepareMigration {
        #[arg(long)]
        rollout: PathBuf,
        #[arg(long)]
        backend: PathBuf,
        #[arg(long)]
        runtime_root: Option<PathBuf>,
        #[arg(long)]
        fixture: bool,
    },
    ApplyMigration {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        fixture: bool,
    },
    RollbackMigration {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        fixture: bool,
    },
    RestoreOriginal {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        fixture: bool,
    },
    InspectCheckpoints {
        #[arg(long)]
        rollout: PathBuf,
    },
    NativeCompact {
        #[arg(long)]
        rollout: PathBuf,
        #[arg(long)]
        backend: PathBuf,
        #[arg(long)]
        runtime_root: Option<PathBuf>,
        #[arg(long)]
        codex_home: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliSort {
    Asc,
    Desc,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliItemsView {
    Summary,
    Full,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Index { rollout, db } => {
            let mut index = IndexedRollout::open(&db)?;
            print_json(&index.sync_rollout(&rollout)?)?;
        }
        Command::Turns {
            db,
            limit,
            cursor,
            sort,
            items,
        } => {
            let index = IndexedRollout::open(&db)?;
            print_json(&index.list_turns(limit, cursor.as_deref(), sort.into(), items.into())?)?;
        }
        Command::Items {
            db,
            turn_id,
            limit,
            cursor,
            sort,
        } => {
            let index = IndexedRollout::open(&db)?;
            print_json(&index.list_items(
                turn_id.as_deref(),
                limit,
                cursor.as_deref(),
                sort.into(),
            )?)?;
        }
        Command::ResumeWindow { db, output } => {
            let index = IndexedRollout::open(&db)?;
            let window = index.load_resume_window()?;
            if let Some(output) = output.as_ref() {
                if let Some(parent) = output.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let file = File::create(output)
                    .with_context(|| format!("failed to create {}", output.display()))?;
                let mut writer = BufWriter::new(file);
                for record in &window.records {
                    serde_json::to_writer(&mut writer, record)?;
                    use std::io::Write;
                    writer.write_all(b"\n")?;
                }
            }
            print_json(&serde_json::json!({
                "source_path": window.source_path,
                "start_offset": window.start_offset,
                "bytes_read": window.bytes_read,
                "records_read": window.records_read,
                "full_scan_required": window.full_scan_required,
                "output": output,
            }))?;
        }
        Command::GenerateFixture {
            output,
            turns,
            tail_after_checkpoint,
            payload_bytes,
            history_mode,
        } => {
            generate_fixture(
                &output,
                &FixtureOptions {
                    turns,
                    tail_after_checkpoint,
                    payload_bytes,
                    history_mode: history_mode.clone(),
                },
            )?;
            print_json(&serde_json::json!({
                "output": output,
                "turns": turns,
                "tail_after_checkpoint": tail_after_checkpoint,
                "payload_bytes": payload_bytes,
                "history_mode": history_mode,
            }))?;
        }
        Command::OracleProject {
            rollout,
            db,
            backend,
            runtime_root,
        } => {
            let runtime_root = runtime_root.map(Ok).unwrap_or_else(default_runtime_root)?;
            let projection = CodexOracle::new(backend, runtime_root).project(&rollout)?;
            let source_sha256 = sha256_file(&rollout)?;
            let mut index = IndexedRollout::open(&db)?;
            let report = index.replace_api_projection(
                &rollout,
                &projection.thread_id,
                &source_sha256,
                &projection.oracle_version,
                &projection.turns,
            )?;
            print_json(&report)?;
        }
        Command::BuildCandidate { db, output } => {
            let index = IndexedRollout::open(&db)?;
            print_json(&build_active_candidate(&index, &output)?)?;
        }
        Command::PrepareMigration {
            rollout,
            backend,
            runtime_root,
            fixture,
        } => {
            let runtime_root = runtime_root.map(Ok).unwrap_or_else(default_runtime_root)?;
            let (manifest_path, manifest) =
                prepare_migration(&rollout, backend, runtime_root, fixture)?;
            print_json(&serde_json::json!({
                "manifestPath": manifest_path,
                "manifest": manifest,
            }))?;
        }
        Command::ApplyMigration { manifest, fixture } => {
            print_json(&apply_migration(&manifest, fixture)?)?;
        }
        Command::RollbackMigration { manifest, fixture } => {
            print_json(&rollback_migration(&manifest, fixture)?)?;
        }
        Command::RestoreOriginal { manifest, fixture } => {
            print_json(&rehydrate_migration(&manifest, fixture)?)?;
        }
        Command::InspectCheckpoints { rollout } => {
            print_json(&scan_native_checkpoints(&rollout)?)?;
        }
        Command::NativeCompact {
            rollout,
            backend,
            runtime_root,
            codex_home,
        } => {
            let runtime_root = runtime_root.map(Ok).unwrap_or_else(default_runtime_root)?;
            let codex_home = codex_home.map(Ok).unwrap_or_else(default_codex_home)?;
            print_json(&create_native_checkpoint_offline(
                &rollout,
                backend,
                runtime_root,
                codex_home,
            )?)?;
        }
    }
    Ok(())
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

impl From<CliSort> for SortDirection {
    fn from(value: CliSort) -> Self {
        match value {
            CliSort::Asc => Self::Asc,
            CliSort::Desc => Self::Desc,
        }
    }
}

impl From<CliItemsView> for ItemsView {
    fn from(value: CliItemsView) -> Self {
        match value {
            CliItemsView::Summary => Self::Summary,
            CliItemsView::Full => Self::Full,
        }
    }
}
