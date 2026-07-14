# Research Evidence

## Current machine observations

- Running Codex Store package: `OpenAI.Codex` version `26.707.9564.0`.
- Store package `26.707.9981.0` was downloaded and staged on the same machine.
  Its bundled backend was byte-identical to `26.707.9564.0` at SHA-256
  `2CAACAD1F7B8B3E9B2527B9BFF9630CFBB30EC25D8D8C018C9D55A2BEC348032`.
- Read-only inspection of this package's `app.asar` shows that the user-message
  navigation rail is enabled only when conversation history is complete, its
  item collector returns no markers for incomplete history, and its reveal
  callback addresses only already-known virtualized turns.
- The same frontend tracks older-page loading but renders no progress indicator
  at the upper history boundary. The visible task-loading status is a separate
  resume/composer path.
- These observations establish an official-frontend ownership boundary: a
  proxy can supply pages but cannot restore the native rail, target an unloaded
  marker, or expose the tracked loading state. The local status is
  `blocked_upstream`, with reactivation criteria in
  `docs/UPSTREAM_FRONTEND_BLOCKER.md`.
- The Store main process reads `CODEX_CLI_PATH` before choosing its bundled
  app-server. A transparent executable at that path was proven with real
  initialize, resume, notification, and turn-page traffic.
- Store Desktop already requests `excludeTurns: true` plus an initial page of
  five full turns. Without CLM's gate it then eagerly drains every older cursor;
  with the installed one-shot gate it stops after the initial page and retries
  the preserved cursor only when the user scrolls upward.
- The managed live canary completed 27 manual older-page requests in 5-11 ms
  each, reached the exact top, and returned downward with zero page errors.
- Unmanaged tasks no longer produce automatic page chains, but their initial
  official resume still reconstructs the complete rollout. A 1.35 GB task took
  7.365 seconds and serialized a later small-task resume behind it.
- The packaged and materialized backend is `codex-cli 0.144.2`; its SHA-256 is
  `2CAACAD1F7B8B3E9B2527B9BFF9630CFBB30EC25D8D8C018C9D55A2BEC348032`.
- The real canary source was over 217 MB during later measurement.
- A structured read-only scan found 35 native replacement-history checkpoints;
  the latest starts at byte 211,008,657.
- The thread contained multiple compactions, so most bytes preceded the latest
  model-history base.
- An offline release transaction against the staged `26.707.9981.0` backend
  reduced a 335,512-byte synthetic legacy source to an 11,388-byte active tail
  and restored the exact original SHA-256.
- Claude Code 2.1.158 also retained pre-compaction JSONL records locally; that
  product solves model context pressure but does not physically rotate history.

These observations are evidence for investigation, not fleet-wide assumptions.
A metadata-only inventory found 428 rollout files totaling 52.609 GiB, including
12 files at least 1 GiB; every migration candidate still requires independent
checkpoint, continuity, identity, and rollback proof.

## Public issue map

公开记录如下。

This map separates reports that directly support CLM's long-history mechanism
from adjacent Windows reliability failures. A link proves that the failure was
publicly reported and remains inspectable; it does not by itself prove that
OpenAI has accepted one root cause or that CLM fixes the entire incident.

### Direct long-history failure family

- [#21211 — Thread navigation/loading slows from unbounded metadata and eager
  large-history hydration](https://github.com/openai/codex/issues/21211) is the
  umbrella report closest to CLM's scope. It covers oversized list metadata,
  eager full-history reads, and the need for bounded initial hydration plus
  paginated turns.
- [#20781 — Repeated huge `thread-stream-state-changed`
  snapshots](https://github.com/openai/codex/issues/20781) shows the same
  retained-history pressure escaping through a different surface: completed
  long threads can repeatedly broadcast full state and multiply work in the VS
  Code extension host and renderer.
- [#32722 — New Windows still fan out full conversation
  snapshots](https://github.com/openai/codex/issues/32722) records the current
  Windows reproduction submitted by CLM's author. It is first-party project
  evidence, not an independent user report, and demonstrates that a newly
  connected window still receives full snapshots for every streaming thread.
- [#31583 — Windows AppX container silently relaunches after long-thread
  resume](https://github.com/openai/codex/issues/31583) is independent impact
  evidence. It links long-running-thread resume to a destructive application
  lifecycle symptom without claiming that eager hydration is the only possible
  cause.
- [The `tail_history` A/B on Windows build
  `26.707.8479.0`](https://github.com/openai/codex/issues/21211#issuecomment-4955990099)
  isolated one idle 1.258 GB task repeatedly requesting old pages and consuming
  almost one app-server core; closing only that restored window stopped the
  loop and reduced app-server CPU to roughly 1%.
- [An independent renderer datapoint on Windows build
  `26.707.9981.0`](https://github.com/openai/codex/issues/21211#issuecomment-4970497508)
  observed a sidebar freeze with 215 threads even though `thread/list` remained
  fast and error-free. The repeatable Settings remount recovery and thousands
  of `ResizeObserver` warnings point toward an adjacent renderer lifecycle
  boundary, not the indexed history path CLM owns.

证据来自不同用户，也来自不同版本。

### Adjacent Windows failures outside CLM coverage

- [#32154 — One eager MCP stack per opened chat plus history
  replay](https://github.com/openai/codex/issues/32154) concerns MCP ownership
  and navigation-time replay. CLM does not create, deduplicate, or retire MCP
  process chains.
- [#26812 — Repeated `git.exe` and `conhost.exe`
  spawning](https://github.com/openai/codex/issues/26812) concerns Git workspace
  discovery and process pressure. It is not evidence for changing conversation
  storage.
- [#29593 — Restart loop after corrupted local
  state](https://github.com/openai/codex/issues/29593) concerns local-state
  corruption and recovery. CLM's transactional rollback reduces its own write
  risk but is not a general Codex state repair tool.
- [#26165 — Crash while opening a local file link with the default
  app](https://github.com/openai/codex/issues/26165) concerns the Desktop shell
  handoff path. It demonstrates a separate crash surface and does not validate
  CLM's history mechanism.

不是一个根因，也不是一个工具能全修。

The adjacent reports matter because they prevent a misleading universal claim:
removing long-history resume pressure can make managed tasks materially faster
and more stable, but it cannot repair signed-frontend virtualization, Git
Review containment, MCP lifecycle, shell activation, or unrelated state
corruption. CLM's credibility depends on preserving that boundary.

## Upstream Codex evidence

- App-server paging, `excludeTurns`, `initialTurnsPage`, and compaction:
  https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md
- Exact packaged source tag `rust-v0.144.2` was inspected at commit
  `a6645b6b8a656360fa16fb7e1c6721d0697d3d6a`; current main was also inspected at
  `c7a4a7e136d96554e1fc6f66532e6060fd2aaf15`
  on 2026-07-13:
  https://github.com/openai/codex/commit/8b2c84ddccafe40dc0dc09f9f52bcbdc9dc45d66
- Rollout loading now streams lines, but still collects all parsed items into a
  `Vec<RolloutItem>` before returning resume history:
  https://github.com/openai/codex/blob/main/codex-rs/rollout/src/recorder.rs
- A surviving replacement-history checkpoint is a complete history base, and
  the source notes that an eventual lazy design should use a resumable reverse
  source instead of an eagerly loaded slice:
  https://github.com/openai/codex/blob/main/codex-rs/core/src/session/rollout_reconstruction.rs
- Compaction implementation and retained-user-message behavior:
  https://github.com/openai/codex/blob/main/codex-rs/core/src/compact.rs
- A public Store Desktop diagnostic shows the packaged client launching its own
  `resources/codex.exe app-server` process:
  https://github.com/openai/codex/issues/20206
- The current app-server source explicitly states that `thread/turns/list`
  replays the entire rollout on every request until turn metadata is indexed:
  https://github.com/openai/codex/blob/main/codex-rs/app-server/src/request_processors/thread_processor.rs
- Upstream has already introduced `ThreadStore::list_turns`,
  `ThreadStore::list_items`, `ThreadHistoryMode::Paginated`, and a
  `thread_history_1.sqlite` schema, but the local store still rejects paginated
  threads and does not implement those paging methods:
  https://github.com/openai/codex/blob/main/codex-rs/thread-store/src/store.rs
  https://github.com/openai/codex/blob/main/codex-rs/thread-store/src/error.rs
  https://github.com/openai/codex/blob/main/codex-rs/state/thread_history_migrations/0001_thread_history.sql
- Main has begun persisting paginated history (`thread_turns`, `thread_items`,
  and projection state), but the app-server API path still does not consume that
  store. The local proxy remains a compatibility bridge, not a claim that
  upstream work is complete.

## Interpretation boundary

Streaming input avoids one giant source string but retaining the complete parsed
vector still consumes host RAM and CPU. It is not equivalent to sending the
whole file into model context. Renderer history and model-visible history must
be measured separately.

The existing paging protocol makes lazy full-history browsing technically
coherent. It does not by itself prove that the current Store frontend avoids
draining every page, nor that it can attach to a custom local backend. Those are
Phase 1 runtime-seam measurements, not assumptions.
