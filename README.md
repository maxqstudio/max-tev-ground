# MAX Research Ground

Rust-first, local-first foundation for reproducible trading research. **RG-R01 Data Ground is Owner accepted with manual desktop UI and CSV import review waived, not tested. RG-R02 Feature Studio is the active implementation scope, but implementation has not started in this transition.** RG-R03–RG-R08 remain planned. This repository does not establish that a trading edge exists and does not execute trades.

## Build and acceptance

Requirements: Node.js 24.19.0, npm 11.17.0, Rust 1.99.1, Windows C++ Build Tools, and WebView2. Rust is installed into the project-local `.tools/` directory; no global PATH or execution-policy changes are made.

```powershell
.\scripts\Initialize-Toolchain.ps1
.\RUN_ACCEPTANCE.ps1
```

`RUN_ACCEPTANCE.ps1` is the RG-R01 acceptance runner and requires a clean commit on `work/rg-r01-data-ground`. It is retained as historical phase evidence tooling. Current RG-R02 automated acceptance remains `NOT_RUN` until its implementation and phase-specific runner are delivered. GitHub CI checks current governance, code quality, build, and desktop shell without claiming the R02 feature gates passed.

## Open the desktop app

Run `.\INSTALL_DESKTOP_SHORTCUT.ps1` once to create a Windows desktop shortcut, then double-click **MAX Research Ground**. The shortcut runs `RUN_DESKTOP.ps1`, which only reuses a build when its record matches the current clean source; otherwise it builds from the current source before launching. To start from PowerShell, run:

```powershell
.\RUN_DESKTOP.ps1
```

`-WhatIf` previews the launch path without opening the GUI. Do not use the desktop launch as a substitute for the acceptance runner.

## RG-R01 Data Ground (accepted)

The Data Ground page imports user-selected local CSV or Parquet OHLCV files. It supports mapped timestamps, explicit UTC interpretation, market roles, multiple symbols and timeframes, broker/server provenance, quota limits, missing-interval audit, and a preview that must be explicitly confirmed before registry promotion. Registered snapshots keep the original source bytes and canonical sorted Parquet with SHA-256 identities. Catalogue verification streams through the snapshots on one background worker, reports progress, supports cancellation, and exposes a dataset as `READY` only after its hashes verify. A pending promotion journal recovers confirmed imports after interruption; ambiguous files are preserved and fail closed. Invalid, duplicate, descending, or unclassified data is rejected or marked blocked, and candles are never filled synthetically.

Owner acceptance of RG-R01 is bound to source `7f50b398ca272c4c84b5e5c01866945e9eda2a2a`, tree `46c8969bc31faa7f3efaa6474d5ced2322270623`, and exact-main CI [run #39285471335](https://github.com/maxqstudio/max-tev-ground/actions/runs/39285471335). Manual desktop UI and CSV import review were **waived, not tested**. The known `cancelPreview` cancel-to-refresh messaging debt remains unresolved.

## RG-R02 Feature Studio (active, not started)

RG-R02 is authorized for implementation handoff. The Feature Studio page shows its active status while clearly stating that the feature workflow is not implemented yet. Its mandatory data authority, indicator math, causal timing, multi-timeframe, immutable materialization, resource, UI, and reproducible-evidence gates are frozen in `PHASE_MANIFEST.yaml`. Their automated status remains `NOT_RUN` until each gate has real source-backed coverage.

Synthetic repository fixtures are parser and deterministic test inputs only; they are not market evidence or proof of trading performance. Historical data remains on the user's machine; the importer does not upload market data. An importer for the legacy research store is deferred. The MT5 bridge is not implemented; RG-R01 does not open a terminal or access account/order APIs. No live trading or automatic Champion promotion is part of this phase.

See [CONTRACTS.md](CONTRACTS.md), [SECURITY.md](SECURITY.md), [ACCEPTANCE.md](ACCEPTANCE.md), and generated [ROADMAP.md](ROADMAP.md).