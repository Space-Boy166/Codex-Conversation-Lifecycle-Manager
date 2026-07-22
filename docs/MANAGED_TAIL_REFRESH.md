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

The public alpha does not install the machine-specific launcher integration.
On this machine, the verified Desktop shortcut routes through
`CodexClmLifecycleLauncher.exe`, which waits on the exact guarded root process
and invokes this same transaction after a natural full exit. Manual use still
requires every Codex owner to be closed.

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

### Missing previous rollback

The immutable Vault archive is the byte-exact source authority. A previous
same-volume rollback may be absent after an independently verified recovery or
older incident cleanup. That absence no longer deadlocks refresh:

- the archive must still match the manifest byte count and SHA-256;
- the active prefix, index, thread id, candidate sidecar, rollback sidecar,
  archive subtree, and index subtree must all pass topology checks;
- a successful refresh creates and verifies the new generation's same-volume
  rollback normally;
- a failed refresh restores the previous small active rollout, index, and Vault
  and restores the rollback to its exact prior state, including remaining
  absent when it was absent before the attempt.

A rollback path that exists as a directory, has the wrong hash, or escapes the
active rollout's exact sidecar path is a hard refusal. CLM never substitutes an
unrelated file or invents rollback provenance.

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
Codex for the user. Machine-local exit maintenance is event-driven: one
lifecycle owner waits on the exact Desktop process handle, then scans and
rotates serially only after the owner exits. There is no periodic task, file
watcher, process polling loop, or live-rollout rewrite.

The public transaction remains deliberately offline and explicit. It rotates
only around a newer useful checkpoint that Codex produced naturally. If no such
checkpoint exists, refresh refuses to activate a larger generation and leaves
the previous model semantics and rollout bytes unchanged. File size or host
pressure never triggers semantic native Compact.

Refresh cannot safely replace an active rollout while Codex owns it. A future
public automatic lifecycle must first prove exact process ownership, recovery,
and rollback without broad process control.

## Compact-image policy inheritance

When a user has separately approved Compact-image externalization, the active
manifest records `exact_archive_with_model_reference_v1`. A later offline
refresh inherits that policy. If the new native Compact contains supported
inline Base64 images, refresh first builds and verifies the ordinary exact
candidate, then archives those image bytes, activates lightweight references,
and requires the final candidate to reduce Resume bytes. If the new Compact has
no inline images, ordinary refresh continues and the policy remains available
for a future generation.

This policy never triggers native Compact and never runs while Codex is open.
Its semantic boundary and real-copy evidence are documented in
[Compact Image Externalization](COMPACT_IMAGE_EXTERNALIZATION.md).
