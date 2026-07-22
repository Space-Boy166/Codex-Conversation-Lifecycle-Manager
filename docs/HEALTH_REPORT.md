# CLM Health Report

`tools/Get-CodexClmHealth.ps1` is a one-shot, read-only operator report. It
does not install CLM, refresh a task, alter Codex state, launch or stop a
process, register a watcher, or poll in the background.

Run the ordinary report from the source tree:

```powershell
.\tools\Get-CodexClmHealth.ps1
```

Use JSON for automation or a saved incident snapshot:

```powershell
.\tools\Get-CodexClmHealth.ps1 -AsJson
```

The report reconciles five independent surfaces:

1. the live CLM proxy, official backend, Desktop roots, and renderers;
2. the Desktop shortcut and lifecycle installation state;
3. every managed manifest, active tail, immutable archive, rollback, and
   SQLite index;
4. the latest exit-maintenance outcome;
5. the newest Codex Store log's unknown-conversation fan-out, `skills/list`
   latency and CLM cache hit/store evidence, Resume/list calls, Review capture
   calls, and Git-unavailable warnings.

The default report uses metadata only. `-DeepIntegrity` additionally computes
SHA-256 for every managed immutable archive, which can read many gigabytes and
should be reserved for an offline audit or an explicitly selected maintenance
window.

The Store log and `session_index.jsonl` are opened read-only with Windows
read/write/delete sharing. The one-shot report can therefore inspect files that
Codex is actively appending without stopping Codex or taking ownership of them.

The top-level `state` is deliberately conservative:

- `runtime_not_effective`: the expected one-proxy/one-backend chain is absent;
- `paging_effective_lifecycle_disarmed`: lazy paging is live, but automatic
  offline tail maintenance is not installed;
- `paging_effective_tail_maintenance_needed`: paging and lifecycle are present,
  but one or more active tails reached the maintenance threshold;
- `paging_and_lifecycle_effective`: the observed runtime and threshold checks
  are healthy.

This report diagnoses state; it is not authorization to repair it. A real task
refresh still requires a complete Codex exit, exact transaction preflight, and
post-reopen acceptance.
