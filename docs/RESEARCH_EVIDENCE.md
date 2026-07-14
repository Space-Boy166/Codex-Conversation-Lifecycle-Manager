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
