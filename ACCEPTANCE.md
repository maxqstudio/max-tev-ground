# RG-R01 acceptance

Run `./RUN_ACCEPTANCE.ps1` from a PowerShell session on the authorized `work/rg-r01-data-ground` branch. The runner requires a clean candidate commit rooted at `D:\MAX_RESEARCH_GROUND`, applies the pinned Rust toolchain locally, checks Node/npm pins and Windows build prerequisites, then writes per-command logs and a checksummed `acceptance.json` under `artifacts/acceptance/rg-r01-*`.

The automated gates cover roadmap synchronization, evidence-contract negative tests, Rust formatting/tests/lints/build, frontend install/lint/typecheck/tests/build, and the Tauri desktop build when MSVC is available. CI maps each RG-R01 gate to the validation suites that exercise it and uploads a checksummed report bound to the workflow event SHA and Git tree. Rust command-boundary tests exercise a real source fixture through import, confirm, catalog, and inspect; frontend interaction tests are fixture-backed component tests. Neither is reported as an interactive Owner review.

The default automated runner is non-interactive. It does not open the desktop or launch a GUI test. Owner UI acceptance remains `OWNER_UI_ACCEPTANCE_PENDING` until the Owner reviews RG-R01. The legacy research importer is deferred; the acceptance runner does not read, copy, or modify that baseline. MT5 integration is not implemented and no terminal/account/order APIs are used. Local acceptance cannot attest to activity by unrelated processes. GitHub CI reports the legacy host state as `NOT_VERIFIED_BY_GITHUB_CI`.

Statuses remain distinct: `AUTOMATED_PASS`, `ENVIRONMENT_BLOCKED`, `FAILED`, and `OWNER_UI_ACCEPTANCE_PENDING`. A successful compile alone does not satisfy acceptance. RG-R02–RG-R08 remain locked, and no live trading or automatic Champion promotion is part of this phase.
