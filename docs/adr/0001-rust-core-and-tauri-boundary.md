# ADR 0001: Rust research core and Tauri shell

- Status: Accepted for RG-R00
- Scope: Foundation architecture only

## Decision

The workspace separates headless Rust contracts, roadmap governance, and SQLite metadata from the Tauri desktop adapter. The React frontend communicates with Rust through explicitly registered Tauri commands. The research crates do not depend on Tauri, React, or WebView.

## Reason

This keeps experiment identity, timestamp rules, and persistence testable without a GUI, and keeps future model, strategy, indicator, and policy work behind versioned interfaces. Desktop transport can change without changing research semantics.

## Consequences

- rg-contracts, rg-governance, and rg-persistence compile as independent crates.
- The shell currently exposes one read-only governance snapshot command.
- Data Ground, Feature Studio, strategy execution, model adapters, policies, and evaluation remain planned.
- No plugins, live trading, external services, or broad desktop permissions ship in RG-R00.