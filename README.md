# MAX Research Ground

Rust-first, local-first foundation for reproducible trading research. The active scope is **RG-R00 only**. Research results, strategy edge, and Owner acceptance are not implied by this foundation.

## Build

Requirements: Node.js 24.19.0, npm 11.17.0, Windows C++ Build Tools, and WebView2. The Rust toolchain installs locally under .tools/ without changing system PATH.

    .\scripts\Initialize-Toolchain.ps1
    .\RUN_ACCEPTANCE.ps1

## Open the desktop app

Run `.\INSTALL_DESKTOP_SHORTCUT.ps1` once to create `MAX Research Ground` on the Windows Desktop, then open it with a double-click. The shortcut uses `RUN_DESKTOP.ps1`: it reuses a Tauri binary only when its acceptance record matches the current clean commit, otherwise it builds from the current source in an isolated target directory before launching. To run without a shortcut, use `.\RUN_DESKTOP.ps1` from PowerShell. `-WhatIf` previews the build and launch paths without opening the GUI.

For local iteration, run npm ci, npm run dev, and cargo test --workspace --locked. Tauri desktop shell is under src-tauri/; headless contracts and persistence are independent crates under crates/.

## RG-R00 behavior

- Overview reports the compiled governance snapshot through a Tauri command.
- The other seven navigation destinations are explicitly marked PLANNED.
- No market data, model, strategy, policy, or results are fabricated.
- SQLite stores immutable experiment metadata only. Historical candles are out of scope.
- No live trading, automatic champion promotion, or access to the legacy research baseline is implemented.

See CONTRACTS.md, SECURITY.md, ACCEPTANCE.md, and the generated ROADMAP.md.
