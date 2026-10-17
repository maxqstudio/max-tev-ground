# MAX Research Ground

Rust-first, local-first foundation for reproducible trading research. RG-R00 is Owner accepted; **RG-R01 Data Ground is the only active phase**. RG-R02–RG-R08 remain locked. This repository does not establish that a trading edge exists and does not execute trades.

## Build and acceptance

Requirements: Node.js 24.19.0, npm 11.17.0, Rust 1.99.1, Windows C++ Build Tools, and WebView2. Rust is installed into the project-local `.tools/` directory; no global PATH or execution-policy changes are made.

```powershell
.\scripts\Initialize-Toolchain.ps1
.\RUN_ACCEPTANCE.ps1
```

The acceptance runner requires a clean commit on `work/rg-r01-data-ground`, runs headless checks, and writes per-gate logs plus a checksummed report under `artifacts/acceptance/`. It never launches or automates the desktop UI. Owner interactive review of RG-R01 remains pending.

## Open the desktop app

Run `.\INSTALL_DESKTOP_SHORTCUT.ps1` once to create a Windows desktop shortcut, then double-click **MAX Research Ground**. The shortcut runs `RUN_DESKTOP.ps1`, which only reuses a build when its record matches the current clean source; otherwise it builds from the current source before launching. To start from PowerShell, run:

```powershell
.\RUN_DESKTOP.ps1
```

`-WhatIf` previews the launch path without opening the GUI. Do not use the desktop launch as a substitute for the acceptance runner.

## RG-R01 Data Ground

The Data Ground page imports user-selected local CSV or Parquet OHLCV files. It supports mapped timestamps, explicit UTC interpretation, market roles, multiple symbols and timeframes, broker/server provenance, quota limits, missing-interval audit, and a preview that must be explicitly confirmed before registry promotion. Registered snapshots keep the original source bytes and canonical sorted Parquet with SHA-256 identities. Catalogue verification streams through the snapshots on one background worker, reports progress, supports cancellation, and exposes a dataset as `READY` only after its hashes verify. A pending promotion journal recovers confirmed imports after interruption; ambiguous files are preserved and fail closed. Invalid, duplicate, descending, or unclassified data is rejected or marked blocked, and candles are never filled synthetically.

The UI shows imported datasets only after a real local import. Repository fixtures are synthetic parser tests and are not market data or evidence of trading performance. Historical data remains on the user's machine; no market data is uploaded by the importer.

An importer for the legacy research store is deferred. The old research repository is not accessed by the acceptance runner. The MT5 bridge is not implemented; RG-R01 does not open a terminal or access account/order APIs. Other roadmap pages remain explicitly planned or locked and do not present inactive controls as functional.

See [CONTRACTS.md](CONTRACTS.md), [SECURITY.md](SECURITY.md), [ACCEPTANCE.md](ACCEPTANCE.md), and generated [ROADMAP.md](ROADMAP.md).
