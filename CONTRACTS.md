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