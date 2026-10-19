# RG-R01 closure and acceptance evidence

## Accepted checkpoint

RG-R01 Data Ground is Owner accepted on merged `main` at source SHA `7f50b398ca272c4c84b5e5c01866945e9eda2a2a`, Git tree `46c8969bc31faa7f3efaa6474d5ced2322270623`. Post-merge GitHub Actions [run #39285471335](https://github.com/maxqstudio/max-tev-ground/actions/runs/39285471335) completed successfully on that exact SHA. Windows foundation checks and acceptance evidence attestation both succeeded. The uploaded artifact `rg-r01-evidence-39285471335-1` has GitHub digest `sha256:b7d6e6be850d150d8918d5a3331eb2c126c99749e13efbce61d8a891a0015d58`.

Owner acceptance is recorded in [Issue #2 comment #6269930193](https://github.com/maxqstudio/max-tev-ground/issues/2#issuecomment-6269930193). Manual desktop UI and CSV import review were **WAIVED, NOT TESTED**. No interactive GUI test was run, and fixture tests are not represented as Owner UI evidence. The acceptance artifact checksum and attestation apply to the CI artifact; GitHub CI did not verify the Owner's legacy research baseline, which remains `NOT_VERIFIED_BY_GITHUB_CI`.

## Preserved UX debt

The known `cancelPreview` path can report a misleading generic error when cancellation succeeds but registry refresh fails. This remains unresolved and is carried forward as known UX debt; the closure does not claim it was fixed or manually reproduced.

## RG-R02 status

RG-R02 Feature Studio is the active authorized phase after closure. Its implementation has not started in this transition. The mandatory feature gates are registered in `PHASE_MANIFEST.yaml`, and their automated acceptance status remains `NOT_RUN`. The Feature Studio page reports the handoff state and does not present a working feature engine.

## RG-R01 acceptance runner

`RUN_ACCEPTANCE.ps1` is scoped to the historical `work/rg-r01-data-ground` candidate branch and runs non-interactively from the authorized Windows root. It checks pinned Rust/Node toolchains, Windows prerequisites, roadmap synchronization, Rust formatting/tests/lint/build, frontend lint/typecheck/tests/build, and Tauri build when MSVC is available. It writes per-command logs and a checksummed report bound to its exact candidate SHA/tree. It never launches the desktop or reads, copies, or modifies the legacy research baseline.

GitHub CI now names uploaded source-bound evidence generically so future phase artifacts are not mislabeled as RG-R01. Passing CI code/build checks do not claim RG-R02 acceptance gates passed. RG-R03–RG-R08 remain planned.