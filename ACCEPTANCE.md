# RG-R00 acceptance

RUN_ACCEPTANCE.ps1 is non-interactive. It checks the exact clean Git commit and tree, runs Rust formatting/tests/lints/build, frontend install/lint/typecheck/tests/build, Tauri desktop compilation without packaging, and roadmap synchronization. Output is saved under ignored artifacts/acceptance/ and includes per-command logs.

Gate outcomes are kept separate:
- AUTOMATED_PASS: all local automated gates passed on the exact source SHA.
- ENVIRONMENT_BLOCKED: a required local tool or prerequisite prevents a gate from running.
- OWNER_UI_ACCEPTANCE_PENDING: the Owner has not reviewed the desktop UI. A compile or component fixture test does not satisfy this gate.

The runner never reports RG-R00 accepted, never unlocks RG-R01, and never promotes a champion. CI is NOT_CONFIGURED until the Owner supplies an official Git remote. No live or GUI acceptance test is started by the script.