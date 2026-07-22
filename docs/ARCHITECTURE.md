# Architecture

## 1. Problem statement

Codex keeps an append-only rollout for each task. Compaction replaces what the
model needs for continuation, but older rollout records remain on disk. Current
upstream code streams rollout lines but retains every parsed item in a
`Vec<RolloutItem>` for resume and replays the complete rollout for each
`thread/turns/list` request. Large files therefore still create host-side I/O,
allocation, parsing, serialization, renderer, and garbage-collection pressure
even when most old records no longer enter the model context.

The project must remove that eager-load pressure while preserving exact native
history in the same task. Opening a task must not require reading, parsing,
serializing, transferring, or rendering its complete transcript.

## 2. Product invariants

1. The user can reach the exact first message and every attachment from the
   original task UI.
2. Opening a task loads only a bounded newest page.
3. Upward scrolling requests bounded older pages through stable cursors.
4. The renderer keeps only a virtualized visible window plus a small cache.
5. Model continuation loads the latest valid native replacement-history
   checkpoint plus its suffix, independently of what the user browses.
6. Transcript size may increase disk usage and total search work, but must not
   make ordinary task opening proportional to total history size.

## 3. Vocabulary

The public UI calls the operation **Enable lazy history**. The implementation
uses `migration` for the atomic storage-layout transaction; it does not move the
task, change its id, or relocate its canonical active path.

- Native Archive: Codex sidebar lifecycle action that hides/moves a task.
- Cold segment: immutable old rollout records moved out of the active JSONL.
- Checkpoint: latest native `compacted` item with valid replacement history.
- Active suffix: all records after that checkpoint.
- Continuity card: verified project handoff extracted from one user task.
- Rehydration: byte-faithful reconstruction of the original full rollout order.

Native Archive and cold segmentation are independent. Performance governance
does not require hiding a task. Archive is also a de-layering boundary: a
managed task that becomes natively archived is losslessly rehydrated to a full
native rollout and its lazy index is disabled while Codex has no owner. Only a
later native unarchive can return it to active-layer eligibility. Rehydration
writes a durable receipt that binds the restored full-history hash to the
disabled index and displaced compact candidate. Reactivation verifies that
receipt, preserves the retired Archive generation, and creates a fresh manifest,
Vault source, rollback, and index instead of reusing historical paths.

## 4. Four memory layers

1. Disk history: the JSONL bytes persisted under the Codex home.
2. App-server RAM: Rust strings and parsed rollout objects in local process RAM.
3. Renderer RAM: JSON-RPC payloads and JavaScript/UI objects in Chromium RAM.
4. Model context: the compacted history and current suffix tokenized for a model.

Reading a 180 MiB rollout into app-server RAM does not mean the model receives
180 MiB. The local process may still allocate several representations before it
selects the much smaller model-visible checkpoint and suffix.

## 5. Three continuity guarantees

### Model continuity

Preserve the latest native replacement-history checkpoint and every later
record exactly. Never generate a substitute checkpoint externally.

This checkpoint requirement belongs to the current compatibility transaction,
not to model inference as a universal law. The current official backend can
reconstruct continuation from either the complete rollout or an official
replacement-history checkpoint plus its exact suffix. Removing a prefix without
either representation may still leave parseable JSONL, but it silently removes
model-visible state. A future native segmented backend may store pre-checkpoint
records in separate files without semantic loss only when the official context
builder can resolve the exact required items across those segments.

Semantic and host-performance control stay independent. Codex decides when its
model context requires Compact. CLM may reduce Resume I/O only around a useful
checkpoint that already exists naturally; bytes, elapsed time, or host pressure
must never force semantic compaction.

An explicitly approved optional policy may externalize Base64 `input_image`
payloads that the native Compact itself retained. It stores exact decoded image
bytes by SHA-256 and replaces only the active checkpoint copy with an explicit
local reference. The full rollout archive and rollback remain byte-exact. This
is not semantically identical to the default checkpoint: old images become
on-demand model inputs instead of automatic inputs on every Resume, so the
policy is opt-in and persists across later refresh generations once selected.

The current release deliberately implements no Compact decision engine. It
consumes useful checkpoints that Codex has already produced and otherwise
defers the storage rotation unchanged. The manual official-Compact command is
retained as an operator capability, but CLM lifecycle maintenance, file-size
thresholds, and Resume measurements never call it. A later semantic policy may
be designed separately; it is not part of lazy-history or exit maintenance.

### Project continuity

Before trimming, inspect the full task and the real project. Write verified
progress into an existing Roadmap, board, whiteboard, or broadcast when one is
available. An uncertain task remains untrimmed.

### Exact-history continuity

Store the removed prefix in immutable segments with hashes, line/byte ranges,
time ranges, attachment metadata, and enough ordering data for rehydration.

## 6. Primary runtime data path

1. New rollout items append to immutable or append-only bounded segments.
2. A transactional SQLite index maps thread, turn, and item identities to a
   segment id, byte offset, byte length, ordering key, and attachment metadata.
3. Model resume looks up the newest valid native checkpoint by reverse index and
   reads only that checkpoint and the exact later suffix.
4. Task open returns the newest bounded turn page and a backward cursor.
5. Upward scroll calls the existing paged thread API for one older page.
6. The frontend virtualizes rows and releases decoded off-screen attachments.
7. Search queries the index and materializes only matching records and context.

The first implemented slice uses the existing JSONL as canonical storage and a
SQLite sidecar projection. This proves incremental indexing and bounded reads
before raw history is segmented or any live Codex state is changed.

This is a storage and retrieval change, not a larger model-context window. A
100 GiB transcript can remain exactly browsable page by page while the model
still receives only its bounded continuation state.

## 7. Runtime integration boundary

The app-server is the primary intervention point because it owns rollout load,
reconstruction, and paged thread responses. If measurement shows that Desktop
already requests and retains only bounded pages, the backend change can carry
most of the solution. If Desktop eagerly follows every cursor or retains all
decoded items, its request loop and list renderer must also change.

The current signed Microsoft Store build checks `CODEX_CLI_PATH` before its
bundled app-server path. That launch seam is now proven end to end without
changing the signed package. CLM points it at a transparent proxy, and the proxy
starts a hash-pinned official backend copied with its companion executables.

Desktop has used two lazy-history request shapes across Store releases. Older
builds put a bounded `initialTurnsPage` inside `thread/resume`. The local-host
branch in Store package `26.715.8383.0` instead sends `excludeTurns: true`, waits
for the Resume skeleton, and then requests the first five full turns through a
separate `thread/turns/list`. The proxy accepts both shapes. When the initial
page is inline, it observes that page's older cursor and stops Desktop's first
automatic follow-up request once; a later request for the same cursor remains
valid. For an unmanaged thread a later page is forwarded to the exact official
backend. For a managed thread the proxy additionally:

1. lets the official backend resume only the compact active rollout;
2. imports the bounded mutable tail returned by that backend;
3. clears `thread.turns` when the client requested `excludeTurns`;
4. injects the requested indexed `initialTurnsPage` when the client used the
   inline shape;
5. otherwise lets Desktop request the first and later pages through
   `thread/turns/list`, served directly from SQLite.

The proxy does not patch `app.asar`, the Store package, Git, or shared Codex
state. Store and protocol versions remain a compatibility gate on every update.

### Workspace restart continuity

Conversation storage continuity and desktop workspace continuity are separate
planes. The proxy keeps one task fast and lossless; it cannot make Electron
remember ten BrowserWindows across a process restart.

The workspace manager therefore records a metadata-only map from exact active
thread ids to top-level HWND geometry and Windows virtual desktop GUIDs. It
resolves task identity from the accessible Codex header against read-only
`state_5.sqlite` plus `session_index.jsonl`, fixes its own process to Per-Monitor
DPI Aware V2, and refuses ambiguous mappings. On restore it uses
`codex://threads/<id>`, the signed client's own **Open in new window** command,
public `IVirtualDesktopManager`, and `SetWindowPos`. It never patches the Store
package or edits sidebar/task state.

This plane has its own acceptance boundary: every Codex owner must be absent,
the monitor and virtual-desktop topology must match, and the final live task,
desktop, rectangle, and count map must equal the snapshot. Partial windows
created by a failed attempt are closed only by their exact newly captured HWND.
See [Codex Workspace Restore](WORKSPACE_RESTORE.md).

### Bounded Optimistic Resume

Managed lazy paging removes transcript-size work, but the signed Desktop still
waits roughly 2.5 seconds for the official backend to establish a resumed
thread. CLM may hide that fixed wait only for an explicitly enabled managed
canary with a verified official response cache:

1. The first matching Resume always takes the ordinary backend path. After
   success, CLM stores the official result skeleton without turns or an initial
   history page.
2. The cache is bound to the thread id, canonical backend identity and metadata,
   Codex Home/config hash, backend arguments, and stable Resume parameters. Any
   mismatch is a cold fallback, never a synthesized response.
3. A warm Resume receives that official skeleton immediately. For the inline
   protocol shape CLM also attaches the newest bounded SQLite page. For the
   current local-host shape Desktop immediately follows with
   `thread/turns/list`, which CLM serves from SQLite without waiting for the real
   Resume. The same real Resume is still forwarded to the official backend; its
   duplicate client response is swallowed after CLM refreshes the mutable
   active tail and cache.
4. `turn/start` for that thread is held until the real Resume owns the backend
   session. The gate releases at most once in FIFO order and is bounded to four
   turns, 1 MiB, and 15 seconds. Failure or timeout returns explicit errors.

This is perceived-latency speculation, not fake backend readiness. It is off by
default and configured from `<runtime-root>\Data\optimistic-resume.json` as
`disabled`, an exact `canary` thread-id set, or `all_managed`. Production starts
with a small canary. A proxy crash after backend acceptance remains outside a
durable exactly-once guarantee; CLM does not claim transactional Send across
process failure.

Resume timing schema v3 records the original `excludeTurns` value, whether an
inline initial page was present, whether policy enabled the thread, and a
content-free cache outcome such as `hit`, `cache_file_missing`,
`cache_environment_mismatch`, or `cache_request_mismatch`. Cache read, parse,
metadata, and size failures always fall back to the official Resume path.

Replacing packaged binaries or patching `app.asar` in place is prohibited.

## 8. Offline compatibility lifecycle

This path is retained for migration, rollback, old Codex releases, and emergency
pressure relief. It is not the desired final user experience.

1. Confirm that Codex Desktop and all owning app-server processes are closed.
2. Snapshot thread registry, file identity, size, timestamps, and hashes.
3. Group real user tasks by canonical project root.
4. Require an existing useful native Compact checkpoint; leave tasks without
   one unchanged unless the user separately invokes official Compact.
5. Run continuity extraction against the still-complete original history.
6. Reconcile each project group against live files, Git, tests, and handoffs.
7. Apply or preserve the approved project-level continuation writeback.
8. Stream the dead prefix into immutable cold segments.
9. Build a candidate containing canonical metadata, latest checkpoint, and suffix.
10. Prove parse validity, byte identity, ordering, and reconstructed equivalence.
11. Replace the active path transactionally and retain a same-volume rollback.
12. Reopen one canary task and verify UI identity, model continuity, and appends.
13. Only then process another bounded batch.

### Managed generation refresh

Lazy paging bounds the archived prefix, but it does not make the active JSONL
permanently small. New turns, tool results, reasoning records, and later native
compaction records continue appending to that active file. A long-running
managed task can therefore become expensive to resume again even though its old
pages are still served lazily from the sidecar index.

`refresh-migration` is the offline generation-rotation transaction for that
case. It losslessly rehydrates the archived prefix plus every post-activation
record, prepares a new index and candidate around the newest native checkpoint,
and activates the candidate only when it is smaller than the active file it
replaces. The previous manifest, rollback, compact active file, and index remain
in timestamped evidence paths. If preparation, projection, activation, or
verification fails, the previous managed generation is restored by exact file
ownership rather than deleted.

Refresh is not a watcher and never mutates a live rollout. It requires a
controlled Codex shutdown and a newer useful native checkpoint. See
`docs/MANAGED_TAIL_REFRESH.md` for the command, transaction states, and
acceptance evidence.

### Automation boundary

The public alpha exposes refresh as an explicit offline transaction. It does
not install a polling watcher, close Codex, relaunch the app, or schedule
maintenance for the user.

The machine-local event lifecycle avoids polling by waiting on one exact Codex
root process handle, then verifying that no exact Codex owner remains before it
activates eligible originals and inspects managed manifests. It retains a
session marker as attempt evidence and retains the previous generation so an
interrupted exit cannot leave the active task between generations. The marker
is never a startup gate. A user launch requests cancellation, waits only for the
current atomic task, and then owns the reopen; an unexpected failure retires the
marker into evidence and also releases startup. Public packaging remains a
separate gate.

Automation must rotate only around a newer useful checkpoint that Codex
produced naturally. If none exists, it must defer without changing the rollout.
Host performance must never trigger semantic native Compact, and process
control must never broaden into terminating unrelated Codex windows.

## 9. Thread classification

- Fresh checkpoint: rotate directly after continuity audit.
- Stale checkpoint: defer until Codex naturally writes a newer useful Compact.
- No checkpoint: leave the rollout byte-for-byte unchanged.
- Legacy checkpoint without replacement history: do not rotate in v0.
- Corrupt, partial, or ambiguous history: do not rotate.
- Active/in-flight owner: do not inspect as a write candidate.
- Finished native-archived task: ledger-only during active maintenance; only a
  separate explicit compatibility transaction may rehydrate it, and only a
  later native unarchive plus a verified receipt may start a new generation.

## 10. Continuity auditor

The auditor operates per project, not as independent writers per thread.

Each task card records:

- state: active, completed, superseded, blocked, or uncertain;
- user intent and non-negotiable constraints;
- verified work, paths, commits, and tests;
- unresolved questions and next executable action;
- contradictions between transcript claims and current project state;
- cold-segment references for exact retrieval.

The project reconciler merges all related cards and performs one project-level
writeback. Conversation claims never outrank current files or verified tests.

## 11. Storage layout

Source remains in this repository. Installed runtime and private conversation
data belong under the configured CLM runtime root (by default
`%LOCALAPPDATA%\ConversationLifecycleManager`):

```text
Data/Vault/Codex/<thread-id>/
  manifest.json
  continuity.json
  segments/
    segment-000001.jsonl.zst
  attachments/
    compact-images/
      tx-<prepared-unix-ms>-<candidate-sha-prefix>/
        <content-sha256>.<image-extension>
  compact-images.json
  index.sqlite
```

Segments are immutable. The manifest stores uncompressed and stored hashes.
Compression is an implementation choice and must never block exact recovery.
The Compact-image manifest also records every source occurrence by JSONL record
ordinal and RFC 6901 JSON Pointer. Thread, transaction, and content-hash layers
must never be flattened or mixed across conversations.
An upstream implementation may keep equivalent indexed segments under the
Codex-owned data root; the logical contract matters more than this fallback
vault's physical path.

## 12. Fallback retrieval and rehydration

The compatibility layer provides on-demand commands for search, message windows,
image extraction, and full rehydration. A small future Codex skill tells agents
when and how to invoke those commands. It does not preload archive contents and
does not run a watcher, MCP server, or background indexer.

Retrieval triggers include references such as "earlier", "the previous image",
"the decision from last week", or a checkpoint that points to archived detail.

The planned retrieval contract is hierarchical rather than a top-k raw-chunk
dump:

1. An always-small history map advertises that exact indexed evidence exists.
2. A thread timeline summary identifies relevant phases, decisions, constraints,
   artifacts, and unresolved work.
3. Bounded episode summaries narrow the search to exact turn ranges.
4. Raw turns, tool records, values, quotations, and attachments are materialized
   only when exact evidence is required.

Summaries are navigation artifacts, not native Compact records and not evidence
authorities. Every summary node keeps source ranges, timestamps, hashes, and
children so retrieval can descend to immutable raw records. Full-text search
locates exact paths, numbers, commands, and wording. Embedding similarity may
locate paraphrased concepts, but it only proposes candidates; it never replaces
raw evidence or enters model context by itself.

For Codex compatibility, bounded high-confidence recall can be injected at a
turn boundary and deeper retrieval remains explicit. For UAF integration, CLM
owns lossless storage, indexing, provenance, and bounded retrieval; UAF owns the
decision to recall, the evidence budget, and what enters each agent's active
context. This keeps current-goal context small while making older project memory
available on demand.

## 13. Native UI feasibility

Codex app-server already exposes paged turn APIs, cursor fields, summary/full
item views, `excludeTurns`, and initial-page controls. A fully indexed backend
can keep native upward scrolling while loading only bounded pages.

Storage and protocol integration feasibility are established for package
`26.707.8479.0` and `codex-cli 0.144.2`. Without the compatibility gate,
installed Desktop calls
`loadRemainingConversationTurns`, follows every older cursor in five-turn pages
with no backoff, merges each page into conversation state, and broadcasts a new
snapshot. When another client connects, `handleClientStatusChanged` also
broadcasts the complete state of every streaming conversation. Indexed backend
pages remove repeated full-rollout scans, but they do not by themselves prevent
frontend eager loading or full-state fan-out.

The installed proxy now adds a one-shot drain gate for every resumed thread. It
records the initial page's older cursor from either the official resume response
or the managed indexed page and rejects the first matching page request once.
Live Store logs prove Desktop exits its automatic load loop while preserving the
cursor. On the managed canary, real upward scrolling then fetched 27 successful
indexed pages in 5-11 ms each, reached the exact oldest content, and returned to
the bottom. Unmanaged tasks also stop their automatic page chains, and a later
manual retry remains forwarded unchanged to the official backend.

This establishes true Store lazy paging for one managed task, not bounded open
cost for the legacy fleet. Before a task is migrated, its initial
`thread/resume` still makes the official backend reconstruct the entire rollout.
A 1.35 GB unmanaged task took 7.365 seconds and serialized a later small-task resume
behind it. Full-state fan-out on a newly connected client also remains an open
gate. The compatibility route stays version-gated because it relies on current
Desktop stop-and-retry behavior.

Post-filter user acceptance now distinguishes task resume from window birth.
Switching existing tasks no longer stalls the pointer, although an unmanaged
task can still show a black interval while its full legacy rollout is rebuilt.
Creating a blank window before any resume still hitches the pointer. In the
current runtime that blank-window sample spent 6.375 CPU seconds in Electron's
shared GPU process and 1.344 CPU seconds in DWM while the official app-server
spent only 0.016 CPU seconds. CLM cannot classify that remaining renderer/GPU
bootstrap as history reconstruction, and a backend history fix alone cannot
remove it.

### Navigation rail and loading-feedback boundary

The current Store frontend does not treat its left-side user-message navigation
rail as an index over unloaded history. It enables the rail only when
`isConversationHistoryComplete` is true, its item collector returns an empty
array while history is incomplete, and the rail itself stays hidden below four
items. Its reveal callback can materialize an already-known virtualized turn,
but it has no contract for fetching a turn that exists only behind an older
cursor.

This explains the current tradeoff precisely: CLM's drain gate preserves a real
older cursor and therefore preserves lazy loading, but that same incomplete
history state suppresses the native rail. A proxy-only response rewrite cannot
remove the tradeoff safely. Marking the history complete would stop native
upward paging; preloading full turns would recreate the original pressure; and
preloading incomplete summaries would leave no supported path to hydrate the
missing bodies.

The target UI therefore needs two independent projections:

1. **Navigation catalog:** stable user-message item id, turn id/key, ordinal,
   short bounded label, and enough attachment/type metadata to render a marker.
   It contains no assistant body, tool payload, image bytes, or model context.
2. **Body page cache:** recent and explicitly requested full turn pages, loaded
   through indexed cursors and released outside a bounded renderer window.

Selecting a catalog marker must resolve its turn anchor directly to one indexed
target page, merge that page into the sparse/virtualized view, and scroll only
after the target element materializes. Sequentially loading every page between
the current position and the target is not an acceptable jump implementation.

Both upward-scroll paging and target-marker reveal require visible progress:
show a spinner or loading state at the upper boundary and on the selected
marker, preserve the existing content while the request is pending, and expose
retry/failure instead of making a delayed request look like the beginning of
history.

This contract requires official frontend participation or a supported
integration seam. The live app-server proxy can serve the catalog and target
pages, but it cannot make the signed Store frontend consume a new projection.
Runtime injection, in-place `app.asar` modification, and an external overlay
remain outside the approved route.

Decision as of 2026-07-14: this is the safe proxy-only optimization ceiling,
not another local implementation backlog. Status is `blocked_upstream`. Do not
retry cursor rewrites or synthetic placeholders when the missing capability is
frontend ownership. `docs/UPSTREAM_FRONTEND_BLOCKER.md` records the rejected
routes, official unlock conditions, and exact reactivation checks.

Managed explicit full reads use the complete ordered SQLite API-turn projection,
not the compact active rollout. For `thread/read(includeTurns=true)`, the proxy
asks the official backend for metadata only, verifies the returned thread id,
then injects every projected turn after row-id and total-count checks. This
restores API and task-tool compatibility without changing bounded Resume or
manual older-page loading. It can intentionally materialize the whole history
for that one explicit caller, so it is not the ordinary Desktop navigation path.

Managed forks remain blocked. The compact active rollout and API-turn projection
are sufficient for continuation and read APIs but are not a byte-exact native
fork source. A future rehydration transaction must materialize an exact temporary
full rollout before fork is enabled.

The Store also broadcast-falls back child-agent `turn/started`,
`turn/completed`, `item/started`, and `item/completed` notifications when no
Desktop renderer owns the child thread. With several windows open, every
renderer rejects the same event as an `unknown conversation`, multiplying main
process work and log writes. The installed proxy records thread ids referenced
by real Desktop requests and drops only those four stream notifications for ids
that no client has ever referenced. It preserves owned task events, global
notifications, responses, history paging, and backend-side agent execution.
The filter became live on 2026-07-14 at SHA-256
`BB40D14A0C2805E786DB769D5A2B0CC8C1F1C49168E91084B5BA29FFF6C3804D`.
A minimal native child completed and closed while its parent retained the
expected completion notification. User-visible pointer acceptance and a raw
post-flush Store-log zero-error check remain open gates.

That filter does not provide per-renderer isolation for root tasks. Desktop
funnels every renderer through one app-server stream. A request can prove that
the shared Desktop client owns a thread, but neither the request at the proxy
seam nor a later backend notification carries the originating
`rendererWebContentsId`. Once any window has referenced a root thread, dropping
its events would also drop them for the real owner. The signed Electron main
process still chooses whether to route or broadcast that one output stream.

The 2026-07-19 Store log confirms this remaining boundary: 1,374 root-task
notifications reached renderers that reported `unknown conversation`. CLM now
reports that count but does not pretend to fix it. Safe root-task isolation
requires either a renderer/client subscription identity in the app-server
protocol or correct owner routing inside the signed Desktop main process.

Repeated skill discovery is separable because it is an ordinary request and
response on the proxy seam. The development proxy caches successful identical
`skills/list` responses for at most 30 seconds and eight parameter sets,
rewrites only the caller request id, bypasses on `forceReload=true`, invalidates
on skill/plugin mutations, rejects errors and stale generations, and bounds
pending bookkeeping. See `docs/SKILLS_LIST_CACHE.md`.

## 14. Target native implementation

The target Codex store appends bounded chunks and maintains an index from thread,
turn, and item ids to segment offsets. Model resume loads only the newest
checkpoint and suffix. UI history pages older turns from indexed segments into
a virtual list. Exact native browsing is the target, not a future bonus.

The compatibility vault still retains full rehydration so no migration decision
can strand history or bind recovery to one client implementation.

## 15. Deferred live-maintenance ladder

The current compatibility release finishes the offline foundation before it
attempts a transparent running-process refresh. Its accepted baseline is:

1. consume an existing useful native Compact;
2. preserve exact archived history and its SQLite projection;
3. serve bounded Resume and lazy UI pages from the managed generation;
4. refresh the mutable active tail only after Codex owners exit;
5. retain a complete previous generation and verified rollback.

Live maintenance remains a staged future route:

1. **Shadow sync:** on an existing `turn/completed` event, incrementally index
   only complete appended records and prepare read-only refresh evidence. This
   does not replace the active rollout or claim to free live process memory.
2. **Visible backend recycle canary:** after proving no in-flight turn, stop the
   exact official app-server child, rotate around an existing natural Compact,
   restart it, and verify protocol initialization, append continuity, paging,
   notifications, and rollback while the Desktop window remains visible.
3. **Transparent backend recycle:** queue and replay bounded client requests so
   the proven recycle no longer requires user coordination.
4. **Native bounded runtime:** move segmentation and in-memory eviction into the
   owning backend and virtualized frontend. This is the only route that can keep
   disk, app-server RAM, and renderer RAM bounded without periodic recycling.

No later stage may be described as seamless until the preceding stage passes a
real Store canary. Until that ownership contract is proven, the hard rule remains
zero active-rollout writes while any Codex owner is alive.

## 16. Hard acceptance gates

- Zero writes while an owner process is alive.
- Zero maintenance result, marker, or failed recovery attempt may block a user
  launch after the current atomic task releases its mutex.
- Zero synthetic checkpoints.
- Zero trim without a continuity card.
- Zero trim after failed project reconciliation.
- Zero active-path change or thread-id change.
- Byte-exact checkpoint and suffix preservation.
- Exact cold-segment hash verification.
- Successful same-thread reopen and append canary.
- Tested rollback before fleet processing.
- Exact first-to-last history reachable through bounded pages.
- Task-open I/O, RPC payload, and renderer retention remain bounded as the
  transcript grows.
- No full-rollout read on ordinary model resume or initial UI open.
- No automatic cursor drain after the bounded initial page.
- No older-page request before a real upward-scroll boundary.
- No complete multi-conversation snapshot fan-out when a new task/window client
  connects.
