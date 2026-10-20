//! End-to-end gate tests: labeled synthetic bars are imported through Data Ground, become RG-R02
//! feature sets, and are evaluated by the RG-R03 strategy engine through its public store API.

mod support;

use std::sync::atomic::AtomicBool;

use rg_data_ground::Timeframe;
use rg_persistence::{open_database, save_pending_decision_set_promotion, DecisionAttemptRecord};
use rg_strategy_engine::{
    store::{DecisionCommitDisposition, PreparedDecisionSet},
    DecisionProgress, DecisionSetStatus, StrategyError,
};
use support::*;

const START: &str = "2024-01-01T00:00:00Z";
const TARGET: &str = "EURUSD.m";
const CONTEXT: &str = "GBPUSD.m";

/// Target EURUSD.m H1 (48 bars) and context GBPUSD.m H4 (12 bars) in one dataset. Returns the
/// dataset ID and the bars, so tests can perturb and re-import them.
fn market_bars(target_phase: f64, context_phase: f64) -> Vec<Bar> {
    let mut rows = bars(TARGET, Timeframe::H1, START, 48, target_phase);
    rows.extend(bars(CONTEXT, Timeframe::H4, START, 12, context_phase));
    rows
}

/// Registers a feature set with fast/slow SMAs and ROC on the target, plus RSI context from the
/// H4 reference with a generous (`ctx`) and a one-hour (`ctx_tight`) staleness limit.
fn trend_feature_set(fixture: &Fixture, name: &str, rows: &[Bar]) -> String {
    let dataset = fixture.import(name, rows);
    let reference = selector(&dataset, CONTEXT, "H4");
    fixture.feature_set(&feature_request(
        selector(&dataset, TARGET, "H1"),
        vec![
            target_feature("fast", "SMA", 3),
            target_feature("slow", "SMA", 8),
            target_feature("roc", "ROC", 2),
            reference_feature("ctx", "RSI_WILDER", 3, reference.clone(), 8 * 3600),
            reference_feature("ctx_tight", "RSI_WILDER", 3, reference, 3600),
        ],
    ))
}

fn trend_request(
    feature_set_id: &str,
    context_key: &str,
) -> rg_strategy_engine::DecisionSetRequest {
    decision_request(
        feature_set_id,
        "TREND_CONTEXT",
        &[
            ("margin", 0.0),
            ("context_min", 0.0),
            ("context_max", 100.0),
        ],
        &[("fast", "fast"), ("slow", "slow"), ("context", context_key)],
    )
}

fn band_request(feature_set_id: &str) -> rg_strategy_engine::DecisionSetRequest {
    decision_request(
        feature_set_id,
        "THRESHOLD_BAND",
        &[("upper", 0.3), ("lower", -0.3)],
        &[("signal", "roc")],
    )
}

fn decisions(fixture: &Fixture, decision_set_id: &str) -> Table {
    read_table(
        &fixture
            .decision_dir(decision_set_id)
            .join("decisions.parquet"),
    )
}

#[test]
fn gate_strategy_input_authority_accepts_only_ready_verified_feature_sets() {
    let fixture = Fixture::new();
    let rows = market_bars(0.0, 1.0);
    let ready = trend_feature_set(&fixture, "authority", &rows);
    let dataset = fixture.import(
        "authority-short",
        &bars("AUDUSD.m", Timeframe::H1, START, 20, 0.5),
    );
    let blocked = fixture.feature_set(&feature_request(
        selector(&dataset, "AUDUSD.m", "H1"),
        vec![target_feature("roc", "ROC", 100)],
    ));

    let candidates = fixture
        .store()
        .list_input_candidates(&AtomicBool::new(false))
        .unwrap();
    let candidate = |id: &str| candidates.iter().find(|c| c.feature_set_id == id).unwrap();
    assert!(candidate(&ready).eligible);
    assert_eq!(candidate(&ready).target.as_ref().unwrap().symbol, TARGET);
    assert!(!candidate(&blocked).eligible);
    assert!(candidate(&blocked)
        .reason
        .as_deref()
        .unwrap()
        .contains("BLOCKED"));

    assert!(matches!(
        fixture.prepare(&band_request(&blocked)),
        Err(StrategyError::FeatureSetNotUsable { status, .. }) if status == "BLOCKED"
    ));
    assert!(matches!(
        fixture.prepare(&band_request(&format!("fs_{}", "0".repeat(64)))),
        Err(StrategyError::FeatureSetMissing(_))
    ));
    for path in [
        r"C:\Users\fixture\AppData\Roaming\MetaQuotes\Terminal\Common\Files\EURUSD.csv",
        "../feature-sets/fs_fixture",
        &format!("fs_{}", "A".repeat(64)),
    ] {
        assert!(
            matches!(
                fixture.prepare(&band_request(path)),
                Err(StrategyError::InvalidRequest(_))
            ),
            "{path}"
        );
    }
    assert!(fixture.prepare(&band_request(&ready)).is_ok());

    // Tampering with the source dataset after registration revokes the feature set, so no
    // strategy can consume it and it is offered as ineligible with the reason.
    let dataset_id = fixture
        .features
        .inspect_feature_set(&ready)
        .unwrap()
        .unwrap()
        .manifest
        .unwrap()
        .identity
        .inputs[0]
        .dataset_id
        .clone();
    flip_middle_byte(&fixture.dataset_bars(&dataset_id));
    assert!(matches!(
        fixture.prepare(&band_request(&ready)),
        Err(StrategyError::FeatureSetNotUsable { status, .. }) if status == "QUARANTINED"
    ));
    let candidates = fixture
        .store()
        .list_input_candidates(&AtomicBool::new(false))
        .unwrap();
    let revoked = candidates
        .iter()
        .find(|c| c.feature_set_id == ready)
        .unwrap();
    assert!(!revoked.eligible);
    assert!(revoked.reason.as_deref().unwrap().contains(&dataset_id));
    assert_eq!(
        fixture.staging_entries(),
        1,
        "only the earlier successful preview is staged"
    );
}

#[test]
fn gate_strategy_input_authority_decisions_use_registered_values_without_recomputation() {
    let fixture = Fixture::new();
    let feature_set = trend_feature_set(&fixture, "values", &market_bars(0.0, 1.0));
    let committed = fixture.decide(&band_request(&feature_set));
    let decided = decisions(&fixture, &committed.decision_set_id);
    let features = read_table(&fixture.feature_output(&feature_set));
    assert_eq!(decided.rows, features.rows);
    assert_eq!(
        decided.times("decision_time_utc"),
        features.times("decision_time_utc")
    );
    assert_eq!(
        decided.times("open_time_utc"),
        features.times("open_time_utc")
    );
    let statuses = features.text("roc__status");
    let values = features.floats("roc__value");
    let inputs = decided.floats("input__signal");
    for row in 0..features.rows {
        let expected = (statuses[row] == "READY").then(|| values[row].unwrap());
        assert_eq!(
            inputs[row].map(f64::to_bits),
            expected.map(f64::to_bits),
            "row {row}: the decision must read the registered value bit for bit"
        );
    }
}

#[test]
fn gate_typed_strategy_registry_bindings_must_match_feature_roles_and_outputs() {
    let fixture = Fixture::new();
    let feature_set = trend_feature_set(&fixture, "bindings", &market_bars(0.0, 1.0));
    let request =
        |bindings: &[(&str, &str)]| decision_request(&feature_set, "TREND_CONTEXT", &[], bindings);
    let invalid_binding = |result: Result<PreparedDecisionSet, StrategyError>, role: &str| {
        assert!(
            matches!(&result, Err(StrategyError::InvalidBinding { role: r, .. }) if r == role),
            "{role}: {result:?}"
        );
    };
    // Missing role, unknown feature, a TARGET role bound to a reference feature, and a REFERENCE
    // role bound to a target feature.
    invalid_binding(
        fixture.prepare(&request(&[("fast", "fast"), ("slow", "slow")])),
        "context",
    );
    invalid_binding(
        fixture.prepare(&request(&[
            ("fast", "fast"),
            ("slow", "missing"),
            ("context", "ctx"),
        ])),
        "slow",
    );
    invalid_binding(
        fixture.prepare(&request(&[
            ("fast", "ctx"),
            ("slow", "slow"),
            ("context", "ctx"),
        ])),
        "fast",
    );
    invalid_binding(
        fixture.prepare(&request(&[
            ("fast", "fast"),
            ("slow", "slow"),
            ("context", "roc"),
        ])),
        "context",
    );
    invalid_binding(
        fixture.prepare(&request(&[
            ("fast", "fast"),
            ("slow", "slow"),
            ("context", "ctx"),
            ("signal", "roc"),
        ])),
        "signal",
    );
    let mut wrong_output = trend_request(&feature_set, "ctx");
    wrong_output.bindings.get_mut("fast").unwrap().output = "upper".into();
    invalid_binding(fixture.prepare(&wrong_output), "fast");
    let mut wrong_version = trend_request(&feature_set, "ctx");
    wrong_version.strategy_version = 2;
    assert!(matches!(
        fixture.prepare(&wrong_version),
        Err(StrategyError::UnsupportedStrategyVersion { .. })
    ));
    assert_eq!(fixture.staging_entries(), 0);
}

#[test]
fn gate_typed_strategy_registry_one_definition_serves_any_target_series() {
    let fixture = Fixture::new();
    let mut ids = Vec::new();
    for (symbol, timeframe, count) in [
        ("EURUSD.m", Timeframe::H1, 48),
        ("XAUUSD.x", Timeframe::M15, 40),
    ] {
        let dataset = fixture.import(symbol, &bars(symbol, timeframe, START, count, 0.7));
        let feature_set = fixture.feature_set(&feature_request(
            selector(&dataset, symbol, timeframe.as_str()),
            vec![target_feature("roc", "ROC", 2)],
        ));
        let committed = fixture.decide(&band_request(&feature_set));
        let table = decisions(&fixture, &committed.decision_set_id);
        assert_eq!(table.rows, count);
        assert!(table.text("symbol").iter().all(|value| value == symbol));
        assert!(table
            .text("timeframe")
            .iter()
            .all(|value| value == timeframe.as_str()));
        ids.push(committed.decision_set_id);
    }
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn gate_decision_semantics_one_decision_per_target_bar_matches_an_independent_rule() {
    let fixture = Fixture::new();
    let feature_set = trend_feature_set(&fixture, "semantics", &market_bars(0.0, 1.0));
    let committed = fixture.decide(&band_request(&feature_set));
    let table = decisions(&fixture, &committed.decision_set_id);
    let features = read_table(&fixture.feature_output(&feature_set));
    assert_eq!(table.rows, 48);

    // Independent reference: BUY above 0.3, SELL below -0.3 (strict), WARMUP while ROC(2) seeds.
    let statuses = features.text("roc__status");
    let values = features.floats("roc__value");
    let (decision, reason, role) = (
        table.text("decision"),
        table.text("reason_code"),
        table.texts("reason_role"),
    );
    let mut counts = [0u64; 3];
    for row in 0..table.rows {
        let expected = match (statuses[row].as_str(), values[row]) {
            ("WARMUP", _) => ("SKIP", "WARMUP", Some("signal")),
            ("READY", Some(value)) if value > 0.3 => ("BUY", "BUY_RULE", None),
            ("READY", Some(value)) if value < -0.3 => ("SELL", "SELL_RULE", None),
            ("READY", Some(_)) => ("SKIP", "NO_SIGNAL", None),
            other => panic!("unexpected feature row {other:?}"),
        };
        assert_eq!(
            (
                decision[row].as_str(),
                reason[row].as_str(),
                role[row].as_deref()
            ),
            expected,
            "row {row}"
        );
        counts[["BUY", "SELL", "SKIP"]
            .iter()
            .position(|d| *d == expected.0)
            .unwrap()] += 1;
    }
    assert_eq!(&statuses[..2], ["WARMUP", "WARMUP"]);
    assert!(
        counts[0] > 0 && counts[1] > 0 && counts[2] > 2,
        "{counts:?}"
    );
    let manifest = fixture
        .store()
        .inspect_decision_set(&committed.decision_set_id)
        .unwrap()
        .unwrap()
        .manifest
        .unwrap();
    assert_eq!(
        [
            manifest.summary.buy_rows,
            manifest.summary.sell_rows,
            manifest.summary.skip_rows
        ],
        counts
    );
    assert_eq!(manifest.summary.skip_reasons["WARMUP"], 2);
    assert!(manifest.research_notice.contains("not orders"));
    assert_eq!(manifest.identity.feature_set.target.symbol, TARGET);
}

#[test]
fn gate_causal_no_lookahead_future_perturbation_leaves_earlier_decisions_unchanged() {
    let fixture = Fixture::new();
    let base_rows = market_bars(0.0, 1.0);
    let base = trend_feature_set(&fixture, "causal-base", &base_rows);

    // Target bars from index 30 change; decisions up to bar 29's close must not.
    let mut target_future = base_rows.clone();
    perturb_from(&mut target_future[..48], 30, 1.05);
    let changed_target = trend_feature_set(&fixture, "causal-target", &target_future);

    // Context H4 bars from index 6 (open 24:00, available 28:00) change; H1 decisions before
    // 28:00 must not.
    let mut context_future = base_rows.clone();
    perturb_from(&mut context_future[48..], 6, 1.07);
    let changed_context = trend_feature_set(&fixture, "causal-context", &context_future);

    let table = |feature_set: &str| {
        let committed = fixture.decide(&trend_request(feature_set, "ctx"));
        decisions(&fixture, &committed.decision_set_id)
    };
    let (base, target, context) = (
        table(&base),
        table(&changed_target),
        table(&changed_context),
    );
    for row in 0..30 {
        assert_eq!(
            base.row(row),
            target.row(row),
            "target perturbation leaked into row {row}"
        );
    }
    assert!((30..48).any(|row| base.row(row) != target.row(row)));
    let boundary = micros(START) + 28 * HOUR_US;
    let decision_times = base.times("decision_time_utc");
    let unaffected = decision_times
        .iter()
        .filter(|time| **time < boundary)
        .count();
    assert_eq!(unaffected, 27);
    for row in 0..unaffected {
        assert_eq!(
            base.row(row),
            context.row(row),
            "late context leaked into row {row}"
        );
    }
    assert!((unaffected..48).any(|row| base.row(row) != context.row(row)));
}

#[test]
fn gate_mtf_context_only_reference_series_never_adds_decision_rows() {
    let fixture = Fixture::new();
    let feature_set = trend_feature_set(&fixture, "context", &market_bars(0.0, 1.0));
    let committed = fixture.decide(&trend_request(&feature_set, "ctx_tight"));
    let table = decisions(&fixture, &committed.decision_set_id);
    let feature_rows = read_table(&fixture.feature_output(&feature_set)).rows;
    assert_eq!(table.rows, feature_rows);
    assert_eq!(
        table.rows, 48,
        "only target H1 bars are decided, never the 12 H4 context bars"
    );
    assert!(table.text("symbol").iter().all(|symbol| symbol == TARGET));
    assert!(table
        .text("timeframe")
        .iter()
        .all(|timeframe| timeframe == "H1"));

    // With a one-hour staleness limit, an H4 context older than an hour is STALE: the decision is
    // SKIP and names the context role instead of acting on an old reference value.
    let reasons = table.text("reason_code");
    let roles = table.texts("reason_role");
    let stale: Vec<_> = (0..table.rows)
        .filter(|row| reasons[*row] == "STALE")
        .collect();
    assert!(!stale.is_empty());
    assert!(stale
        .iter()
        .all(|row| roles[*row].as_deref() == Some("context")));
    assert!(table
        .floats("input__context")
        .iter()
        .zip(&reasons)
        .all(|(value, reason)| reason != "STALE" || value.is_none()));

    let manifest = fixture
        .store()
        .inspect_decision_set(&committed.decision_set_id)
        .unwrap()
        .unwrap()
        .manifest
        .unwrap();
    assert_eq!(manifest.lineage.context_series.len(), 1);
    assert_eq!(manifest.lineage.context_series[0].symbol, CONTEXT);
    assert_eq!(manifest.lineage.context_series[0].timeframe, "H4");
    assert_eq!(manifest.lineage.target.symbol, TARGET);
}

#[test]
fn gate_skip_and_conflict_handling_unavailable_inputs_skip_and_counts_are_not_inflated() {
    let fixture = Fixture::new();
    let feature_set = trend_feature_set(&fixture, "skips", &market_bars(0.0, 1.0));
    // The same feature bound to both trend roles is never above or below itself.
    let duplicated = decision_request(
        &feature_set,
        "TREND_CONTEXT",
        &[],
        &[("fast", "slow"), ("slow", "slow"), ("context", "ctx")],
    );
    let committed = fixture.decide(&duplicated);
    let table = decisions(&fixture, &committed.decision_set_id);
    let decisions_column = table.text("decision");
    assert!(decisions_column.iter().all(|decision| decision == "SKIP"));
    let reasons = table.text("reason_code");
    let roles = table.texts("reason_role");
    // Role order decides which unavailable input is named: SMA(8) bound as fast seeds over rows
    // 0-6; then RSI(3) on H4 context is still seeding until its fourth H4 bar is available at
    // 16:00, the decision time of row 15. Only then does the rule run, and x vs x is no signal.
    let warmup =
        |row: usize, role: &str| reasons[row] == "WARMUP" && roles[row].as_deref() == Some(role);
    assert!((0..7).all(|row| warmup(row, "fast")));
    assert!((7..15).all(|row| warmup(row, "context")));
    assert!((15..table.rows).all(|row| reasons[row] == "NO_SIGNAL" && roles[row].is_none()));
    let manifest = fixture
        .store()
        .inspect_decision_set(&committed.decision_set_id)
        .unwrap()
        .unwrap()
        .manifest
        .unwrap();
    assert_eq!(manifest.row_count, 48);
    assert_eq!(manifest.summary.skip_rows, 48);
    assert_eq!(manifest.summary.buy_rows + manifest.summary.sell_rows, 0);
    assert_eq!(manifest.summary.skip_reasons.values().sum::<u64>(), 48);

    // A context band no RSI value reaches turns every READY row into CONTEXT_FILTER, never a
    // direction.
    let filtered = decision_request(
        &feature_set,
        "TREND_CONTEXT",
        &[("context_min", 101.0), ("context_max", 200.0)],
        &[("fast", "fast"), ("slow", "slow"), ("context", "ctx")],
    );
    let committed = fixture.decide(&filtered);
    let table = decisions(&fixture, &committed.decision_set_id);
    assert!(table
        .text("decision")
        .iter()
        .all(|decision| decision == "SKIP"));
    assert!(table
        .text("reason_code")
        .iter()
        .any(|reason| reason == "CONTEXT_FILTER"));
}

#[test]
fn gate_immutable_decision_materialization_registers_only_after_confirmation_and_replays() {
    let fixture = Fixture::new();
    let feature_set = trend_feature_set(&fixture, "materialize", &market_bars(0.0, 1.0));
    let never = AtomicBool::new(false);
    let prepared = fixture.prepare(&band_request(&feature_set)).unwrap();
    let id = prepared.manifest.decision_set_id.clone();
    assert!(id.starts_with("dc_"));
    assert!(prepared.manifest.identity_is_valid());
    assert_eq!(prepared.preview.sample_rows.len(), 20);
    assert!(fixture
        .store()
        .list_decision_sets(&never)
        .unwrap()
        .is_empty());
    assert!(fixture.store().inspect_decision_set(&id).unwrap().is_none());
    assert_eq!(fixture.staging_entries(), 1);
    assert!(!prepared
        .staging_directory
        .join("input-features.parquet")
        .exists());

    let committed = fixture.store().commit_prepared(prepared).unwrap();
    assert_eq!(
        committed.disposition,
        DecisionCommitDisposition::Materialized
    );
    assert_eq!(fixture.staging_entries(), 0);
    let catalog = fixture.store().list_decision_sets(&never).unwrap();
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].status, DecisionSetStatus::Ready);
    assert_eq!(catalog[0].feature_set_id, feature_set);
    let first_bytes = std::fs::read(fixture.decision_dir(&id).join("decisions.parquet")).unwrap();

    // Replaying the same identity is byte-identical and reuses the registered set.
    let replay = fixture.prepare(&band_request(&feature_set)).unwrap();
    assert_eq!(replay.manifest.decision_set_id, id);
    assert_eq!(
        std::fs::read(replay.staging_directory.join("decisions.parquet")).unwrap(),
        first_bytes
    );
    let reused = fixture.store().commit_prepared(replay).unwrap();
    assert_eq!(reused.disposition, DecisionCommitDisposition::Reused);

    // A changed parameter or a different feature set is a new identity.
    let mut changed = band_request(&feature_set);
    changed.parameters.insert("upper".into(), 0.31);
    assert_ne!(
        fixture.prepare(&changed).unwrap().manifest.decision_set_id,
        id
    );
    let other_feature_set =
        trend_feature_set(&fixture, "materialize-other", &market_bars(0.2, 1.0));
    let other = fixture.prepare(&band_request(&other_feature_set)).unwrap();
    assert_ne!(other.manifest.decision_set_id, id);

    // A discarded preview leaves no stage and is recorded.
    fixture.store().discard_prepared(other).unwrap();
    let statuses: Vec<_> = fixture
        .store()
        .list_attempts()
        .unwrap()
        .into_iter()
        .map(|attempt| attempt.status)
        .collect();
    assert!(statuses.contains(&"MATERIALIZED".to_owned()));
    assert!(statuses.contains(&"REUSED".to_owned()));
    assert!(statuses.contains(&"CANCELLED".to_owned()));
    assert_eq!(fixture.store().list_decision_sets(&never).unwrap().len(), 1);
}

#[test]
fn gate_resource_cancel_recovery_quotas_and_cancellation_leave_nothing_registered() {
    let fixture = Fixture::new();
    let feature_set = trend_feature_set(&fixture, "quotas", &market_bars(0.0, 1.0));
    let mut rows = band_request(&feature_set);
    rows.quotas.max_rows = 47;
    assert!(
        matches!(fixture.prepare(&rows), Err(StrategyError::ResourceQuota(message)) if message.contains("48"))
    );
    let mut bytes = band_request(&feature_set);
    bytes.quotas.max_output_bytes = 64;
    assert!(matches!(
        fixture.prepare(&bytes),
        Err(StrategyError::ResourceQuota(_))
    ));
    let mut zero = band_request(&feature_set);
    zero.quotas.max_rows = 0;
    assert!(matches!(
        fixture.prepare(&zero),
        Err(StrategyError::InvalidRequest(_))
    ));
    let cancelled = fixture.store().prepare(
        &band_request(&feature_set),
        &AtomicBool::new(true),
        &DecisionProgress::default(),
    );
    assert!(matches!(cancelled, Err(StrategyError::Cancelled)));
    assert_eq!(fixture.staging_entries(), 0);
    assert!(fixture
        .store()
        .list_decision_sets(&AtomicBool::new(false))
        .unwrap()
        .is_empty());
}

#[test]
fn gate_resource_cancel_recovery_output_quota_bounds_the_final_file_including_its_footer() {
    let fixture = Fixture::new();
    let feature_set = trend_feature_set(&fixture, "output-quota", &market_bars(0.0, 1.0));
    let final_size = {
        let prepared = fixture.prepare(&band_request(&feature_set)).unwrap();
        let size = std::fs::metadata(prepared.staging_directory.join("decisions.parquet"))
            .unwrap()
            .len();
        fixture.store().discard_prepared(prepared).unwrap();
        size
    };
    let mut just_below = band_request(&feature_set);
    just_below.quotas.max_output_bytes = final_size - 1;
    assert!(
        matches!(
            fixture.prepare(&just_below),
            Err(StrategyError::ResourceQuota(_))
        ),
        "a {final_size}-byte output must not pass a {}-byte quota",
        final_size - 1
    );
    assert_eq!(fixture.staging_entries(), 0);
    let mut exact = band_request(&feature_set);
    exact.quotas.max_output_bytes = final_size;
    let prepared = fixture.prepare(&exact).unwrap();
    assert_eq!(
        std::fs::metadata(prepared.staging_directory.join("decisions.parquet"))
            .unwrap()
            .len(),
        final_size
    );
}

fn journal(fixture: &Fixture, prepared: &PreparedDecisionSet) {
    let manifest = &prepared.manifest;
    let connection = open_database(fixture.root.join("research.sqlite3")).unwrap();
    save_pending_decision_set_promotion(
        &connection,
        &rg_persistence::PendingDecisionSetPromotion {
            staging_name: prepared
                .staging_directory
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            decision_set: rg_persistence::DecisionSetMetadata {
                decision_set_id: manifest.decision_set_id.clone(),
                identity_sha256: manifest.identity_sha256.clone(),
                feature_set_id: manifest.identity.feature_set.feature_set_id.clone(),
                feature_set_manifest_sha256: manifest.identity.feature_set.manifest_sha256.clone(),
                manifest_json: prepared.manifest_json.clone(),
                manifest_sha256: prepared.manifest_sha256.clone(),
                output_sha256: manifest.output_sha256.clone(),
                created_at_utc: manifest.created_at_utc.clone(),
                status: "READY".into(),
                manifest_integrity_valid: true,
            },
            attempt: DecisionAttemptRecord {
                attempt_id: format!("crash-{}", manifest.decision_set_id),
                started_at_utc: manifest.created_at_utc.clone(),
                completed_at_utc: manifest.created_at_utc.clone(),
                status: "MATERIALIZED".into(),
                decision_set_id: None,
                diagnostic_code: None,
                diagnostic_message: None,
            },
        },
    )
    .unwrap();
}

#[test]
fn gate_resource_cancel_recovery_restart_completes_a_journaled_promotion_before_and_after_rename() {
    let mut fixture = Fixture::new();
    let feature_set = trend_feature_set(&fixture, "recovery", &market_bars(0.0, 1.0));

    // Crash after journaling, before the rename.
    let before = fixture.prepare(&band_request(&feature_set)).unwrap();
    journal(&fixture, &before);
    let before_id = before.manifest.decision_set_id.clone();
    drop(before);
    fixture.reopen().unwrap();
    let entry = fixture
        .store()
        .inspect_decision_set(&before_id)
        .unwrap()
        .unwrap();
    assert_eq!(entry.status, DecisionSetStatus::Ready);

    // Crash after the rename, before registration.
    let after = fixture
        .prepare(&trend_request(&feature_set, "ctx"))
        .unwrap();
    journal(&fixture, &after);
    let after_id = after.manifest.decision_set_id.clone();
    std::fs::rename(&after.staging_directory, fixture.decision_dir(&after_id)).unwrap();
    drop(after);
    fixture.reopen().unwrap();
    let entry = fixture
        .store()
        .inspect_decision_set(&after_id)
        .unwrap()
        .unwrap();
    assert_eq!(entry.status, DecisionSetStatus::Ready);
    assert_eq!(fixture.staging_entries(), 0);
    assert_eq!(
        fixture
            .store()
            .list_decision_sets(&AtomicBool::new(false))
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn gate_resource_cancel_recovery_recovery_fails_closed_and_preserves_tampered_evidence() {
    let mut fixture = Fixture::new();
    let feature_set = trend_feature_set(&fixture, "recovery-tamper", &market_bars(0.0, 1.0));
    let prepared = fixture.prepare(&band_request(&feature_set)).unwrap();
    journal(&fixture, &prepared);
    let stage = prepared.staging_directory.clone();
    flip_middle_byte(&stage.join("decisions.parquet"));
    drop(prepared);
    assert!(matches!(
        fixture.reopen(),
        Err(StrategyError::RecoveryBlocked { .. })
    ));
    assert!(
        stage.join("decisions.parquet").exists(),
        "evidence must be preserved"
    );
}

#[test]
fn gate_adversarial_tamper_tampered_or_missing_decision_files_are_quarantined() {
    let fixture = Fixture::new();
    let feature_set = trend_feature_set(&fixture, "tamper", &market_bars(0.0, 1.0));
    let output_id = fixture.decide(&band_request(&feature_set)).decision_set_id;
    let manifest_id = fixture
        .decide(&trend_request(&feature_set, "ctx"))
        .decision_set_id;
    let missing_id = fixture
        .decide(&trend_request(&feature_set, "ctx_tight"))
        .decision_set_id;
    assert!(fixture.store().output_path(&output_id).is_ok());

    flip_middle_byte(&fixture.decision_dir(&output_id).join("decisions.parquet"));
    let manifest_path = fixture.decision_dir(&manifest_id).join("manifest.json");
    let tampered = std::fs::read_to_string(&manifest_path)
        .unwrap()
        .replace("\"upper\":0.3", "\"upper\":0.4")
        .replace("\"margin\":0.0", "\"margin\":9.0");
    std::fs::write(&manifest_path, tampered).unwrap();
    std::fs::remove_file(fixture.decision_dir(&missing_id).join("decisions.parquet")).unwrap();

    for (id, check) in [
        (&output_id, "output"),
        (&manifest_id, "manifest"),
        (&missing_id, "output"),
    ] {
        let entry = fixture.store().inspect_decision_set(id).unwrap().unwrap();
        assert_eq!(entry.status, DecisionSetStatus::Quarantined, "{check}");
        assert!(entry.diagnostic.is_some());
        assert!(matches!(
            fixture.store().output_path(id),
            Err(StrategyError::DecisionSetNotUsable { status, .. }) if status == "QUARANTINED"
        ));
    }
    let catalog = fixture
        .store()
        .list_decision_sets(&AtomicBool::new(false))
        .unwrap();
    assert!(catalog
        .iter()
        .all(|entry| entry.status == DecisionSetStatus::Quarantined));
}

#[test]
fn gate_adversarial_tamper_source_tamper_after_registration_revokes_decision_readiness() {
    let fixture = Fixture::new();
    let feature_set = trend_feature_set(&fixture, "source-tamper", &market_bars(0.0, 1.0));
    let id = fixture.decide(&band_request(&feature_set)).decision_set_id;
    assert_eq!(
        fixture
            .store()
            .inspect_decision_set(&id)
            .unwrap()
            .unwrap()
            .status,
        DecisionSetStatus::Ready
    );

    flip_middle_byte(&fixture.feature_output(&feature_set));
    let entry = fixture.store().inspect_decision_set(&id).unwrap().unwrap();
    assert_eq!(entry.status, DecisionSetStatus::Quarantined);
    assert!(!entry.integrity.lineage_valid);
    assert!(
        entry.integrity.output_valid,
        "the decision file itself is unchanged"
    );
    assert!(entry.diagnostic.as_deref().unwrap().contains(&feature_set));
    assert!(fixture.store().output_path(&id).is_err());
}

#[test]
fn gate_adversarial_tamper_feature_set_changed_between_preview_and_confirm_fails_closed() {
    let fixture = Fixture::new();
    let feature_set = trend_feature_set(&fixture, "toctou", &market_bars(0.0, 1.0));
    let prepared = fixture.prepare(&band_request(&feature_set)).unwrap();
    let id = prepared.manifest.decision_set_id.clone();
    flip_middle_byte(&fixture.feature_output(&feature_set));
    assert!(matches!(
        fixture.store().commit_prepared(prepared),
        Err(StrategyError::FeatureSetIntegrity { .. })
    ));
    assert!(fixture.store().inspect_decision_set(&id).unwrap().is_none());
    assert_eq!(fixture.staging_entries(), 0);
    let attempts = fixture.store().list_attempts().unwrap();
    assert_eq!(attempts[0].status, "FAILED");
    assert_eq!(
        attempts[0].diagnostic_code.as_deref(),
        Some("FEATURE_SET_INTEGRITY")
    );
}
