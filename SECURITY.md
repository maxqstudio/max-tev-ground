# Security and research boundaries

- MAX Research Ground is a local research application. It does not place, route, or manage live trades.
- The prior `D:\MAX_JEV_BENCHMARK\JEV_TEV_EURUSD_H1_LAST2M` research project is outside this repository. The importer and acceptance runner do not read, copy, or modify it. No host-side baseline check is claimed in GitHub evidence.
- The Tauri shell exposes a native file picker and narrow source-inspection, preview, confirmation, cancellation, catalog, and integrity-inspection commands. The frontend receives an opaque selection token rather than an arbitrary filesystem path. Source data remains local and is copied only into the application's dataset staging area after selection.
- The store rejects symlinked storage directories and database files, verifies source and staged files, enforces byte and row quotas, and promotes a complete snapshot before registering it. Failed and cancelled imports do not create registered datasets. The application does not expose a general filesystem, process, shell, credential, or arbitrary network command.
- SQLite stores experiment and dataset metadata, fingerprints, provenance, import attempts, and immutable registry state. Database triggers prevent metadata edits/deletes; each catalog read rechecks the manifest SHA-256 and raw/Parquet snapshot hashes. Hashes detect content changes; they are not signatures against an actor able to rewrite the database and all related files.
- GitHub Actions uploads a source-SHA-bound acceptance summary and checksum; the default-branch workflow signs the report. Build and test steps do not receive attestation write permissions.
- No model endpoint, runtime secret, MT5 account bridge, or trading API is configured. The future MT5 bridge must remain read-only and separately accepted before any terminal interaction is claimed.
- Repository fixtures are labeled synthetic parser-test data. They are not historical market data, trading results, or acceptance benchmarks. No synthetic market bars are generated during import.
- Keep credentials out of manifests, logs, screenshots, and commits. Future model adapters, external connections, or trading integrations require separate threat review and scoped capabilities.
- Dependencies are exact-pinned and lockfiles are committed. Report security issues to the Owner before publishing changes.
