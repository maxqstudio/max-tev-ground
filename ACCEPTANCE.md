# RG-R01 closure and acceptance evidence

## Accepted checkpoint

RG-R01 Data Ground is Owner accepted on merged `main` at source SHA `7f50b398ca272c4c84b5e5c01866945e9eda2a2a`, Git tree `46c8969bc31faa7f3efaa6474d5ced2322270623`. Post-merge GitHub Actions [run #39285471335](https://github.com/maxqstudio/max-tev-ground/actions/runs/39285471335) completed successfully on that exact SHA. Windows foundation checks and acceptance evidence attestation both succeeded. The uploaded artifact `rg-r01-evidence-39285471335-1` has GitHub digest `sha256:b7d6e6be850d150d8918d5a3331eb2c126c99749e13efbce61d8a891a0015d58`.

Owner acceptance is recorded in [Issue #2 comment #6269930193](https://github.com/maxqstudio/max-tev-ground/issues/2#issuecomment-6269930193). Manual desktop UI and CSV import review were **WAIVED, NOT TESTED**. No interactive GUI test was run, and fixture tests are not represented as Owner UI evidence. The acceptance artifact checksum and attestation apply to the CI artifact; GitHub CI did not verify the Owner's legacy research baseline, which remains `NOT_VERIFIED_BY_GITHUB_CI`.

## Preserved UX debt

The known `cancelPreview` path can report a misleading generic error when cancellation succeeds but registry refresh fails. This remains unresolved and is carried forward as known UX debt; the closure does not claim it was fixed or manually reproduced.

## RG-R02 status

RG-R02 Feature Studio is implemented on `work/rg-r02-feature-studio`. The Control Room audit of PR #6 head `7a828b63a69f30d8c3bbf03b45505b2dfb2a1ead` returned `REPAIR_REQUIRED` for two defects, both repaired on the same branch and submitted for re-audit (`READY_FOR_CONTROL_ROOM_REAUDIT`). It is not Owner accepted.

- P1: a registered feature set stayed `READY` after its source `bars.parquet` was tampered with, because catalogue reads checked only SQLite status and stored fingerprints. Catalogue reads and the downstream output accessor now re-verify every source snapshot's current bytes and canonical hashes through Data Ground, with no SQLite lock held while hashing; a missing, modified, unverifiable, or non-`READY` source quarantines the set.
- P2: `max_input_rows` was checked per dataset. It is now the aggregate of the unique physical input datasets, checked once with overflow-checked arithmetic before materialization; `max_target_rows` is unchanged.

Each regression was run against the unrepaired head and failed, then passed after the repair. The machine-readable state records `builder_status: IMPLEMENTED` and `automated_status: AUTOMATED_PASS` for the candidate, with `owner_ui_status: PENDING`, `owner_acceptance: PENDING`, and `next_phase_unlocked: false`. The acceptance gate IDs in `PHASE_MANIFEST.yaml` are unchanged from the activation commit.

Each automated gate is evidenced by tests named after its gate ID (Rust `gate_<id>_*`; frontend `gate feature_studio_ui ...`). `scripts/Invoke-PhaseGateTests.ps1` runs each gate's tests separately, fails a gate that has no matching tests, writes per-gate logs, and writes `phase-gates.json` with test counts. `New-CiEvidence.ps1` embeds those counts and their checksum in `acceptance.json`. A generic suite pass is never inherited as gate evidence: `Test-CiEvidence.ps1` proves that without per-gate results every feature gate reports `NOT_RUN`.

All feature inputs in tests are labeled synthetic fixtures (`synthetic-fixture:` source IDs) imported through the real Data Ground importer. They are not market data or evidence of edge. No real historical dataset was available to CI, so the real-data baseline remains `NOT_VERIFIED_BY_GITHUB_CI`. The Feature Studio desktop UI was exercised only by headless component and Tauri IPC tests; manual Owner UI review is **pending, not tested**.

The RG-R01 `cancelPreview` cancel-to-refresh UX debt is unchanged in Data Ground and remains declared debt. Feature Studio's own cancel and confirm paths report the job outcome separately from any later catalogue refresh.

## Acceptance runner

`RUN_ACCEPTANCE.ps1` is scoped to the `work/rg-r02-feature-studio` candidate branch and runs non-interactively from the authorized Windows root. It checks pinned Rust/Node toolchains, Windows prerequisites, roadmap synchronization, CI evidence contracts, Rust formatting/tests/lint/build, frontend lint/typecheck/tests/build, the phase gate-traceable tests, and the Tauri build when MSVC is available. It writes per-command logs and a checksummed report bound to the exact candidate SHA/tree. It never launches the desktop or reads, copies, or modifies the legacy research baseline.

GitHub CI runs the same gate-traceable step and uploads source-bound evidence, including the per-gate logs. RG-R03–RG-R08 remain planned.
