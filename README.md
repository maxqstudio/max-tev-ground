# MAX Research Ground

Rust-first, local-first foundation for reproducible trading research. The active scope is **RG-R00 only**. Research results, strategy edge, and Owner acceptance are not implied by this foundation.

## Build

Requirements: Node.js 24.19.0, npm 11.17.0, Windows C++ Build Tools, and WebView2. The Rust toolchain installs locally under .tools/ without changing system PATH.

    .\scripts\Initialize-Toolchain.ps1
    .\RUN_ACCEPTANCE.ps1

For local iteration, run npm ci, npm run dev, and cargo test --workspace --locked. Tauri desktop shell is under src-tauri/; headless contracts and persistence are independent crates under crates/.

## RG-R00 behavior

- Overview reports the compiled governance snapshot through a Tauri command.
- The other seven navigation destinations are explicitly marked PLANNED.
- No market data, model, strategy, policy, or results are fabricated.
- SQLite stores immutable experiment metadata only. Historical candles are out of scope.
- No live trading, automatic champion promotion, or access to the legacy research baseline is implemented.

See CONTRACTS.md, SECURITY.md, ACCEPTANCE.md, and the generated ROADMAP.md.