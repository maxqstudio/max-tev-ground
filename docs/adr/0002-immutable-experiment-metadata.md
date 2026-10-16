# ADR 0002: SQLite metadata and immutable experiment identity

- Status: Accepted for RG-R00
- Scope: Metadata foundation only

## Decision

SQLite stores versioned experiment manifest JSON, its SHA-256 digest, source revision, creation time, and lifecycle status. A primary-key collision is an error; existing experiment metadata is never overwritten. Schema migrations are versioned, checksummed, and applied in transactions.

Historical market bars are out of scope for this phase. The later Data Ground phase owns Parquet ingestion and dataset lineage.

## Reason

A reproducible result needs an auditable record of every input. An append-only insert boundary prevents edited configurations from masquerading as the original run. Separating large market history from local metadata keeps the RG-R00 schema small.

## Consequences

- A new configuration requires a new experiment ID.
- Database schema version is recorded with SQLite user_version and a migration ledger.
- Migration failures roll back their DDL, ledger row, and version update.
- Database tests use explicit contract fixtures and make no benchmark or PnL claim.