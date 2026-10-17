# RG-R00 acceptance

RUN_ACCEPTANCE.ps1 is non-interactive. It checks the exact clean Git commit and tree, runs Rust formatting/tests/lints/build, CI evidence contract tests, frontend install/lint/typecheck/tests/build, Tauri desktop compilation without packaging, and roadmap synchronization. Output is saved under ignored artifacts/acceptance/ and includes per-command logs. It does not read, copy, or modify the legacy research baseline.

Gate outcomes are kept separate:
- AUTOMATED_PASS: all local automated gates passed on the exact source SHA.
- ENVIRONMENT_BLOCKED: a required local tool or prerequisite prevents a gate from running.
- OWNER_UI_ACCEPTANCE_PENDING: the Owner has not reviewed the desktop UI. A compile or component fixture test does not satisfy this gate.

The runner never reports RG-R00 accepted, never unlocks RG-R01, and never promotes a champion. CI evidence is external to the committed implementation state: GitHub Actions publishes an acceptance artifact bound to the event commit and tree, with SHA-256 checksums, then signs the report with a GitHub artifact attestation on the repository's default branch. The artifact records workflow run identity and per-gate outcomes. The attestation proves report provenance and integrity, not successful gates or RG-R00 acceptance. A successful workflow does not prove the legacy host baseline was untouched or satisfy Owner UI review. No live or GUI acceptance test is started by the local script.
