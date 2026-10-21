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
- Downstream use: `FeatureStore::output_path` and `FeatureStore::consumable_feature_set` are the only accessors for a registered output, and they return a path only for a feature set that is `READY` after that full re-verification. `BLOCKED`, `QUARANTINED`, and unregistered sets fail with `FEATURE_SET_NOT_USABLE`. Readers must still compare the output digest with the manifest around each read.

# Decision-set contracts (RG-R03, schema 1)

The executable Rust contract is `rg-strategy-engine::spec`. A `DecisionSetRequest` uses strict typed JSON (`deny_unknown_fields`). It names exactly one registered feature set (`fs_<sha256>`), a strategy ID and version, numeric parameters, a binding per strategy role (`feature_key` and `output`), and quotas. No request field accepts a filesystem path, an order size, or any broker field.

- Strategies are native Rust registry entries: `THRESHOLD_BAND` v1 and `TREND_CONTEXT` v1. Each declares a stable ID, an integer version, bounded numeric parameters, roles with a required source (`TARGET` or `REFERENCE`), a decision rule, equality semantics, and a warmup rule. Definitions never name a symbol, broker suffix, or timeframe; the target series comes from the feature set. Defaults are filled before hashing (and `-0.0` is normalized), so equivalent requests normalize to one identity. Unknown strategies, versions, parameters, and roles are rejected, as are out-of-range, non-finite, or inconsistent parameters (`lower >= upper`, `context_min > context_max`). A role must bind an existing output of a feature whose source matches the role.
- Decision semantics version 1:
  - There is exactly one decision per feature-set row, that is, per closed bar of the target series, in ascending decision-time order.
  - A decision reads only its own row's feature values, which RG-R02 already made causal. Nothing is recomputed, joined, or read from another row.
  - Reference features are context only and never create decision rows.
  - Precedence: roles are checked in definition order, and the first `WARMUP`, `STALE`, or `NO_OBSERVATION` input gives `SKIP` with that reason and role. A missing or non-finite `READY` value gives `NON_FINITE_INPUT`. Only then does the rule run: `CONTEXT_FILTER` first, then `NUMERIC_OVERFLOW` when a value the rule derives from finite inputs is not finite (for example `fast - slow` overflowing), then `CONFLICT` when the BUY and SELL conditions both hold, then `BUY_RULE`, `SELL_RULE`, or `NO_SIGNAL`.
  - Directional comparisons are strict; the `TREND_CONTEXT` context band is inclusive.
- Identity: `decision_set_id = "dc_" + sha256(DecisionSetIdentity)`. The identity binds:
  - the schema version and the engine ID and version;
  - the decision semantics;
  - the normalized strategy (ID, version, parameters, and role bindings with their sources);
  - the consumed feature set's ID, identity SHA-256, manifest SHA-256, output SHA-256, row fingerprint, row count, and target series.

  Quotas are not part of the identity. A replay of an existing identity must reproduce its output SHA-256 exactly, or the commit fails closed.
- Input verification: preview takes the feature set through `consumable_feature_set` and copies its output into a private stage while hashing; the copy must match the manifest SHA-256. While evaluating that copy, the engine checks the Parquet schema against the manifest and recomputes the RG-R02 row fingerprint and row count. Rows must be in strictly increasing decision-time order, and any unrecognized status fails closed. The copy is deleted before promotion.
- Output: `decisions.parquet` has these columns, in this order:
  - `symbol`, `timeframe`, `open_time_utc`, `decision_time_utc`;
  - `decision` (`BUY`, `SELL`, or `SKIP`), `reason_code`, and a nullable `reason_role`;
  - one nullable `input__<role>` float per role, holding the exact registered value a decision used.

  The manifest records the output SHA-256, a row-level output fingerprint, BUY/SELL/SKIP counts with SKIP reasons, the target and context series, the feature set and its dataset lineage, and `READY` status.
- Quotas: `max_rows` bounds decision rows and is checked before evaluation and per row. `max_output_bytes` bounds the output file: it is checked while batches are written, and again on the file's final physical length after the Parquet footer is written. Any excess fails as `RESOURCE_QUOTA`, and the staged output is removed.
- SQLite schema 7 adds an immutable `decision_sets` table (foreign key to `feature_sets`, recording the feature-set manifest SHA-256), a `decision_set_promotions` journal, and immutable `decision_attempts`.
  - Every catalogue read (`list_decision_sets`, `inspect_decision_set`) rechecks the manifest and output digest. It also re-verifies the consumed feature set through `FeatureStore`, and thereby every source dataset.
  - A feature set that is missing, changed, or not `READY` makes the decision set `QUARANTINED`, with a diagnostic naming the feature set.
  - Confirmation re-verifies the feature set first, so a change between preview and confirm fails closed.
  - `StrategyStore::output_path` returns a path only for a `READY` decision set.
- Schema upgrade safety: `open_database`, the only way any store opens the database, never upgrades an existing store without a verified backup.
  - Before migrating a store below the current schema, it writes `schema-backups/<db>.schema-v<N>.<id>.sqlite3` with SQLite `VACUUM INTO`. This reads through the live connection, so committed content still in the WAL is included.
  - The backup must pass `PRAGMA integrity_check`, have the source's schema version, and reproduce the source's logical content SHA-256, which covers every schema object and row. Otherwise it is removed and the open fails before any version change. A full disk surfaces as a failed write at this step.
  - A failed migration rolls back and leaves the store at its old version, with the verified backup kept.
  - Each backup gets a receipt, `<backup>.receipt.json`, written atomically once the backup is verified. It records the backup and source database file names, the schema version, the source's logical content SHA-256, and the backup file's raw SHA-256 and size. If the receipt cannot be written, the backup is removed and the upgrade does not start.
  - `verify_schema_backup` compares a backup with its receipt and never derives the expected values from the backup itself. The receipt must exist, be well formed, and name this backup and database; the raw file hash and size must match; and SQLite's integrity check, schema version, and logical content digest must equal the receipt. `rg-schema-backup list` marks each backup `VERIFIED` or `UNVERIFIED`.
  - `restore_schema_backup` (`rg-schema-backup restore`, `RESTORE_DATABASE_BACKUP.ps1`) accepts only a backup that verifies against its receipt, and refuses while the database is in use. It renames the current database, WAL, and shared-memory files aside (never deleting them), installs a copy, and checks it against the receipt. If that final check fails, the installed copy is moved aside as `.failed-restore-*` and the original files are put back; if that rollback cannot complete, the error names where each state is. A successful restore lets a schema 6 build reopen the store with its dataset and feature-set content unchanged.
  - Trust limit: the receipt detects accidental or out-of-band changes to a backup alone. It is a checksum, not a signature, so it does not protect against someone able to rewrite both the backup and its receipt.
- Decisions are research records. Nothing in this contract places, sizes, routes, or simulates an order, or computes a position, PnL, backtest, or performance metric.

# Model registry and inference contracts (RG-R04, schema 1)

The executable Rust contract is `rg-model-registry::spec`. Every request type uses strict typed JSON (`deny_unknown_fields`). No request field accepts a filesystem path, an endpoint other than the stored loopback setting, an order or position field, or any broker field.

- Adapters:
  - `OLLAMA_LOOPBACK` (`ollama-generate-json/1`) uses models already installed in an Ollama runtime at the Owner-configured loopback endpoint (default `http://127.0.0.1:11434`).
  - `ONNX_NATIVE` (`rg-native-onnx/1`) runs a `.onnx` file copied into application storage through the built-in interpreter. It supports opsets 7–21 of the default domain and the operators `Add`, `Clip`, `Concat`, `Div`, `Flatten`, `Gemm`, `Identity`, `LeakyRelu`, `MatMul`, `Mul`, `Relu`, `Reshape`, `Sigmoid`, `Softmax`, `Sub`, and `Tanh`. A model needs one FLOAT input of shape `[N]` or `[1|batch, N]` and FLOAT outputs. Anything else is refused at discovery with a diagnostic naming the reason.
- Discovery: `discover_ollama` lists installed models with their manifest digests, and `stage_onnx_candidate` copies and inspects a selected file. Both produce unregistered candidates only. An absent runtime is `UNAVAILABLE`, and a listing with any entry lacking a valid name or SHA-256 digest is `MALFORMED`; neither is ever reported as an empty runtime. A name listed more than once is ineligible. Staged ONNX candidates are discarded when the application restarts.
- Registration request: a candidate ID, the declared input field names, the declared outputs, runtime parameters, and `authority`.
  - Output fields are `FLOAT64` with finite `minimum` < `maximum`, or `CATEGORY` with 2–16 unique `UPPER_SNAKE_CASE` labels. ONNX outputs must be `FLOAT64`, and their input and output counts must equal the model's tensor widths.
  - Ollama parameters are `seed`, `max_output_tokens` (16–1024), `context_tokens` (512–32768), and an `instruction` of up to 2000 printable characters. ONNX takes no parameters.
  - Decoding is fixed by the adapter and recorded in the identity. Ollama uses temperature 0, `top_k` 1, `top_p` 1, no streaming, and a JSON-schema answer format. ONNX evaluates FLOAT32, sequentially, one row at a time.
  - `authority` must be `ADVISORY_ONLY`. `DECISION_FILTER`, `DECISION_VETO`, `DECISION_OVERRIDE`, `POSITION_SIZING`, and `ORDER_GENERATION` are refused as `AUTHORITY_REFUSED`; any other value is invalid.
- Model identity: `model_id = "md_" + sha256(ModelIdentity)`. The identity binds:
  - the schema and registry versions, and the adapter and its version;
  - the model name and the content digest (the Ollama manifest digest or the ONNX file SHA-256);
  - the input fields and output schema;
  - the normalized runtime parameters and fixed decoding settings;
  - the ONNX tensor and operator summary, and the authority.

  `preview_registration` computes the identity without registering anything. `register_model` rebuilds the request against the candidate's current content and registers only if it reproduces the confirmed `model_id`; a changed runtime listing or staged file fails as `CANDIDATE_CHANGED`. An ONNX registration stores `model.onnx` beside its manifest.
- Model verification: every catalogue read rechecks the manifest and, for ONNX, the stored file's size and SHA-256. Ollama models are checked against one live listing per read. A missing or changed digest makes the model `QUARANTINED`, with a diagnostic naming the model and both digests. The registration itself never changes. An unreachable runtime makes the model `RUNTIME_UNAVAILABLE`. Inference requires `READY`. For an Ollama model, the registered digest is checked against the live listing before inference, after every 16 generations, after the last row, and at confirmation. A changed or missing digest fails the run or the confirmation as `MODEL_NOT_USABLE` with status `QUARANTINED`, and an unreachable runtime fails it with status `RUNTIME_UNAVAILABLE`; the diagnostic names the check, the model ID and name, and both digests. Ollama returns no content digest with a generation, so these repeated checks narrow but cannot exclude a change that is reverted between two of them; the runtime offers no atomic pinning.
- Inference request: a `model_id`, an input set (`FEATURE_SET` `fs_<sha256>` or `DECISION_SET` `dc_<sha256>`), one binding per declared input field, `authority`, and quotas (`max_rows`, `max_output_bytes`, and `request_timeout_ms` of 100–120000).
  - On a feature set, a field binds one feature output. Its row status must be `READY`, or the row is `SKIPPED` with `WARMUP`, `STALE`, or `NO_OBSERVATION`.
  - On a decision set, a field binds an `input__<role>` value or the decision itself, encoded `BUY` = 1, `SELL` = −1, `SKIP` = 0.
  - A missing or non-finite value skips the row with `NON_FINITE_INPUT` without calling the model.
- Inference semantics version 1: each input row yields exactly one output row, in ascending decision-time order. A row's model input comes only from that row's bound values, and the model gets no timestamp, symbol, or other row. An answer that is not exactly the declared fields, or that has a non-finite or out-of-bounds number or an undeclared label, is `INVALID_OUTPUT` with `SCHEMA_MISMATCH`, `NON_FINITE_OUTPUT`, or `OUT_OF_RANGE`, and no value. A runtime that is unavailable, fails, times out, or drops the connection stops the run; it is never recorded as output.
- Input verification: the input is taken through `consumable_feature_set` or `StrategyStore::output_path`, copied privately while hashing, and must match its manifest. While reading, the engine checks the Parquet schema against the manifest and recomputes the upstream row fingerprint and row count. The input is re-verified after the run and again at confirmation; any change fails closed.
- Inference record identity: `record_id = "ir_" + sha256(InferenceIdentity)`. The identity binds the schema and registry versions, the inference semantics, the model reference (ID, identity SHA-256, adapter, name, and content digest), and the input identity (kind, ID, identity, manifest and output SHA-256, row fingerprint, row count, and target). It also binds the normalized bindings and the authority. Quotas and timeouts are not part of it.
- Output: `inference.parquet` has these columns, in this order:
  - `symbol`, `timeframe`, `open_time_utc`, `decision_time_utc`;
  - `row_status` (`OK`, `SKIPPED`, or `INVALID_OUTPUT`) and a nullable `reason_code`;
  - one nullable `input__<field>` float per input field, then one nullable `output__<field>` per output (float or label).

  The manifest records:
  - the output SHA-256 and row fingerprint;
  - the OK, skipped, and invalid counts with their reasons;
  - descriptive run timings, which are not part of the identity or digest;
  - the model and input lineage down to the source datasets;
  - `authority: ADVISORY_ONLY` and `READY` status.
- Materialization and replay: preview runs into a private stage, and `commit_inference` promotes it through a journal (`model_lab_promotions`) after re-verifying the input and model. Recovery on restart completes a consistent journal entry and otherwise fails closed, preserving evidence. Committing an identity that already exists never overwrites it. The run is stored in `inference_replays` as `REPRODUCED` when its output SHA-256 matches, and as `NON_DETERMINISTIC` when it does not. The record's determinism is `NOT_REPLAYED`, `REPRODUCED`, or `NON_DETERMINISTIC`.
- Record verification: every catalogue read rechecks the manifest and output digest, re-verifies the consumed feature or decision set (and through it the source datasets), and re-verifies the model registration and stored ONNX file. A change anywhere makes the record `QUARANTINED`, with a diagnostic naming the affected identity. The live Ollama listing is not consulted for records: a record keeps the digest it was computed with.
- Comparison: `compare` accepts 2–8 distinct `READY` records over the same input identity, re-verifying each output copy and its row fingerprint. Before copying or allocating anything, it refuses with `RESOURCE_QUOTA` when the selected records' total cells (rows x (outputs + 1), summed over records, overflow-checked) exceed `max_comparison_cells` (default 32 Mi; at most 8 bytes are held per cell), or their output files together exceed `max_comparison_copy_bytes` (default 2 GiB). Records are copied and loaded one at a time, and each copy is deleted once loaded. For each record it reports coverage (OK rows / rows), the invalid-output rate, and recorded latency. For each record pair and output field it reports rows where both records are OK, plus category agreement or the mean and maximum absolute numeric difference. Numeric statistics are always finite: the mean is a running mean bounded by the largest difference, and when two finite outputs differ by more than the Float64 range the comparison refuses with `COMPARISON_REFUSED`, naming the output, both records, and the row. It has no labels or prices, so it reports no accuracy against outcomes, PnL, win rate, profitability, or edge.
- Resources: one inference job runs at a time, off the UI thread. Requests are sequential, at most one in flight, each with its own timeout. Row and byte quotas are checked before and during the run and on the final file size. Cancellation is checked before every row. ONNX models are additionally bounded at load by node, parameter, tensor-size, and per-row operation limits.
- SQLite schema 8 adds immutable `model_registrations`, `inference_records` (foreign key to `model_registrations`), and `inference_replays`; a `model_lab_promotions` journal; immutable `model_lab_attempts`; and a `model_lab_settings` table that accepts only `ollama_endpoint`. The upgrade from schema 7 (or 6) takes the same verified, receipted backup as earlier upgrades.
- Inference records are research records. Nothing in this contract creates, alters, vetoes, or overrides a decision, sizes a position, or places, routes, or simulates an order.
