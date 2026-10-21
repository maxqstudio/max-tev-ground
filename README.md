# MAX Research Ground

Rust-first, local-first foundation for reproducible trading research. **RG-R01 Data Ground, RG-R02 Feature Studio, and RG-R03 Deterministic Strategy Engine are Owner accepted with manual UI review waived, not tested. RG-R04 Ollama and ONNX Model Registry is the active scope, but its implementation has not started.** RG-R05–RG-R08 remain planned. This repository does not establish that a trading edge exists and does not execute trades.

## Build and acceptance

Requirements: Node.js 24.19.0, npm 11.17.0, Rust 1.99.1, Windows C++ Build Tools, and WebView2. Rust is installed into the project-local `.tools/` directory; no global PATH or execution-policy changes are made.

```powershell
.\scripts\Initialize-Toolchain.ps1
.\RUN_ACCEPTANCE.ps1
```

`RUN_ACCEPTANCE.ps1` is the RG-R04 acceptance runner. It requires a clean commit on `work/rg-r04-model-registry` or on the governance-only branch `work/rg-r03-closure-rg-r04-activation`. Besides the workspace, frontend, and Tauri checks, it runs `scripts/Invoke-PhaseGateTests.ps1`, which executes the tests named after each `PHASE_MANIFEST.yaml` gate and fails any gate with zero matching tests. GitHub CI runs the same gate step, so a gate passes only on its own tests, never on a generic suite result. (While a phase records `builder_status: NOT_STARTED`, the step runs no gate tests and reports every gate `NOT_RUN`.)

## Open the desktop app

Run `.\INSTALL_DESKTOP_SHORTCUT.ps1` once to create a Windows desktop shortcut, then double-click **MAX Research Ground**. The shortcut runs `RUN_DESKTOP.ps1`, which only reuses a build when its record matches the current clean source; otherwise it builds from the current source before launching. To start from PowerShell, run:

```powershell
.\RUN_DESKTOP.ps1
```

`-WhatIf` previews the launch path without opening the GUI. Do not use the desktop launch as a substitute for the acceptance runner.

### Upgrade warning: database schema 7

A build that includes RG-R03 upgrades an existing research database from schema 6 to 7 on its first start, and builds that only know schema 6 cannot open the upgraded store. Before the upgrade, the application writes a verified backup to `schema-backups\` beside the database (`%APPDATA%\com.max.research-ground\` for the desktop app). If that backup fails, the store is left at schema 6 and the application does not start. To return to a schema 6 build, close the application, then:

```powershell
.\RESTORE_DATABASE_BACKUP.ps1                       # lists the backups
.\RESTORE_DATABASE_BACKUP.ps1 -Backup <listed path>  # restores one; asks for confirmation, -WhatIf previews
```

Only backups listed as `VERIFIED` can be restored: each must still match the checksum receipt written when it was taken. The restore moves the current database files aside under a `.pre-restore-*` name; it never deletes them. If the restored copy fails its final check, the original files are put back. Anything recorded after the upgrade is only in the preserved files.

## RG-R01 Data Ground (accepted)

The Data Ground page imports user-selected local CSV or Parquet OHLCV files. It supports mapped timestamps, explicit UTC interpretation, market roles, multiple symbols and timeframes, broker/server provenance, quota limits, missing-interval audit, and a preview that must be explicitly confirmed before registry promotion. Registered snapshots keep the original source bytes and canonical sorted Parquet with SHA-256 identities. Catalogue verification streams through the snapshots on one background worker, reports progress, supports cancellation, and exposes a dataset as `READY` only after its hashes verify. A pending promotion journal recovers confirmed imports after interruption; ambiguous files are preserved and fail closed. Invalid, duplicate, descending, or unclassified data is rejected or marked blocked, and candles are never filled synthetically.

Owner acceptance of RG-R01 is bound to source `7f50b398ca272c4c84b5e5c01866945e9eda2a2a`, tree `46c8969bc31faa7f3efaa6474d5ced2322270623`, and exact-main CI [run #39285471335](https://github.com/maxqstudio/max-tev-ground/actions/runs/39285471335). Manual desktop UI and CSV import review were **waived, not tested**. The known `cancelPreview` cancel-to-refresh messaging debt remains unresolved.

## RG-R02 Feature Studio (accepted)

The `rg-feature-studio` crate computes versioned technical features only from `READY` Data Ground datasets. Before computing, it re-verifies each input through the dataset store, then recomputes the RG-R01 canonical row fingerprint from the exact Parquet rows it reads; after computing, it re-hashes the source. Nine native indicators (SMA, EMA, Wilder RSI, Wilder ATR, population standard deviation, Bollinger bands, ROC, log return, Donchian channel) have typed, bounded parameters, a declared warmup, and hand-calculated plus independently implemented golden-vector tests. No indicator reads volume, because RG-R01 manifests do not declare whether volume is tick or exchange volume.

Each feature row is decided at its target bar's `available_at_utc` (the canonical close). Warmup rows are null with status `WARMUP`. Reference series join as of `available_at_utc + declared lag <= decision`, so an unfinished higher-timeframe bar is never visible; references older than their declared maximum staleness are null with status `STALE`. Nothing is resampled, forward-filled, or synthesized. Computation streams bounded batches on one background worker with progress and cancellation. Output is a canonical Parquet file whose identity (`fs_<sha256>`) binds the input dataset identities, engine version, normalized parameters, and temporal semantics. Previews are staged and registered only after explicit confirmation, through a crash-recoverable promotion journal. Every catalogue read rechecks the manifest and output digest and re-verifies the current bytes of every source dataset; a feature set whose source is missing, modified, or no longer `READY` is reported `QUARANTINED` and cannot be used downstream. The input-row quota bounds the total bars of all datasets one request reads.

The Feature Studio page selects verified inputs, configures registry indicators and reference joins, shows the timing contract, previews real computed values with lineage and hashes, and confirms or cancels.

Owner acceptance of RG-R02 is bound to `main` merge `59e0f2e7bd5a875b661bb608d289b3b21b595bd2`, tree `326b01e72b915746be2134d00a2d0d1fa737d853` (identical to accepted candidate `235bfe045abb9f3740611c3bf87114eb55973722`), and exact-main CI [run #39405958946](https://github.com/maxqstudio/max-tev-ground/actions/runs/39405958946). The native desktop build, launch, restart, and the IPC workflow were evidenced, but manual desktop click-through was **NOT_RUN; UI review waived, not tested**. The Owner's visual redesign request is deferred UX debt, not accepted.

## RG-R03 Deterministic Strategy Engine (accepted)

The `rg-strategy-engine` crate ([Issue #7](https://github.com/maxqstudio/max-tev-ground/issues/7)) turns one registered, `READY` RG-R02 feature set into one `BUY`, `SELL`, or `SKIP` research decision per closed bar of that feature set's target series. Two native, versioned rules exist:
- `THRESHOLD_BAND` compares one target feature with strict upper and lower thresholds.
- `TREND_CONTEXT` compares fast and slow target features, filtered by a declared reference-market context feature.

Neither rule names a symbol, suffix, or timeframe. Roles must be bound to features of the required source, and context markets can only filter target decisions; they never create decision rows.

The engine evaluates a private, digest-verified copy of the feature output and recomputes the RG-R02 row fingerprint while deciding. It reads each row's own causal values only; it never recomputes a feature or reads another row. Unavailable or conflicting inputs produce `SKIP` with a reason code and the responsible role. Decisions are staged, confirmed explicitly, journaled, and promoted atomically. Every read re-verifies the decision set, its feature set, and the source datasets, so upstream tampering revokes `READY`.

The Strategy Studio page selects a feature set, a strategy, role bindings, and parameters, then previews and confirms decisions and inspects lineage.

Owner acceptance of RG-R03 is bound to `main` merge `3d3ceeb34958130f1e3f67309d0a213d10c971c3`, tree `4e8f1542a1d6a3872219c1dad9747dde5841bd88` (identical to accepted candidate `277c85f90a36b96c67ff59cdff935b2b41e8175f`), and exact-main CI [run #39453334794](https://github.com/maxqstudio/max-tev-ground/actions/runs/39453334794). Only headless component and IPC tests have run: manual desktop click-through was **NOT_RUN; UI review waived, not tested**. All tests use labeled synthetic fixtures, so real-market performance and any edge are not proven. The acceptance does **not** authorize upgrading the Owner's existing research database to schema 7 (see the upgrade warning above).

## RG-R04 Ollama and ONNX Model Registry (active, not started)

RG-R04 is authorized, but no model code exists yet. Its thirteen acceptance gates are frozen in `PHASE_MANIFEST.yaml` before implementation, and all of them remain `NOT_RUN`. The Model Lab page states that implementation has not started. RG-R04 will register local Ollama and ONNX models by immutable identity and record advisory research outputs over verified feature and decision sets. It will not download models, call remote endpoints, change strategy decisions, place orders, or claim an edge.

Synthetic repository fixtures are parser and deterministic test inputs only; they are not market evidence or proof of trading performance. Historical data remains on the user's machine; the importer does not upload market data. An importer for the legacy research store is deferred. The MT5 bridge is not implemented. No phase opens a terminal, reads MT5 `Common\Files`, or accesses account or order APIs. Feature sets and decision sets are research records, not trading signals, orders, backtests, or evidence of edge. No live trading or automatic Champion promotion is part of any phase so far.

See [CONTRACTS.md](CONTRACTS.md), [SECURITY.md](SECURITY.md), [ACCEPTANCE.md](ACCEPTANCE.md), and generated [ROADMAP.md](ROADMAP.md).