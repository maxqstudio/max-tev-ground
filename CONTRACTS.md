# Research contracts (schema 1)

The executable Rust contract is rg-contracts::ExperimentManifest. JSON uses strict typed objects (deny_unknown_fields) and the version field is mandatory. A changed configuration gets a new ExperimentId; SQLite inserts are append-only and reject duplicate IDs.

Every manifest carries:
- Dataset ID, SHA-256 fingerprint, source, symbol, timeframe, UTC candle-open/close/decision-availability fields, decision rule, and an explicit missing-data policy.
- Feature engine ID/version plus every enabled feature ID, version, timeframe, and parameter map.
- Deterministic strategy ID/version/parameters.
- Optional model name, immutable digest, adapter/runtime configuration, and required authority: ADVISORY_ONLY, VETO_ONLY, RISK_ADJUSTMENT, or INDEPENDENT_DECISION.
- Optional versioned policy identity and rules digest.
- Risk, fill/spread/commission/slippage simulator configuration, temporal split, OOS exposure, source version, and random seed.

Timestamps in the current schema are canonical UTC YYYY-MM-DDTHH:MM:SSZ. Temporal boundaries are ordered. Missing bars are rejected or preserved as gaps; the contract has no synthetic-fill option. Scores are not labeled calibrated win probabilities by this foundation. Calibration evidence and policy execution are future-phase work.

Headless research contracts do not depend on Tauri, React, SQLite, or the desktop shell. Persistence depends on contracts, never the reverse.

# Feature-set contracts (RG-R02, schema 1)

The executable Rust contract is `rg-feature-studio::spec`. Requests (`FeatureSetRequest`) use strict typed JSON (`deny_unknown_fields`) and name inputs only by registered dataset ID (`ds_<sha256>`), exact symbol (broker suffixes are significant), and canonical timeframe. No request field accepts a filesystem path.

- Indicators are native Rust registry entries with a stable ID, integer version, typed parameters (integer, number, or choice, each bounded), declared inputs, outputs, formula, and warmup. Defaults are filled before hashing, so equivalent requests normalize to one identity. Unknown indicators, versions, parameters, and out-of-range or non-finite values are rejected.
- Temporal semantics version 1: a row's decision time is the target bar's `available_at_utc`; the target bar that closes at that time is included. Warmup rows are null with status `WARMUP`. A reference bar is visible only when `available_at_utc + availability_lag_seconds <= decision`; the latest visible bar is used. A reference older than `max_staleness_seconds` is `STALE`, and one not yet observed is `NO_OBSERVATION`. Windows count observed closed bars; gaps are neither filled nor resampled.
- Identity: `feature_set_id = "fs_" + sha256(FeatureSetIdentity)`, where the identity binds schema version, engine ID/version, temporal semantics, target series, normalized features, and every input dataset's ID, fingerprint, canonical bars SHA-256, canonical row fingerprint, and bar count. Resource quotas are not part of the identity. Changing any content input yields a new identity, and a replay of an existing identity must reproduce its output SHA-256 exactly or the commit fails closed.
- Output: `features.parquet` has `open_time_utc`, `close_time_utc`, and `decision_time_utc`; per feature, a nullable `<key>__<output>` float column per output and a `<key>__status` column; and, for reference features, `<key>__source_available_at_utc`. Rows are sorted by decision time. The manifest records the output SHA-256, a row-level output fingerprint, per-feature status counts, input provenance lineage, and `READY` or `BLOCKED` status (`BLOCKED` when any feature has no ready row).
- Quotas: `max_target_rows` bounds the target series; `max_input_rows` bounds the **aggregate** bar count of the unique physical datasets a request reads, each counted once however many features reference it, and is checked once with overflow-checked arithmetic before materialization; `max_output_bytes` bounds the output file. Any excess or overflow fails as `RESOURCE_QUOTA`.
- SQLite schema 6 adds immutable `feature_sets`, `feature_set_inputs` (foreign keys to `datasets`), a `feature_set_promotions` journal, and immutable `feature_attempts`. Catalogue reads (`list_feature_sets`, `inspect_feature_set`) recheck the manifest and output digest, and re-verify every source snapshot through Data Ground: the current raw-source, canonical-bars, and manifest bytes must match their registered digests, the dataset must be `READY`, and its fingerprint, canonical bars SHA-256, canonical row fingerprint, and bar count must equal the feature set's recorded identity. A missing, modified, unverifiable, or non-`READY` source makes the feature set `QUARANTINED` with a diagnostic naming the dataset.
- Downstream use: `FeatureStore::output_path` is the only accessor for a registered output and returns a path only for a feature set that is `READY` after that full re-verification; `BLOCKED`, `QUARANTINED`, and unregistered sets fail with `FEATURE_SET_NOT_USABLE`. Readers must still compare the output digest with the manifest around each read.
