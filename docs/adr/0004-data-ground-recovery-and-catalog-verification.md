# ADR 0004: Crash-safe Data Ground promotion and catalogue verification

- Status: Accepted for RG-R01 implementation
- Date: 2026-10-18
- Scope: Dataset snapshot promotion and integrity reads

## Context

Dataset promotion spans a filesystem rename and a SQLite transaction. Those operations cannot commit atomically together. Catalogue reads also hash the complete raw-source and Parquet files, so performing a full catalogue scan in a synchronous Tauri command can block the desktop event loop.

## Decision

- Before moving a confirmed staging directory, SQLite records a durable pending-promotion journal with the dataset metadata, staging UUID, and immutable import-attempt identity.
- After the atomic directory rename, one SQLite transaction inserts the registry row and import attempt and removes the journal. Startup recovery verifies the journal, manifest, recomputed dataset fingerprint, source hash, and Parquet hash before completing an interrupted promotion.
- A missing, conflicting, or tampered recovery path blocks that store open and preserves its files and journal. Unjournaled directories from older interrupted commits can be recovered only when the complete snapshot validates; ambiguous directories are never replaced or deleted. Unconfirmed abandoned stages remain unregistered and preserved.
- Catalogue verification uses one bounded worker per store, keyset-paged SQLite metadata, and streaming SHA-256 checks. It reports each verified dataset and supports cancellation between file-read chunks. A UI catalogue entry is exposed only after its complete snapshot verifies; there is no trust cache that can leave a stale dataset marked `READY`.
- Inspection and compatibility catalogue commands also run their hashing work away from the Tauri main thread.
- The Tauri process retains the active import job and prepared preview; a read-only resume command lets the UI restore it after navigation and cancel or confirm it.

## Consequences

- SQLite schema version 5 adds the pending-promotion journal.
- A confirmed import can be completed after a process crash without requiring a second import or overwriting an existing directory.
- A damaged or ambiguous promoted snapshot fails closed and requires explicit repair outside automatic startup recovery.
- Catalogue verification remains proportional to bytes on disk, but uses bounded memory, one worker, progress updates, and cancellation while leaving the UI event loop available.
- The navigation resume contract is process-local. A prepared, unconfirmed import is not promoted after a process restart; its staging files are preserved rather than treated as accepted data.

## Verification

Rust and frontend regression tests cover navigation resume, IPC visibility, interrupted promotion and retry, ambiguous-file preservation, abandoned-stage preservation, large-file cancellation, and asynchronous catalogue progress. Passing these tests does not substitute for interactive Owner UI review.
