# MAX Research Ground

Rust-first, local-first foundation for reproducible trading research. **RG-R01 Data Ground is Owner accepted with manual desktop UI and CSV import review waived, not tested. RG-R02 Feature Studio is implemented on `work/rg-r02-feature-studio` and awaits Control Room audit; Owner UI review and acceptance are pending.** RG-R03–RG-R08 remain planned. This repository does not establish that a trading edge exists and does not execute trades.

## Build and acceptance

Requirements: Node.js 24.19.0, npm 11.17.0, Rust 1.99.1, Windows C++ Build Tools, and WebView2. Rust is installed into the project-local `.tools/` directory; no global PATH or execution-policy changes are made.

```powershell
.\scripts\Initialize-Toolchain.ps1
.\RUN_ACCEPTANCE.ps1
```

`RUN_ACCEPTANCE.ps1` is the RG-R02 acceptance runner and requires a clean commit on `work/rg-r02-feature-studio`. Besides the workspace, frontend, and Tauri checks, it runs `scripts/Invoke-PhaseGateTests.ps1`, which executes the tests named after each `PHASE_MANIFEST.yaml` gate and fails any gate with zero matching tests. GitHub CI runs the same gate step, so a gate passes only on its own tests, never on a generic suite result.

## Open the desktop app

Run `.\INSTALL_DESKTOP_SHORTCUT.ps1` once to create a Windows desktop shortcut, then double-click **MAX Research Ground**. The shortcut runs `RUN_DESKTOP.ps1`, which only reuses a build when its record matches the current clean source; otherwise it builds from the current source before launching. To start from PowerShell, run:

```powershell
.\RUN_DESKTOP.ps1
```

`-WhatIf` previews the launch path without opening the GUI. Do not use the desktop launch as a substitute for the acceptance runner.

## RG-R01 Data Ground (accepted)

The Data Ground page imports user-selected local CSV or Parquet OHLCV files. It supports mapped timestamps, explicit UTC interpretation, market roles, multiple symbols and timeframes, broker/server provenance, quota limits, missing-interval audit, and a preview that must be explicitly confirmed before registry promotion. Registered snapshots keep the original source bytes and canonical sorted Parquet with SHA-256 identities. Catalogue verification streams through the snapshots on one background worker, reports progress, supports cancellation, and exposes a dataset as `READY` only after its hashes verify. A pending promotion journal recovers confirmed imports after interruption; ambiguous files are preserved and fail closed. Invalid, duplicate, descending, or unclassified data is rejected or marked blocked, and candles are never filled synthetically.

Owner acceptance of RG-R01 is bound to source `7f50b398ca272c4c84b5e5c01866945e9eda2a2a`, tree `46c8969bc31faa7f3efaa6474d5ced2322270623`, and exact-main CI [run #39285471335](https://github.com/maxqstudio/max-tev-ground/actions/runs/39285471335). Manual desktop UI and CSV import review were **waived, not tested**. The known `cancelPreview` cancel-to-refresh messaging debt remains unresolved.

## RG-R02 Feature Studio (implemented, pending audit)

The `rg-feature-studio` crate computes versioned technical features only from `READY` Data Ground datasets. Before computing, it re-verifies each input through the dataset store, then recomputes the RG-R01 canonical row fingerprint from the exact Parquet rows it reads; after computing, it re-hashes the source. Nine native indicators (SMA, EMA, Wilder RSI, Wilder ATR, population standard deviation, Bollinger bands, ROC, log return, Donchian channel) have typed, bounded parameters, a declared warmup, and hand-calculated plus independently implemented golden-vector tests. No indicator reads volume, because RG-R01 manifests do not declare whether volume is tick or exchange volume.

Each feature row is decided at its target bar's `available_at_utc` (the canonical close). Warmup rows are null with status `WARMUP`. Reference series join as of `available_at_utc + declared lag <= decision`, so an unfinished higher-timeframe bar is never visible; references older than their declared maximum staleness are null with status `STALE`. Nothing is resampled, forward-filled, or synthesized. Computation streams bounded batches on one background worker with progress and cancellation. Output is a canonical Parquet file whose identity (`fs_<sha256>`) binds the input dataset identities, engine version, normalized parameters, and temporal semantics. Previews are staged and registered only after explicit confirmation, through a crash-recoverable promotion journal. Every catalogue read rechecks the manifest and output digest and re-verifies the current bytes of every source dataset; a feature set whose source is missing, modified, or no longer `READY` is reported `QUARANTINED` and cannot be used downstream. The input-row quota bounds the total bars of all datasets one request reads.

The Feature Studio page selects verified inputs, configures registry indicators and reference joins, shows the timing contract, previews real computed values with lineage and hashes, and confirms or cancels. Only headless component and IPC tests have run; **the desktop UI has not been manually tested.** RG-R03–RG-R08 pages remain planned.

Synthetic repository fixtures are parser and deterministic test inputs only; they are not market evidence or proof of trading performance. Historical data remains on the user's machine; the importer does not upload market data. An importer for the legacy research store is deferred. The MT5 bridge is not implemented; neither RG-R01 nor RG-R02 opens a terminal, reads MT5 `Common\Files`, or accesses account/order APIs. Feature sets are research inputs, not trading signals or evidence of edge. No live trading or automatic Champion promotion is part of this phase.

See [CONTRACTS.md](CONTRACTS.md), [SECURITY.md](SECURITY.md), [ACCEPTANCE.md](ACCEPTANCE.md), and generated [ROADMAP.md](ROADMAP.md).