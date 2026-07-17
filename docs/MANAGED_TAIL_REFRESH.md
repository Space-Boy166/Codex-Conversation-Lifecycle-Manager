# Managed Tail Refresh

## The regression this closes

CLM lazy paging and active-rollout size are separate boundaries.

The validated live canary originally activated a 6,490,905-byte rollout. Old
history remained indexed and manually pageable, but continued work appended
new turns, tool records, reasoning records, and native Compact records to the
active JSONL. By 2026-07-16 that same active file had reached 151,236,658 bytes.
The proxy was still serving old pages lazily; the official backend was simply
being asked to Resume a large mutable tail again.

That distinction matters. A running proxy is not proof that task-open cost is
still bounded.

## Command

```powershell
conversation-lifecycle-manager.exe refresh-migration `
  --manifest <runtime-root>\Data\Vault\Codex\<thread-id>\manifest.json `
  --backend <runtime-root>\Backend\<version>\codex.exe `
  --runtime-root <runtime-root>
```

Do not pass `--fixture` for a real task. The normal command refuses to run while
`ChatGPT.exe`, `codex.exe`, or `codex-clm-proxy.exe` still owns Codex state.

The public alpha does not install an automatic shutdown or relaunch worker for
this command. Close every Codex Desktop window yourself, confirm that no Codex
owner remains, and retain the existing CLM vault and rollback generation before
running an advanced manual refresh.

## Transaction

1. Verify the canonical manifest, active prefix, archive, rollback, index, and
   thread identity.
2. Rehydrate the byte-exact original archive plus every complete record appended
   after activation.
3. Rotate the previous manifest vault and same-volume rollback into timestamped
   generation paths.
4. Use the pinned official backend to project the full current rollout and
   prepare a new sidecar index.
5. Build a new active candidate from the latest valid native Compact record and
   its exact suffix.
6. Refuse activation when the candidate is not smaller than the current active
   file. This prevents a refresh without a newer useful Compact from adding
   churn while gaining nothing.
7. Activate and verify the new active file, full archive, full rollback, index,
   hashes, and retained previous generation.

Any failure after rehydration preserves the attempted generation under
`clm-refresh-failed-*` paths and restores the previous managed active rollout,
index, manifest vault, and rollback. No failed artifact is silently deleted.

## Validation evidence

The 2026-07-16 fixture transaction used the pinned official
`codex-0.144.2` backend:

- managed active before refresh: 24,903 bytes;
- managed active after refresh: 6,207 bytes;
- full rehydrated history: 154,080 bytes;
- previous manifest, rollback, active file, and index all retained;
- official backend resumed the refreshed active rollout;
- restoring the new generation reproduced the full-history SHA-256 exactly.

A second fixture intentionally appended data without a newer Compact. Refresh
was rejected, the command returned failure, and the active, index, manifest,
and rollback hashes all matched their pre-refresh values.

## Automation boundary

The public `CLMSetup` alpha does not schedule refreshes, add a watcher, or close
Codex for the user. Event-driven exit maintenance is still an internal local
integration, not part of the downloadable public release.

The public transaction remains deliberately offline and explicit. It rotates
only around a newer useful checkpoint that Codex produced naturally. If no such
checkpoint exists, refresh refuses to activate a larger generation and leaves
the previous model semantics and rollout bytes unchanged. File size or host
pressure never triggers semantic native Compact.

Refresh cannot safely replace an active rollout while Codex owns it. A future
public automatic lifecycle must first prove exact process ownership, recovery,
and rollback without broad process control.
