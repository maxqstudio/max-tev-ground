# MAX Research Ground roadmap

Generated from ROADMAP.yaml. Current active phase: RG-R04. Later phases stay locked until the Owner accepts the current checkpoint and authorizes progression.

## RG-R00 — Rust foundation, Tauri shell, governance, CI (ACCEPTED)

Create the modular Rust research core foundation and a truthful desktop shell.

Acceptance evidence: Owner acceptance: https://github.com/maxqstudio/max-tev-ground/issues/1; source SHA a1f7ada003d57a26dcd4dfa41e86ba62a5890bed; CI https://github.com/maxqstudio/max-tev-ground/actions/runs/39037310698. Interactive UI review explicitly waived, not tested.

## RG-R01 — Data Ground (ACCEPTED)

Add historical ingestion, dataset integrity, lineage, and data quality audit.

Acceptance evidence: Owner acceptance WITH_UI_REVIEW_WAIVED: https://github.com/maxqstudio/max-tev-ground/issues/2#issuecomment-6269930193; desktop UI and CSV import review were waived, not tested; source SHA 7f50b398ca272c4c84b5e5c01866945e9eda2a2a; source tree SHA 46c8969bc31faa7f3efaa6474d5ced2322270623; exact-main CI SUCCESS https://github.com/maxqstudio/max-tev-ground/actions/runs/39285471335; acceptance artifact rg-r01-evidence-39285471335-1 digest sha256:b7d6e6be850d150d8918d5a3331eb2c126c99749e13efbce61d8a891a0015d58; known unresolved cancelPreview cancel-to-refresh UX debt is preserved.

## RG-R02 — Feature Studio (ACCEPTED)

Add a versioned indicator registry and anti-lookahead feature engine.

Acceptance evidence: Owner acceptance WITH_UI_REVIEW_WAIVED: https://github.com/maxqstudio/max-tev-ground/issues/4; manual desktop click-through NOT_RUN, UI review waived, not tested; source SHA 59e0f2e7bd5a875b661bb608d289b3b21b595bd2 (PR #6 merge); source tree SHA 326b01e72b915746be2134d00a2d0d1fa737d853, identical to accepted candidate 235bfe045abb9f3740611c3bf87114eb55973722; exact-main CI SUCCESS https://github.com/maxqstudio/max-tev-ground/actions/runs/39405958946; acceptance artifact research-ground-evidence-39405958946-1 digest sha256:f42c1ba9a1c97e9cb0c32213a365b851abdc6cd191131a1782177d01cff5006e; candidate push CI https://github.com/maxqstudio/max-tev-ground/actions/runs/39402302070 and PR CI https://github.com/maxqstudio/max-tev-ground/actions/runs/39402310436 SUCCESS with 12 of 12 automated gates; Owner-declared visual UX redesign is deferred, not accepted; RG-R01 cancelPreview cancel-to-refresh UX debt is carried forward.

## RG-R03 — Deterministic Strategy Engine (ACCEPTED)

Add deterministic native Rust strategies and isolated strategy contracts.

Acceptance evidence: Owner acceptance WITH_UI_REVIEW_WAIVED: https://github.com/maxqstudio/max-tev-ground/issues/7#issuecomment-6295197683; manual desktop click-through NOT_RUN, the Owner accepted the deliverable without manual UI review: waived, not tested; source SHA 3d3ceeb34958130f1e3f67309d0a213d10c971c3 (PR #9 merge); source tree SHA 4e8f1542a1d6a3872219c1dad9747dde5841bd88, identical to accepted candidate 277c85f90a36b96c67ff59cdff935b2b41e8175f; exact-main CI SUCCESS https://github.com/maxqstudio/max-tev-ground/actions/runs/39453334794; acceptance artifact research-ground-evidence-39453334794-1 digest sha256:d20645f018bff44982a38c6197b4bb5125cd071661058972e1a6fda7c5ac6d3b; candidate push CI https://github.com/maxqstudio/max-tev-ground/actions/runs/39446641385 and PR CI https://github.com/maxqstudio/max-tev-ground/actions/runs/39446648817 SUCCESS with 10 of 10 automated gates; synthetic fixtures only, real-market evaluation, profitability, PnL, OOS evidence and trading edge NOT_PROVEN; schema 6 to 7 upgrade of the Owner research database NOT AUTHORIZED and not run against Owner data; RG-R02 feature-output quota debt, deferred visual UX redesign and RG-R01 cancelPreview cancel-to-refresh UX debt are carried forward.

## RG-R04 — Ollama and ONNX Model Registry (ACTIVE)

Add model discovery, immutable identity, runtime adapters, and model comparison.

## RG-R05 — Policy DSL and Extensibility (PLANNED)

Add a typed policy DSL, visual policy builder, and isolated extensibility.

## RG-R06 — Experiment Orchestration (PLANNED)

Add experiment matrices, queueing, resume, checkpoint, and resource control.

## RG-R07 — Replay and Evaluation (PLANNED)

Add replay, metrics, walk-forward, CPCV, Monte Carlo, and untouched OOS.

## RG-R08 — Integrated Desktop UI and Owner Acceptance (PLANNED)

Integrate implemented modules and complete Owner acceptance.
