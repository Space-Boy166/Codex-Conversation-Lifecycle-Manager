# Security and privacy

Conversation Lifecycle Manager runs locally. It does not upload conversation
content, require an API key, or include a telemetry client.

## Data access

The CLM proxy sits between Codex Desktop and the locally installed official
Codex app-server. It can therefore observe the local JSON-RPC conversation
traffic needed to provide indexed history pages. Indexes, manifests, archives,
and rollback files stay under the selected CLM runtime directory.

`CLMSetup.exe` copies the Codex backend from the user's installed Microsoft
Store package into the local runtime. Releases never redistribute OpenAI's
`codex.exe`, `ChatGPT.exe`, Store package, credentials, or user conversations.

## Safety model

- Real rollout files are changed only while Codex Desktop and its app-server
  are closed.
- Activation requires a native Codex replacement-history checkpoint.
- A byte-exact archive and same-volume rollback copy are retained.
- Every source, archive, candidate, and restored prefix is SHA-256 checked.
- Restore rehydrates the full original and appends records created after CLM
  activation, so continued work is not silently discarded.
- An ambiguous or failed verification leaves the active source untouched or
  retains both sides for manual recovery.

## Reporting a vulnerability

Use GitHub private vulnerability reporting when it is available for the
repository. Do not paste private conversation content, credentials, rollout
files, or local environment dumps into a public issue. A minimal synthetic
fixture and redacted logs are preferred.
