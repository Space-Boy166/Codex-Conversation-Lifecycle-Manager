# Bounded Skills Discovery Cache

## Problem

Codex Desktop repeatedly requests `skills/list` for the same working-directory
set. The current official protocol already defines `forceReload=true` as a disk
rescan that bypasses the backend's own cache, yet the 2026-07-19 Store log still
showed 25 ordinary requests with a 5,332 ms p95 and a 5,649 ms maximum.

CLM therefore adds a small in-memory response cache at the existing app-server
proxy seam. It reduces duplicate discovery work without changing skill files,
Codex configuration, or the signed Desktop package.

## Contract

- Only successful `skills/list` responses are cached. Errors and malformed
  responses are always passed through and never stored.
- The key is the exact protocol `params` object with `forceReload` removed.
  Working-directory order remains significant because the response is ordered
  by the caller's request.
- A hit preserves the complete official response and replaces only `id` with
  the current caller's request id.
- Entries live for at most 30 seconds and the cache holds at most eight distinct
  parameter sets.
- Pending bookkeeping lives for at most 120 seconds and holds at most 64 ids.
  Eviction only forfeits cache eligibility; it never drops the backend response.
- `forceReload=true` clears the prior generation and always reaches the official
  backend. An ordinary request racing that force reload cannot return or replace
  the cache with stale data.
- `skills/config/write`, plugin install/uninstall, and plugin share mutations
  invalidate the current generation before they are forwarded.
- External filesystem edits that bypass those protocol mutations become visible
  after the 30-second TTL or immediately through `forceReload=true`.

The cache is process-local and is not persisted. It stores only metadata that
the backend already returned to Desktop.

## Verification

Three Rust protocol tests cover:

1. success reuse and caller-id rewriting;
2. force reload and mutation invalidation;
3. TTL, entry/pending bounds, error rejection, generation rejection, and a
   concurrent ordinary-request versus force-reload race.

`tests/Test-SkillsListCacheCanary.ps1` then launches the newly built proxy
against the pinned official `codex-0.144.2` backend in a temporary `CODEX_HOME`.
The first real request took 51.980 ms in the recorded run; the identical second
request took 0.799 ms, returned the same result, used the second request id, and
emitted both `CLM skills/list cache store` and `CLM skills/list cache hit`.

The same canary also passed against the current Store package
`26.715.3651.0`, whose copied backend reports `codex-cli 0.145.0-alpha.18`:
78.152 ms for the first request and 0.400 ms for the cached request. The Store
binary was copied only into a guarded temporary probe directory and was not
installed or substituted into the production CLM runtime.

This proof is source/offline acceptance. It does not change the installed proxy.
A production upgrade still requires the shared full-exit window and normal
post-reopen task, paging, append, Archive, process-tree, and log acceptance.
