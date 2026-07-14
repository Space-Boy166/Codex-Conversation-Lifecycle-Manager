# Codex Desktop performance troubleshooting

This document records independent mitigations for other Codex Desktop stalls.
They are not CLM features, and `CLMSetup.exe` does not apply them.

## Git Review process storms

Some Codex Desktop builds repeatedly inspect Git state when a task is opened or
a turn is sent. Large dirty repositories, generated files, and many open tasks
can amplify the work.

Useful containment options:

1. Start Codex in an empty, non-Git control directory and work on real projects
   through explicit absolute paths.
2. Keep generated dependencies, caches, media exports, and build artifacts out
   of repository discovery with reviewed project-specific ignore rules.
3. Commit coherent work regularly and stage exact paths rather than using
   `git add -A` as a discovery command.
4. On builds that support it, `git-review-mode = "last-turn-only"` reduces the
   automatic Review surface. Treat this as version-sensitive and verify it
   again after an update.

An empty control directory is containment, not an official Review-off switch.
Do not move project source into it or create fake Git metadata there.

## BelowNormal process priority

Moving a busy backend worker to Windows `BelowNormal` priority can preserve
pointer and desktop responsiveness during a spike. It is scheduling relief,
not a fix for eager history reconstruction, and it can make the affected
background operation finish more slowly.

Priority changes reset when the process exits unless a separate launcher or
policy reapplies them. CLM deliberately does not install such a policy.

## Duplicate MCP servers

Local MCP servers are real processes. Multiple task-owned Blender, browser, or
Node-based MCP chains can consume CPU and memory even when they expose the same
tool name. Keep heavy local MCP servers off by default and verify ownership
before stopping an instance. Do not kill broad `node.exe` or `python.exe`
families.

## New-window compositor hitch

Creating a new Electron window can briefly initialize a renderer, GPU process,
storage service, and compositor resources before any conversation resumes.
CLM does not claim to remove that path because no history has been selected yet.

## Useful issue evidence

Include the Codex Store package version, bundled backend hash/version, rollout
size, task-open timing, process CPU/memory deltas, and whether the stall occurs
before or after `thread/resume`. Keep Git, MCP, history paging, and blank-window
startup as separate reproduction paths.
