# Codex Conversation Lifecycle Manager

**让 Codex Desktop 中的超长对话能继续用，而不是切一下就瞎几把乱卡**

CLM is a reversible Windows compatibility layer for affected Codex Desktop
builds. It preserves task identity, exact recoverability, and on-demand access
to older turns while replacing eager legacy resume with a bounded recent page
backed by an indexed history source.

历史对话应当是资产，而不是妨碍老子开发的累赘

On the validated managed canary, sidebar task switching stopped producing the
pointer stall, and a fresh renderer no longer inherited the complete page set
loaded in another window. That removes duplicated CPU, memory, RPC, and
renderer work specifically from the managed long-history path.

> ClosedAI fuck you

> 只知道营销，不修用户体验，纯傻逼

## 日常变化

- **切换不再拖死鼠标。**
- **多开不再重复吞历史。**
- **旧消息和图片仍可找回。**

These outcomes are deliberately scoped to one explicitly managed long task.
CLM removes its eager Resume cost and prevents fresh renderers from inheriting
already visited history pages; it does not repair the separate Electron
GPU/compositor hitch that can occur before a brand-new blank window resumes any
task.

## Why this exists

A long-running Codex task becomes valuable because it contains decisions,
failed attempts, images, project context, and the exact path that led to the
current work. But on affected Windows Desktop builds, that same history can
become expensive to reopen.

Legacy tasks are stored as append-only JSONL. When one grows to hundreds of
megabytes or more, Codex may reconstruct the full file and eagerly request old
history before you ask to see it. The result can be:

- a blank or slow task while the backend rebuilds old turns;
- one app-server core staying busy and contributing to mouse or desktop stalls;
- the same cost returning when a task or window is reopened;
- extra memory pressure, instability, and crashes around the tasks you most
  want to preserve.

Starting a new task avoids the large file, but it splits a useful working
history into disposable fragments. Archiving the old task also does not help
when you need to reopen it.

## 上游公开记录

并非个例。

CLM targets a documented upstream failure family rather than a hypothetical
benchmark: independent reports across Codex surfaces and Windows builds describe
unbounded thread metadata, eager long-history hydration, repeated full-state
broadcast, and destructive resume-time failures.

截至 2026-07-14，依旧没关闭

| Upstream report | What it establishes |
| --- | --- |
| [#21211](https://github.com/openai/codex/issues/21211) | The core scope: thread navigation and loading can scale with unbounded metadata and eagerly hydrated history. |
| [#20781](https://github.com/openai/codex/issues/20781) | An adjacent transport path: completed long threads repeatedly broadcast large full-state snapshots, multiplying extension-host and renderer work. |
| [#32722](https://github.com/openai/codex/issues/32722) | A current Windows reproduction from this project's author: opening a new window still fans out full conversation snapshots for every streaming thread. |
| [#31583](https://github.com/openai/codex/issues/31583) | An independent Windows impact report: resuming long-running threads can silently destroy and relaunch the AppX container. |

线程少、后端快，照样会卡。

An [independent Windows `26.707.9981.0` datapoint](https://github.com/openai/codex/issues/21211#issuecomment-4970497508)
inside #21211 reproduced a stuck conversation sidebar with only 215 local
threads while `thread/list` stayed at roughly 1 ms median and 14 ms p95. The
repeatable Settings round-trip recovery narrows that adjacent failure toward
renderer virtualization or observer lifecycle rather than slow SQLite or list
RPC.

These public reports do not establish one shared root cause, and CLM does not
claim to solve every crash or renderer fault. CLM specifically mitigates the
legacy long-history resume and replay path; Git Review storms, eager MCP
ownership, local-state corruption, file-link crashes, and signed-frontend
virtualization remain upstream or separate concerns.


## What CLM changes

CLM keeps the complete original history safe on disk, but lets Codex open the
recent part first. Older turns are fetched through Codex's existing paginated
history API only when you scroll upward.

```text
Without CLM: open task -> rebuild a large history -> wait for the task
With CLM:    open recent turns -> continue working -> load older pages on demand
```

That gives you:

- a much smaller history surface for the initial resume;
- the same task title, Project membership, thread id, messages, and old images;
- exact older pages when you choose to scroll back;
- per-task opt-in instead of a silent conversion of every conversation;
- a verified, lossless Restore path if CLM or a future Codex build is not a
  good fit.

Everything stays local. CLM does not upload conversations, patch the signed
Store app, or redistribute the official Codex backend.

> **Alpha software:** CLM changes the active storage representation of a
> selected conversation. It uses verified archives and reversible transactions,
> but the first test should still be one non-critical task. CLM addresses the
> long-history resume path; it does not claim to fix every source of Codex lag.

## 快速开始

1. 从 [GitHub Releases](https://github.com/Space-Boy166/Codex-Conversation-Lifecycle-Manager/releases/latest)
   下载 Windows ZIP。
2. 解压到普通本地文件夹。
3. 双击 `CLMSetup.exe`。
4. 选择 **Enable lazy history**，挑一个对话并确认。

The setup program handles Store-package detection, task discovery, disk-space
checks, official-backend copying, archive creation, indexing, proxy activation,
Codex restart, and startup verification. Users do not need to find JSONL files,
edit environment variables, or run Cargo commands.

想撤销时，再打开 `CLMSetup.exe`，选择 **Restore**。

Restore reconstructs the complete original layout and merges every record
appended after activation, so rollback does not discard work created while CLM
was enabled.

## How it works

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

## What we measured

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
