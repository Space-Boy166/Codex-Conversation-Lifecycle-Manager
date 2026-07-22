# Codex Store Frontend Upstream Blocker

## Decision

As of 2026-07-14 and Store build `26.707.9564.0`, Conversation Lifecycle
Manager has reached the safe local optimization ceiling for native lazy-history
UX.

The managed backend path is proven: exact history remains recoverable, model
resume is bounded, automatic older-page draining is stopped, manual upward
paging reaches the exact first message, and requested managed pages return in
5-11 ms. The remaining defects belong to the signed Store frontend:

- the native user-message navigation rail disappears while history is
  incomplete;
- a rail marker cannot reveal a turn that is not already materialized;
- upward paging exposes no loading, failure, or retry feedback at the history
  boundary.

Status: `blocked_upstream`. This is not a failed local implementation and not a
claim that the missing UX is fixed.

A separate 2026-07-19 proxy fix restores explicit managed
`thread/read(includeTurns=true)` from the complete SQLite API-turn projection.
That repairs API/task-tool compatibility only. It does not make the signed
frontend render unloaded navigation markers, direct target reveal, or visible
upper-boundary loading state, so the frontend status here remains unchanged.

## Proven ownership boundary

Read-only inspection of the current Store bundle established that:

1. the local conversation thread enables the rail only when
   `isConversationHistoryComplete` is true;
2. its navigation-item collector returns no items while history is incomplete;
3. the rail stays hidden below four user-message items;
4. `onRevealItem` can scroll only to a turn already present in the virtualized
   list;
5. older-page loading state exists, but the upper history boundary and rail do
   not render it.

The CLM proxy owns app-server requests and responses. It does not own Store
component rendering, virtual-list navigation, or loading-state presentation.
Therefore another proxy response rewrite cannot supply the missing behavior.

## Rejected local routes

Do not reopen any of these routes without a new user-approved product decision:

- **Claim history is complete:** restores eligibility for the rail but removes
  the native older cursor and stops true upward paging.
- **Preload every full turn:** restores the rail by recreating the memory,
  renderer, and startup pressure that lazy loading was built to remove.
- **Inject summary or placeholder turns:** the current frontend has no supported
  path that later hydrates those incomplete bodies; it risks presenting false
  or truncated history.
- **Sequentially load pages before a jump:** there is no unloaded-target reveal
  callback, and draining all intermediate pages defeats direct navigation.
- **Patch `app.asar` or inject runtime JavaScript:** violates the signed-package
  safety boundary and is fragile across Store updates.
- **Add an external overlay:** does not preserve the native Codex feature and
  introduces another UI, lifecycle, and focus owner.

More proxy tuning cannot change a frontend ownership boundary.

## Official unlock conditions

Reopen this work only when an official Codex release or supported integration
seam provides at least the capabilities needed for the complete route:

1. a lightweight navigation catalog can exist independently of loaded turn
   bodies and `isConversationHistoryComplete`;
2. selecting an unloaded message anchor can request one indexed target page and
   reveal it after materialization;
3. the upper history boundary and selected marker can render pending, failure,
   and retry states;
4. the integration is versioned or otherwise supportable without modifying the
   signed Store package.

A new Store version number, a renamed bundle, or another backend paging API is
not sufficient by itself. The actual frontend behavior must be re-inspected.

## Reactivation checks

When an official unlock appears:

1. inspect the new package read-only and identify the supported frontend seam;
2. prove the navigation catalog contains only bounded metadata, never full
   assistant bodies, tool payloads, image bytes, or model context;
3. fixture-test direct target-page resolution without draining intermediate
   pages;
4. verify loading, failure, and retry feedback in the real UI;
5. repeat exact-oldest-message, old-image, return-to-bottom, renderer-retention,
   title, identity, append, and rollback checks;
6. require explicit user approval before any new live canary or fleet change.

Until those checks are possible, keep the installed CLM runtime unchanged and
treat this native-UX track as parked.

## Cross-renderer root-event routing

Read-only evidence on 2026-07-19 adds a second signed-client boundary. The
Store log records request origins as `originWebcontentsId`, but that identity is
owned by the Electron main process and is not present on the shared app-server
JSON stream seen by CLM. Backend root-task notifications contain a thread id,
not a destination renderer id or subscription token.

CLM can safely drop the four stream-event variants for a child thread that no
Desktop request has ever referenced. It cannot safely drop an event for a root
thread once any renderer has referenced it: the proxy would also starve the
actual owner. Writing the event once to stdout then leaves the signed main
process free to broadcast it to every renderer, which produced 1,374
`unknown conversation` deliveries in the audited Store log.

Do not claim root-event isolation from the current global owned-thread set. Do
not patch `app.asar`, inject renderer JavaScript, or fabricate routing metadata.
Reopen this boundary only if the official protocol exposes a per-renderer
connection/subscription identity, or the signed main process demonstrably stops
broadcasting root events to non-owners.
