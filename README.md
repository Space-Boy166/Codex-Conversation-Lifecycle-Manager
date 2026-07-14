# Codex Conversation Lifecycle Manager

An unofficial, local lazy-history compatibility layer for long Codex Desktop
conversations on Windows.

CLM keeps the complete original transcript, opens a managed conversation from
a bounded recent tail, and serves older turns through Codex's native paginated
history API as the user scrolls upward. It does not patch the signed Store app,
upload conversation data, or replace the official Codex backend.

> **Alpha software:** CLM changes the active storage representation of a
> selected conversation. It uses verified archives and reversible transactions,
> but users should read the limitations below before enabling it on important
> work.

## Quick start

1. Download the latest Windows release ZIP from
   [GitHub Releases](https://github.com/OVN-Venxus/codex-conversation-lifecycle-manager/releases/latest).
2. Extract the ZIP to a normal local folder.
3. Double-click `CLMSetup.exe`.
4. Choose **Enable lazy history**, select one conversation, and confirm.

The setup program handles Store-package detection, task discovery, disk-space
checks, official-backend copying, archive creation, indexing, proxy activation,
Codex restart, and startup verification. Users do not need to find JSONL files,
edit environment variables, or run Cargo commands.

To undo the change, open `CLMSetup.exe` again and choose **Restore**. Restore
rebuilds the complete original file and merges records added after activation.

## What activation changes

Codex legacy conversations are append-only JSONL files. On affected builds, the
official app-server reconstructs the complete file before an external proxy can
page its history. A read-only proxy can stop an automatic older-page drain, but
it cannot remove that initial full-file resume cost.

For one explicitly selected conversation, CLM therefore performs a reversible
activation transaction:

1. Copy and SHA-256 verify the complete original into a local vault.
2. Reconstruct official API Turns with the user's installed Codex backend.
3. Build a transactional SQLite page index.
4. Keep the exact session metadata, latest native replacement-history
   checkpoint, and byte-exact suffix in the active JSONL path.
5. Retain a same-volume rollback copy.
6. Route Codex Desktop through a local proxy that injects a bounded initial
   page and serves older indexed pages on demand.

The thread id, task title, Project membership, canonical rollout path, and exact
old messages remain intact. CLM calls this **Enable lazy history** in the user
interface; the source uses "migration" for the underlying atomic data-layout
transaction.

## Measured proof

The current Windows canary established:

- a `236,869,210` byte real rollout became a `6,490,905` byte initial active
  tail while the full original remained hash-verified;
- the same native task reopened, accepted new turns, reached its exact oldest
  message, and materialized an old image;
- 27 manual older-page requests completed in 5-11 ms with zero page errors;
- a fresh renderer reopened the task from the bounded newest page rather than
  inheriting all pages visited in another window;
- a synthetic legacy transaction using the backend shipped in Store package
  `26.707.9981.0` reduced `335,512` bytes to `11,388` active bytes and restored
  the exact original SHA-256.

The backend binaries in Store packages `26.707.9564.0` and `26.707.9981.0` were
byte-identical during this verification. Store updates remain version-sensitive
and should be checked with `CLMSetup doctor` after an update.

## Important limitations

- The initial release targets the Microsoft Store Codex Desktop app on Windows
  x64.
- A conversation must already contain a native Codex replacement-history
  checkpoint. CLM refuses to synthesize one.
- Activation is per conversation. CLM never silently converts every task or
  installs a polling watcher.
- Codex must fully close while activation or restore changes the active file.
- The vault needs roughly the original history size plus index and safety
  reserve. `CLMSetup` checks free space before preparation.
- The current signed frontend hides its user-message navigation rail while
  history is incomplete and shows no loading indicator at the upper boundary.
  Manual upward scrolling still loads exact older pages.
- Managed full-history reads and thread forks are blocked until CLM can safely
  materialize a temporary complete source for those operations.
- Unlimited upward traversal in one renderer is not proven to have bounded
  JavaScript retention because renderer eviction belongs to the Store frontend.
- A Store update can change the private app-server protocol or
  `CODEX_CLI_PATH` launch seam. Keep the rollback files until the updated build
  has reopened the managed task successfully.
- CLM does not fix blank-window Electron/GPU bootstrap, Git Review process
  storms, duplicate MCP servers, or general system scheduling pressure.

See [Architecture](docs/ARCHITECTURE.md),
[research evidence](docs/RESEARCH_EVIDENCE.md), and the
[upstream frontend boundary](docs/UPSTREAM_FRONTEND_BLOCKER.md) for details.

## Independent Codex mitigations

[Codex Desktop performance troubleshooting](docs/CODEX_DESKTOP_TROUBLESHOOTING.md)
documents optional No-Review workspace containment, `last-turn-only` Review
mode, Windows `BelowNormal` priority, MCP duplication, and new-window hitching.
Those are separate mitigations, not CLM inventions. `CLMSetup.exe` does not
apply them.

## Command line

The setup executable also supports non-interactive inspection:

```powershell
CLMSetup.exe scan --minimum-mib 64
CLMSetup.exe doctor --json
CLMSetup.exe enable --thread-id <thread-id> --yes
CLMSetup.exe restore --thread-id <thread-id> --yes
```

Advanced development commands remain available through
`conversation-lifecycle-manager.exe`:

```powershell
conversation-lifecycle-manager.exe generate-fixture --output fixture.jsonl --turns 10000
conversation-lifecycle-manager.exe inspect-checkpoints --rollout C:\path\to\rollout.jsonl
conversation-lifecycle-manager.exe prepare-migration --rollout C:\path\to\rollout.jsonl `
  --backend C:\path\to\copied\codex.exe --runtime-root C:\path\to\clm-runtime
conversation-lifecycle-manager.exe restore-original --manifest C:\path\to\manifest.json
```

Mutation commands refuse to run while `ChatGPT.exe`, `codex.exe`, or the CLM
proxy is still active. `--fixture` is for copied test data only.

## Build and test

```powershell
cargo fmt --all --check
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
.\tests\Test-PublicReleaseContract.ps1
.\tools\Build-PublicRelease.ps1
```

Runtime conversation data, indexes, archives, copied official backends, and
release artifacts are ignored by Git. Release ZIPs contain CLM binaries only;
`CLMSetup.exe` copies the official backend from the user's installed Store
package after installation.

## Project position

CLM does not claim to have invented lazy loading. Codex upstream is already
developing paginated thread history and SQLite TurnItems. This project is an
external compatibility bridge for current legacy Desktop histories, with
lossless activation, exact archive/rollback guarantees, and a verified native
UI paging path.

The reusable lifecycle core can later support other agent runtimes, including
Ultimate Agent Frame adapters, without bundling their product-specific policy
into the Codex installer.

## License

MIT. This project is not affiliated with or endorsed by OpenAI.
