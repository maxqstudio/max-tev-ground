# Phase closure and acceptance evidence

## RG-R01 accepted checkpoint

RG-R01 Data Ground is Owner accepted on merged `main` at source SHA `7f50b398ca272c4c84b5e5c01866945e9eda2a2a`, Git tree `46c8969bc31faa7f3efaa6474d5ced2322270623`. Post-merge GitHub Actions [run #39285471335](https://github.com/maxqstudio/max-tev-ground/actions/runs/39285471335) completed successfully on that exact SHA. Windows foundation checks and acceptance evidence attestation both succeeded. The uploaded artifact `rg-r01-evidence-39285471335-1` has GitHub digest `sha256:b7d6e6be850d150d8918d5a3331eb2c126c99749e13efbce61d8a891a0015d58`.

Owner acceptance is recorded in [Issue #2 comment #6269930193](https://github.com/maxqstudio/max-tev-ground/issues/2#issuecomment-6269930193). Manual desktop UI and CSV import review were **WAIVED, NOT TESTED**. No interactive GUI test was run, and fixture tests are not represented as Owner UI evidence. The acceptance artifact checksum and attestation apply to the CI artifact; GitHub CI did not verify the Owner's legacy research baseline, which remains `NOT_VERIFIED_BY_GITHUB_CI`.

## Preserved UX debt

The known `cancelPreview` path can report a misleading generic error when cancellation succeeds but registry refresh fails. This remains unresolved and is carried forward as known UX debt; the closure does not claim it was fixed or manually reproduced.

## RG-R02 implementation and repair history

RG-R02 Feature Studio was implemented on `work/rg-r02-feature-studio`. The Control Room audit of PR #6 head `7a828b63a69f30d8c3bbf03b45505b2dfb2a1ead` returned `REPAIR_REQUIRED` for two defects. Both were repaired on the same branch in `0e76f26a5cc06788f8aa72a0ce996ae07be28590`, and the Control Room source re-audit verified the repairs. Owner acceptance is recorded under [RG-R02 accepted checkpoint](#rg-r02-accepted-checkpoint).

- P1: a registered feature set stayed `READY` after its source `bars.parquet` was tampered with, because catalogue reads checked only SQLite status and stored fingerprints. Catalogue reads and the downstream output accessor now re-verify every source snapshot's current bytes and canonical hashes through Data Ground, with no SQLite lock held while hashing; a missing, modified, unverifiable, or non-`READY` source quarantines the set.
- P2: `max_input_rows` was checked per dataset. It is now the aggregate of the unique physical input datasets, checked once with overflow-checked arithmetic before materialization; `max_target_rows` is unchanged.

Each regression was run against the unrepaired head and failed, then passed after the repair. The machine-readable state records `builder_status: IMPLEMENTED` and `automated_status: AUTOMATED_PASS` for the candidate, with `owner_ui_status: PENDING`, `owner_acceptance: PENDING`, and `next_phase_unlocked: false`. The acceptance gate IDs in `PHASE_MANIFEST.yaml` are unchanged from the activation commit.

Each automated gate is evidenced by tests named after its gate ID (Rust `gate_<id>_*`; frontend `gate feature_studio_ui ...`). `scripts/Invoke-PhaseGateTests.ps1` runs each gate's tests separately, fails a gate that has no matching tests, writes per-gate logs, and writes `phase-gates.json` with test counts. `New-CiEvidence.ps1` embeds those counts and their checksum in `acceptance.json`. A generic suite pass is never inherited as gate evidence: `Test-CiEvidence.ps1` proves that without per-gate results every feature gate reports `NOT_RUN`.

All feature inputs in tests are labeled synthetic fixtures (`synthetic-fixture:` source IDs) imported through the real Data Ground importer. They are not market data or evidence of edge. No real historical dataset was available to CI, so the real-data baseline remains `NOT_VERIFIED_BY_GITHUB_CI`. The Feature Studio desktop UI was exercised only by headless component and Tauri IPC tests; manual Owner UI review is **pending, not tested**.

The RG-R01 `cancelPreview` cancel-to-refresh UX debt is unchanged in Data Ground and remains declared debt. Feature Studio's own cancel and confirm paths report the job outcome separately from any later catalogue refresh.

## Desktop functional readiness (Owner direction, PR #6 comment #6287687918)

The Control Room re-audit verified the P1/P2 repairs and the exact-SHA CI. The Owner then set the next gate as `DESKTOP_FUNCTIONAL_READY_FOR_OWNER_TEST`: a desktop app that builds, launches and completes the basic workflow. Visual design acceptance is not part of this gate.

- Launcher: `RUN_DESKTOP.ps1` printed informational native stderr, such as Tauri's `Info ... workspace inheritance` lines, inside a PowerShell `NativeCommandError` banner. A successful build therefore looked fatal. The build itself had exited 0. Native stderr is now shown and logged as its own text, and success is still decided only by the exit code. A nonzero exit still stops the launcher with its diagnostics. The MSVC setup no longer prints a misleading `'vswhere.exe' is not recognized` line. A launch counts as successful only once the main window appears; an exit during start-up, or no window within 30 seconds, is reported as a failure. These were verified under Windows PowerShell 5.1. PowerShell 7 is not installed on the builder machine and was not run.
- Feature Studio ordinary-operation blockers were fixed without changing the layout. When no dataset was eligible, the page did not say why each registered dataset was unavailable (for example `BLOCKED` by gap audit); it now does. After Refresh found a newly `READY` dataset, the target selector appeared to show it while no target was actually selected, so preview failed; Refresh now selects the first eligible dataset and series.
- The workflow smoke `gate_feature_studio_ui_ipc_end_to_end_desktop_workflow_survives_restart` drives the IPC commands the desktop UI calls. It runs on a labeled synthetic fixture in isolated storage: import, confirm `READY`, choose the input, preview, freeze, list and inspect, restart, reuse, cancel, check that source bytes are unchanged, and block a tampered source.
- The native executable was built and launched through the launcher, rendered its WebView window, stayed responsive, closed gracefully with exit code 0, and relaunched from the verified cached build. Clicking through the GUI was not automated, because the builder contract forbids desktop input automation. The Owner's hands-on test remains pending.

**Deferred UX debt (not accepted, not waived):** the Owner finds the current UI confusing and visually unsatisfactory. The visual redesign is deferred. Nothing in this section records `OWNER_UI_PASS` or Owner acceptance.

## RG-R02 accepted checkpoint

The Owner accepted RG-R02 Feature Studio on 2026-10-20 ("iya pass lanjut aja"), as recorded in [Issue #4](https://github.com/maxqstudio/max-tev-ground/issues/4). This is recorded as `OWNER_ACCEPTED_WITH_UI_REVIEW_WAIVED`. Manual desktop click-through was **NOT_RUN**, and the Owner waived UI review: **waived, not tested**. It is not a manual interactive GUI pass.

- Accepted candidate: `235bfe045abb9f3740611c3bf87114eb55973722`, tree `326b01e72b915746be2134d00a2d0d1fa737d853`. Push CI [#39402302070](https://github.com/maxqstudio/max-tev-ground/actions/runs/39402302070) and PR CI [#39402310436](https://github.com/maxqstudio/max-tev-ground/actions/runs/39402310436) both succeeded, with 12 of 12 automated gates passing. Control Room reviews: [source](https://github.com/maxqstudio/max-tev-ground/pull/6#pullrequestreview-5583046308) and [desktop](https://github.com/maxqstudio/max-tev-ground/pull/6#pullrequestreview-5583614520).
- Merge: PR #6 was merged on Owner authorization into `main` at `59e0f2e7bd5a875b661bb608d289b3b21b595bd2`. Its tree, `326b01e72b915746be2134d00a2d0d1fa737d853`, is identical to the accepted candidate's.
- Exact-main CI: [run #39405958946](https://github.com/maxqstudio/max-tev-ground/actions/runs/39405958946) completed successfully on that SHA. Windows foundation checks and acceptance evidence attestation both succeeded. The uploaded artifact `research-ground-evidence-39405958946-1` has GitHub digest `sha256:f42c1ba9a1c97e9cb0c32213a365b851abdc6cd191131a1782177d01cff5006e`.
- Known debts carried forward: the deferred visual redesign; the unverified interactive desktop use (click-through NOT_RUN); and the RG-R01 `cancelPreview` cancel-to-refresh debt. Historical market data and trading edge remain NOT VERIFIED; GitHub CI did not verify the Owner's legacy baseline (`NOT_VERIFIED_BY_GITHUB_CI`).

## RG-R03 activation (governance only)

RG-R03 Deterministic Strategy Engine was activated by governance PR #8 (merged at `fb019b92ea4bc2f02834a13355fe4f692d057f5b`, tree `a14d86bb22c3dd0499805a9c19fed08d804cc37f`, exact-main CI [#39417161028](https://github.com/maxqstudio/max-tev-ground/actions/runs/39417161028) SUCCESS) with no strategy code. Its scope is defined in [Issue #7](https://github.com/maxqstudio/max-tev-ground/issues/7).

The twelve RG-R03 acceptance gates were frozen in `PHASE_MANIFEST.yaml` before implementation, and `rg-governance` rejects any change to that set: `strategy_input_authority`, `typed_strategy_registry`, `decision_semantics`, `causal_no_lookahead`, `mtf_context_only`, `skip_and_conflict_handling`, `immutable_decision_materialization`, `resource_cancel_recovery`, `desktop_strategy_ipc`, `adversarial_tamper`, `reproducible_acceptance`, and `owner_ui_acceptance`. The implementation did not change them.

## RG-R03 implementation and repair history

RG-R03 was implemented on `work/rg-r03-strategy-engine`, branched from the validated `main` above, and submitted for Control Room audit as PR #9. Owner acceptance is recorded under [RG-R03 accepted checkpoint](#rg-r03-accepted-checkpoint). The contract is in [CONTRACTS.md](CONTRACTS.md#decision-set-contracts-rg-r03-schema-1).

- Each automated gate is evidenced by tests named after it, run per gate by `scripts/Invoke-PhaseGateTests.ps1`. The tests cover:
  - the Rust engine, registry, store and persistence;
  - end-to-end tests that import labeled synthetic bars through Data Ground, build RG-R02 feature sets, and evaluate them;
  - Tauri IPC tests;
  - frontend component tests for `desktop_strategy_ipc`.
- Decision expectations are checked against an independent rule implementation inside the tests, and against hand-calculated golden cases.
- `causal_no_lookahead`: perturbing future target bars, or context bars that are not yet available, leaves every earlier decision row byte-identical.
- A mutation check was run before submission. Disabling lineage re-verification, the confirm-time feature-set re-check, or the row-fingerprint comparison each made its gate test fail.
- The Control Room audit of PR #9 head `86f4aaff03ef6a4237440487d454e9e603dcec48` returned `REPAIR_REQUIRED` ([review #5585758291](https://github.com/maxqstudio/max-tev-ground/pull/9#pullrequestreview-5585758291)). All three defects are repaired on the same branch. Each regression was run against `86f4aaf` and failed, then passed after the repair; the logs are in `artifacts/acceptance/pr9-repair-pre-repair-regressions/`.
  - P1: starting the application upgraded the Owner database from schema 6 to 7 with no backup, and schema 6 builds then refused it. Every open now takes a verified `VACUUM INTO` backup, including WAL content, before any upgrade. It fails closed without changing the version, and a restore path is tested from the library, the `rg-schema-backup` binary, and `RESTORE_DATABASE_BACKUP.ps1`. Tests cover a populated schema 6 store with WAL content, backup failure, migration failure, restore for a schema 6 build with an unchanged content digest and lineage, and refused unsafe restores.
  - P2: the output-byte quota ignored the Parquet footer. The final file length is now checked after close.
  - P2: `TREND_CONTEXT` could turn finite inputs into an infinite difference and a direction. That is now `SKIP` with `NUMERIC_OVERFLOW`, and the golden cases are unchanged.
- The Control Room re-audit at `cf8d8a1eab2f7e0fc6e3872d890ffd516010fcff` ([review #5586865314](https://github.com/maxqstudio/max-tev-ground/pull/9#pullrequestreview-5586865314)) verified those three repairs and found one more P2. A backup altered with ordinary SQL, and still structurally valid SQLite, could be restored, because the expected digest was recomputed from the backup itself.
  - Repair: backups are now anchored by a checksum receipt written at backup time, and verification and restore compare against it. A final restore check that fails puts the original files back.
  - The regression was run against `cf8d8a1` and failed, then passed; the log is in `artifacts/acceptance/pr9-reaudit-pre-repair-regression/`.
  - Further tests cover missing, malformed, mismatched, and foreign receipts, the receipt-verified restore, and rollback after a failed final check.
  - The receipt is a checksum, not a signature against someone able to rewrite both the backup and its receipt.
- RG-R02 maintenance debt (not part of RG-R03): the Feature Studio feature-output quota is checked before the Parquet footer is written, as the RG-R03 decision output was before its repair. It is recorded for an Owner-approved targeted repair and is unchanged here.
- The new desktop executable was still not run against the Owner's real database. Owner and Control Room decide when to take the first verified upgrade.
- No real historical dataset was used; the Owner's market baseline remains `NOT_VERIFIED_BY_GITHUB_CI`. The Strategy Studio desktop page was exercised only by headless component and IPC tests, and manual Owner UI review is **pending, not tested**. The RG-R01 and RG-R02 UX debts are unchanged.

## RG-R03 accepted checkpoint

After the [Control Room re-audit #5587712394](https://github.com/maxqstudio/max-tev-ground/pull/9#pullrequestreview-5587712394), the Owner approved RG-R03 Deterministic Strategy Engine on 2026-10-20, as recorded in [Issue #7 comment #6295197683](https://github.com/maxqstudio/max-tev-ground/issues/7#issuecomment-6295197683). This is recorded as `OWNER_ACCEPTED_WITH_UI_REVIEW_WAIVED`: the Owner accepted the deliverable without requiring manual UI review. Manual desktop click-through was **NOT_RUN**: **waived, not tested**. It is not a manual interactive GUI pass, and visual or interactive quality is not assured by it.

- Accepted candidate: PR #9 head `277c85f90a36b96c67ff59cdff935b2b41e8175f`, tree `4e8f1542a1d6a3872219c1dad9747dde5841bd88`. Push CI [#39446641385](https://github.com/maxqstudio/max-tev-ground/actions/runs/39446641385), including attestation, and PR CI [#39446648817](https://github.com/maxqstudio/max-tev-ground/actions/runs/39446648817) both succeeded, with 10 of 10 automated implementation gates passing (`AUTOMATED_PASS`).
- Merge: PR #9 was merged on Owner authorization into `main` at `3d3ceeb34958130f1e3f67309d0a213d10c971c3`, with parents `fb019b92ea4bc2f02834a13355fe4f692d057f5b` (accepted `main`) and the audited candidate. Its tree, `4e8f1542a1d6a3872219c1dad9747dde5841bd88`, is identical to the accepted candidate's.
- Exact-main CI: [run #39453334794](https://github.com/maxqstudio/max-tev-ground/actions/runs/39453334794) completed successfully on that SHA. Windows foundation checks and acceptance evidence attestation both succeeded. The uploaded artifact `research-ground-evidence-39453334794-1` has GitHub digest `sha256:d20645f018bff44982a38c6197b4bb5125cd071661058972e1a6fda7c5ac6d3b`.
- Scope of the evidence: evaluation used labeled synthetic fixtures only. Real-market evaluation, profitability, PnL, OOS evidence, and any trading edge are **NOT_PROVEN**. Research decisions are not orders, and generating them does not demonstrate an edge.
- **Owner database upgrade not authorized.** An RG-R03 build upgrades an existing research database from schema 6 to 7 on first start. The backup, receipt, restore, and rollback paths are source-tested, but no Owner-profile runtime test exists. This acceptance does not authorize running the new executable against the Owner's `research.sqlite3` or touching the Owner machine.
- Known debts carried forward: the Strategy Studio click-through (NOT_RUN); the unauthorized Owner database upgrade; the RG-R02 feature-output quota checked before the Parquet footer ([Issue #4](https://github.com/maxqstudio/max-tev-ground/issues/4#issuecomment-6294996993)); the deferred RG-R02 visual redesign; and the RG-R01 `cancelPreview` cancel-to-refresh debt. None of them is waived by this closure.

## RG-R04 activation (governance only)

RG-R04 Ollama and ONNX Model Registry was activated by a governance-only change, as instructed in [Issue #7 comment #6295197683](https://github.com/maxqstudio/max-tev-ground/issues/7#issuecomment-6295197683). The Owner approved it after Control Room audit, and it merged as PR #10 at `129ac14ed79336bb20288d2c51cd0af2d4fd9d54`. At activation no model code existed: `builder_status` was `NOT_STARTED` and `automated_status` was `NOT_RUN`.

The thirteen RG-R04 acceptance gates were frozen in `PHASE_MANIFEST.yaml` before implementation, and `rg-governance` rejects any change to that set: `local_runtime_boundary`, `model_discovery`, `immutable_model_identity`, `typed_runtime_adapters`, `verified_model_input`, `model_authority_boundary`, `inference_materialization`, `model_comparison`, `resource_cancel_recovery`, `desktop_model_lab_ipc`, `adversarial_tamper`, `reproducible_acceptance`, and `owner_ui_acceptance`. A gate passes only on its own gate-traceable tests; generic suite success is never inherited.

The frozen scope keeps RG-R04 a local research registry:
- Runtimes are reached only through an Owner-configured loopback Ollama endpoint or a user-selected local ONNX file. The application never pulls or downloads a model, and it never calls a remote endpoint.
- Model outputs are research records. RG-R04 runs only `ADVISORY_ONLY` outputs; it never changes an RG-R03 decision, sizes a position, or emits an order. Applying other authorities belongs to the RG-R05 policy scope.
- Model comparison reports descriptive agreement, coverage, error, and latency only, never PnL, win rate, or edge.
- CI uses mock runtimes and labeled fixture models; no installed runtime or network is assumed.

## RG-R04 implementation status

The Builder implemented RG-R04 on `work/rg-r04-model-registry`, branched from the validated `main` merge `129ac14ed79336bb20288d2c51cd0af2d4fd9d54` (governance PR #10) after post-merge CI [#39527456255](https://github.com/maxqstudio/max-tev-ground/actions/runs/39527456255) succeeded, as the [PR #10 Builder handoff](https://github.com/maxqstudio/max-tev-ground/pull/10#issuecomment-6305739611) instructed. `builder_status` is `IMPLEMENTED`. The automated status this candidate claims is verified by the acceptance runner and CI on its exact commit. Owner UI status and Owner acceptance are `PENDING`, and RG-R05–RG-R08 stay locked. This submission is for a separate Control Room audit; the Builder does not merge it.

What exists:
- `rg-model-registry`, with:
  - a loopback-only HTTP client limited to three permitted calls;
  - the Ollama adapter and the native ONNX decoder and interpreter;
  - typed registration, inference, and record contracts;
  - the model and inference-record store, with staging, a journal, recovery, quarantine, and replay recording;
  - descriptive comparison.
- Schema 8 in `rg-persistence`: immutable model registrations, inference records, and replays; a promotion journal; immutable attempts; and the endpoint setting. The upgrade takes the existing verified backup.
- Model Lab Tauri commands and the Model Lab page. These replace the "implementation has not started" placeholder and use the existing page layout and styles; there is no visual redesign.
- `SECURITY.md` records the adapter threat review, and `CONTRACTS.md` the RG-R04 contracts.

Design decisions for the Control Room to review:
- **Native ONNX interpreter instead of an external runtime.** `ort` would bring native ONNX Runtime binaries. `tract-onnx` 0.22 resolves to about 160 crates, including native C builds; it reads external tensor data from paths named inside a model, and it does not bound allocations up front. The Builder instead decodes ONNX with a strict, bounded protobuf reader and evaluates a documented operator subset sequentially in FLOAT32. This keeps model files from causing file reads and makes resource limits checkable before a run. Replays on the same build are byte-identical. The cost is coverage: models using other operators, or tree-ensemble (`ai.onnx.ml`) models, are refused with a diagnostic rather than run.
- **Minimal HTTP client instead of an HTTP library.** It accepts only literal loopback IPs, never follows redirects, and bounds every response, with no proxy, DNS, or TLS code path.
- **Records keep their model digest.** A record's lineage checks its stored model registration and ONNX file, not the live Ollama listing. Runtime drift quarantines the model and blocks new inference, but it does not revoke records that were computed with the registered digest.
- **Prompts contain no dates or symbols.** This keeps a language model from recalling what followed a date in its training data.

Evidence and limits:
- Rust gate tests cover the 11 technical gates:
  - `rg-model-registry`: 18 unit tests and 29 integration tests.
  - `rg-persistence`: 5 schema-8 tests.
  - Tauri: 5 Model Lab IPC tests.
- Vitest: 8 `gate desktop_model_lab_ipc` component tests.
- Every test uses labeled synthetic fixture models and a loopback mock Ollama runtime. No real model runtime, model download, or network is used. Fixture outputs say nothing about any real model's quality, and real-market evaluation, profitability, PnL, OOS evidence, and trading edge remain **NOT_PROVEN**.
- Manual desktop click-through of Model Lab is **NOT_RUN**; Owner UI review is pending and is not inferred from component tests.
- Running this build against an existing research database upgrades it to schema 8 (from 7 or 6). Upgrading the Owner's `research.sqlite3` is **NOT AUTHORIZED**; this build has not been run against Owner data or on the Owner machine.
- Carried debts are unchanged:
  - the RG-R02 feature-output quota check before the Parquet footer;
  - the RG-R01 `cancelPreview` cancel-to-refresh messaging;
  - the deferred RG-R02 visual redesign;
  - manual GUI review NOT_TESTED for accepted phases.

## Acceptance runner

`RUN_ACCEPTANCE.ps1` is scoped to the RG-R04 candidate branch `work/rg-r04-model-registry` and to the governance-only activation branch `work/rg-r03-closure-rg-r04-activation`. It runs non-interactively from the authorized Windows root. It checks pinned Rust/Node toolchains, Windows prerequisites, roadmap synchronization, CI evidence contracts, Rust formatting/tests/lint/build, frontend lint/typecheck/tests/build, the phase gate-traceable tests, and the Tauri build when MSVC is available. It writes per-command logs and a checksummed report bound to the exact candidate SHA/tree. It never launches the desktop or reads, copies, or modifies the legacy research baseline.

GitHub CI runs the same gate-traceable step and uploads source-bound evidence, including the per-gate logs. Each RG-R04 technical gate passes only on its own `gate_<id>` Rust tests, plus `gate desktop_model_lab_ipc` Vitest tests for the desktop gate; a gate with no matching tests fails. `reproducible_acceptance` comes from the source and governance envelope, and `owner_ui_acceptance` stays `OWNER_UI_ACCEPTANCE_PENDING` until the Owner records a decision. RG-R05–RG-R08 remain planned.
