//! End-to-end Feature Studio tests over synthetic fixtures imported through Data Ground.
//! Test names carry the PHASE_MANIFEST.yaml gate they evidence.

mod support;

use std::{
    collections::BTreeSet,
    sync::{atomic::AtomicBool, Arc},
};

use rg_data_ground::{CalendarPolicy, Timeframe};
use rg_feature_studio::{
    store::{FeatureCommitDisposition, FeatureStore},
    FeatureError, FeatureProgress, FeatureSetStatus, ParameterValue, ProgressPhase, PREVIEW_ROWS,
};
use rg_persistence::{
    open_database, save_pending_feature_set_promotion, FeatureAttemptRecord, FeatureSetInput,
    FeatureSetMetadata, PendingFeatureSetPromotion,
};
use support::*;

/// One READY dataset with EURUSD.m H1 (48 bars), EURUSD.m H4 (12 bars), and GBPUSD.m H1 (48 bars).
fn standard_dataset(fixture: &Fixture, name: &str) -> String {
    let mut rows = series(
        "EURUSD.m",
        Timeframe::H1,
        "2024-01-01T00:00:00Z",
        48,
        0.0,
        false,
    );
    rows.extend(series(
        "EURUSD.m",
        Timeframe::H4,
        "2024-01-01T00:00:00Z",
        12,
        0.5,
        false,
    ));
    rows.extend(series(
        "GBPUSD.m",
        Timeframe::H1,
        "2024-01-01T00:00:00Z",
        48,
        1.5,
        false,
    ));
    fixture.import(name, &rows, CalendarPolicy::Continuous)
}

fn standard_request(dataset: &str) -> rg_feature_studio::FeatureSetRequest {
    request(
        selector(dataset, "EURUSD.m", "H1"),
        vec![
            target_feature("sma_3", "SMA", period(3)),
            target_feature("rsi_5", "RSI_WILDER", period(5)),
            reference_feature(
                "gbp_ema_4",
                "EMA",
                period(4),
                selector(dataset, "GBPUSD.m", "H1"),
                3600,
                0,
            ),
        ],
    )
}

fn commit(fixture: &Fixture, request: &rg_feature_studio::FeatureSetRequest) -> String {
    let prepared = fixture.prepare(request).unwrap();
    fixture
        .store()
        .commit_prepared(prepared)
        .unwrap()
        .feature_set_id
}

// ---------------------------------------------------------------------------------------------
// input_authority

#[test]
fn gate_input_authority_accepts_only_registered_ready_verified_datasets() {
    let fixture = Fixture::new();
    let ready = standard_dataset(&fixture, "ready");
    let mut gapped = series(
        "EURUSD.m",
        Timeframe::H1,
        "2024-01-01T00:00:00Z",
        10,
        0.0,
        false,
    );
    gapped.remove(5);
    let blocked = fixture.import("blocked-gap", &gapped, CalendarPolicy::Unknown);

    let missing = format!("ds_{}", "0".repeat(64));
    assert!(matches!(
        fixture.prepare(&standard_request(&missing)),
        Err(FeatureError::InputMissing(id)) if id == missing
    ));
    let blocked_request = request(
        selector(&blocked, "EURUSD.m", "H1"),
        vec![target_feature("sma_3", "SMA", period(3))],
    );
    assert!(matches!(
        fixture.prepare(&blocked_request),
        Err(FeatureError::InputNotReady { status, .. }) if status == "BLOCKED"
    ));
    let blocked_reference = request(
        selector(&ready, "EURUSD.m", "H1"),
        vec![reference_feature(
            "blocked_ref",
            "SMA",
            period(2),
            selector(&blocked, "EURUSD.m", "H1"),
            3600,
            0,
        )],
    );
    assert!(matches!(
        fixture.prepare(&blocked_reference),
        Err(FeatureError::InputNotReady { .. })
    ));
    assert!(fixture.prepare(&standard_request(&ready)).is_ok());
    assert_eq!(
        fixture
            .store()
            .list_feature_sets(&AtomicBool::new(false))
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn gate_input_authority_rejects_tampered_and_quarantined_snapshots() {
    for file_name in ["bars.parquet", "manifest.json", "raw-source.csv"] {
        let fixture = Fixture::new();
        let dataset = standard_dataset(&fixture, "tamper");
        std::fs::write(fixture.dataset_dir(&dataset).join(file_name), b"tampered").unwrap();
        assert!(
            matches!(
                fixture.prepare(&standard_request(&dataset)),
                Err(FeatureError::InputIntegrity { .. })
            ),
            "{file_name}"
        );
        let candidates = fixture
            .store()
            .list_input_candidates(&AtomicBool::new(false))
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(!candidates[0].eligible);
        assert_eq!(candidates[0].status.as_str(), "QUARANTINED");
        assert_eq!(fixture.staging_entries(), 0);
    }
}

#[test]
fn gate_input_authority_requires_exact_series_identities_including_broker_suffix() {
    let fixture = Fixture::new();
    let dataset = standard_dataset(&fixture, "suffix");
    for (symbol, timeframe) in [("EURUSD", "H1"), ("EURUSD.m", "M15"), ("XAUUSD.m", "H1")] {
        let missing = request(
            selector(&dataset, symbol, timeframe),
            vec![target_feature("sma_3", "SMA", period(3))],
        );
        assert!(
            matches!(
                fixture.prepare(&missing),
                Err(FeatureError::SeriesNotFound { .. })
            ),
            "{symbol} {timeframe}"
        );
    }
    let absent_reference = request(
        selector(&dataset, "EURUSD.m", "H1"),
        vec![reference_feature(
            "gbp",
            "SMA",
            period(2),
            selector(&dataset, "GBPUSD", "H1"),
            3600,
            0,
        )],
    );
    assert!(matches!(
        fixture.prepare(&absent_reference),
        Err(FeatureError::SeriesNotFound { .. })
    ));
}

#[test]
fn gate_input_authority_leaves_source_snapshots_byte_identical() {
    let fixture = Fixture::new();
    let dataset = standard_dataset(&fixture, "immutable-source");
    let directory = fixture.dataset_dir(&dataset);
    let before: Vec<_> = ["bars.parquet", "manifest.json", "raw-source.csv"]
        .iter()
        .map(|name| file_sha256(&directory.join(name)))
        .collect();
    commit(&fixture, &standard_request(&dataset));
    let after: Vec<_> = ["bars.parquet", "manifest.json", "raw-source.csv"]
        .iter()
        .map(|name| file_sha256(&directory.join(name)))
        .collect();
    assert_eq!(before, after);
    let entry = fixture.datasets.inspect_dataset(&dataset).unwrap().unwrap();
    assert_eq!(entry.status.as_str(), "READY");
    assert!(entry.integrity.canonical_parquet_valid);
}

// ---------------------------------------------------------------------------------------------
// causal_availability

#[test]
fn gate_causal_availability_rows_are_decided_at_bar_close_with_null_warmup() {
    let fixture = Fixture::new();
    let rows = series(
        "EURUSD.m",
        Timeframe::H1,
        "2024-01-01T00:00:00Z",
        12,
        0.0,
        false,
    );
    let dataset = fixture.import("causal", &rows, CalendarPolicy::Continuous);
    let prepared = fixture
        .prepare(&request(
            selector(&dataset, "EURUSD.m", "H1"),
            vec![target_feature("sma_3", "SMA", period(3))],
        ))
        .unwrap();
    let table = read_output(&prepared.staging_directory.join("features.parquet"));
    assert_eq!(table.rows, 12);
    let opens = table.times("open_time_utc");
    let closes = table.times("close_time_utc");
    let decisions = table.times("decision_time_utc");
    let values = table.floats("sma_3__value");
    let statuses = table.strings("sma_3__status");
    for index in 0..12 {
        let open = rows[index].open_utc.timestamp_micros();
        assert_eq!(opens[index], Some(open));
        assert_eq!(closes[index], Some(open + HOUR_US));
        assert_eq!(
            decisions[index], closes[index],
            "decision boundary is the bar close"
        );
        if index < 2 {
            assert_eq!(values[index], None);
            assert_eq!(statuses[index], "WARMUP");
        } else {
            let expected =
                (rows[index - 2].close + rows[index - 1].close + rows[index].close) / 3.0;
            assert_eq!(
                values[index],
                Some(expected),
                "current closed bar is included"
            );
            assert_eq!(statuses[index], "READY");
        }
    }
    assert!(decisions.windows(2).all(|pair| pair[0] < pair[1]));
    let summary = &prepared.manifest.feature_summaries[0];
    assert_eq!((summary.warmup_rows, summary.ready_rows), (2, 10));
    assert_eq!(
        summary.first_ready_decision_utc.as_deref(),
        Some("2024-01-01T03:00:00.000000Z")
    );
}

#[test]
fn gate_causal_availability_late_reference_waits_for_declared_lag() {
    let fixture = Fixture::new();
    let dataset = standard_dataset(&fixture, "lagged");
    let lagged = request(
        selector(&dataset, "EURUSD.m", "H1"),
        vec![reference_feature(
            "gbp_close",
            "SMA",
            period(1),
            selector(&dataset, "GBPUSD.m", "H1"),
            2 * 3600,
            1800,
        )],
    );
    let prepared = fixture.prepare(&lagged).unwrap();
    let table = read_output(&prepared.staging_directory.join("features.parquet"));
    let statuses = table.strings("gbp_close__status");
    let sources = table.times("gbp_close__source_available_at_utc");
    let decisions = table.times("decision_time_utc");
    // The first GBP bar closes at 01:00 and becomes visible at 01:30, after the 01:00 decision.
    assert_eq!(statuses[0], "NO_OBSERVATION");
    for index in 1..table.rows {
        assert_eq!(statuses[index], "READY");
        let source = sources[index].unwrap();
        assert!(source <= decisions[index].unwrap());
        // With a 30 minute lag on hourly bars, the previous hour's bar is the newest visible one.
        assert_eq!(source, decisions[index].unwrap() - HOUR_US / 2);
    }
}

#[test]
fn gate_causal_availability_outputs_are_finite_and_non_finite_math_fails_closed() {
    let fixture = Fixture::new();
    let dataset = standard_dataset(&fixture, "finite");
    let prepared = fixture.prepare(&standard_request(&dataset)).unwrap();
    let table = read_output(&prepared.staging_directory.join("features.parquet"));
    for name in ["sma_3__value", "rsi_5__value", "gbp_ema_4__value"] {
        assert!(table
            .floats(name)
            .iter()
            .flatten()
            .all(|value| value.is_finite()));
    }
    fixture.store().discard_prepared(prepared).unwrap();

    let mut huge = series(
        "EURUSD.m",
        Timeframe::H1,
        "2024-01-01T00:00:00Z",
        6,
        0.0,
        false,
    );
    scale_prices(&mut huge, 1e300);
    let huge_dataset = fixture.import("overflow", &huge, CalendarPolicy::Continuous);
    let error = fixture
        .prepare(&request(
            selector(&huge_dataset, "EURUSD.m", "H1"),
            vec![target_feature("std_2", "STDDEV", period(2))],
        ))
        .unwrap_err();
    assert!(
        matches!(&error, FeatureError::Indicator { feature_key, .. } if feature_key == "std_2"),
        "{error}"
    );
    assert_eq!(fixture.staging_entries(), 0);
}

// ---------------------------------------------------------------------------------------------
// multi_symbol_timeframe_join

#[test]
fn gate_multi_symbol_timeframe_join_never_exposes_an_unfinished_h4_bar() {
    let fixture = Fixture::new();
    let dataset = standard_dataset(&fixture, "mtf");
    let prepared = fixture
        .prepare(&request(
            selector(&dataset, "EURUSD.m", "H1"),
            vec![reference_feature(
                "h4_close",
                "SMA",
                period(1),
                selector(&dataset, "EURUSD.m", "H4"),
                4 * 3600,
                0,
            )],
        ))
        .unwrap();
    let h4 = series(
        "EURUSD.m",
        Timeframe::H4,
        "2024-01-01T00:00:00Z",
        12,
        0.5,
        false,
    );
    let table = read_output(&prepared.staging_directory.join("features.parquet"));
    let values = table.floats("h4_close__value");
    let statuses = table.strings("h4_close__status");
    let sources = table.times("h4_close__source_available_at_utc");
    for hour in 0..48usize {
        let decision_hour = hour + 1;
        if decision_hour < 4 {
            assert_eq!(statuses[hour], "NO_OBSERVATION", "hour {hour}");
            assert_eq!(values[hour], None);
            continue;
        }
        // The newest H4 bar whose close (open + 4h) is at or before the decision.
        let visible = decision_hour / 4 - 1;
        assert_eq!(statuses[hour], "READY", "hour {hour}");
        assert_eq!(values[hour], Some(h4[visible].close), "hour {hour}");
        assert_eq!(
            sources[hour],
            Some(h4[visible].open_utc.timestamp_micros() + 4 * HOUR_US)
        );
    }
    // An H1 decision exactly at an H4 close sees that H4 bar (equal timestamps are available).
    assert_eq!(sources[3], table.times("decision_time_utc")[3]);
}

#[test]
fn gate_multi_symbol_timeframe_join_marks_stale_references_not_available() {
    let fixture = Fixture::new();
    let dataset = standard_dataset(&fixture, "stale");
    let prepared = fixture
        .prepare(&request(
            selector(&dataset, "EURUSD.m", "H1"),
            vec![reference_feature(
                "h4_close",
                "SMA",
                period(1),
                selector(&dataset, "EURUSD.m", "H4"),
                2 * 3600,
                0,
            )],
        ))
        .unwrap();
    let table = read_output(&prepared.staging_directory.join("features.parquet"));
    let statuses = table.strings("h4_close__status");
    let values = table.floats("h4_close__value");
    for hour in 3..48usize {
        let age_hours = (hour + 1) % 4;
        let expected = if age_hours <= 2 { "READY" } else { "STALE" };
        assert_eq!(statuses[hour], expected, "hour {hour}");
        assert_eq!(values[hour].is_some(), expected == "READY");
    }
    let summary = &prepared.manifest.feature_summaries[0];
    assert_eq!(summary.no_observation_rows, 3);
    assert_eq!(summary.stale_rows, 11);
    assert_eq!(summary.ready_rows, 34);
}

#[test]
fn gate_multi_symbol_timeframe_join_respects_a_reference_market_calendar() {
    let fixture = Fixture::new();
    // A continuous target market and a reference market closed on UTC weekends.
    let target = fixture.import(
        "continuous-target",
        &series(
            "BTCUSD",
            Timeframe::H1,
            "2024-01-05T00:00:00Z",
            96,
            0.0,
            false,
        ),
        CalendarPolicy::Continuous,
    );
    let reference = fixture.import(
        "weekend-reference",
        &series(
            "EURUSD.m",
            Timeframe::H1,
            "2024-01-05T00:00:00Z",
            48,
            1.0,
            true,
        ),
        CalendarPolicy::WeekendsUtc,
    );
    let prepared = fixture
        .prepare(&request(
            selector(&target, "BTCUSD", "H1"),
            vec![reference_feature(
                "fx_close",
                "SMA",
                period(1),
                selector(&reference, "EURUSD.m", "H1"),
                3600,
                0,
            )],
        ))
        .unwrap();
    let table = read_output(&prepared.staging_directory.join("features.parquet"));
    let decisions = table.times("decision_time_utc");
    let statuses = table.strings("fx_close__status");
    let saturday_0100 = micros("2024-01-06T01:00:00Z");
    let monday_0100 = micros("2024-01-08T01:00:00Z");
    for (decision, status) in decisions.iter().zip(&statuses) {
        let decision = decision.unwrap();
        let expected = if decision > saturday_0100 && decision < monday_0100 {
            "STALE"
        } else {
            "READY"
        };
        assert_eq!(status, expected, "decision {decision}");
    }
    assert_eq!(prepared.manifest.feature_summaries[0].stale_rows, 47);
    assert_eq!(prepared.manifest.identity.inputs.len(), 2);
}

// ---------------------------------------------------------------------------------------------
// feature_materialization

#[test]
fn gate_feature_materialization_registers_only_after_confirmation_with_lineage() {
    let fixture = Fixture::new();
    let dataset = standard_dataset(&fixture, "lineage");
    let prepared = fixture.prepare(&standard_request(&dataset)).unwrap();
    let never = AtomicBool::new(false);
    assert!(fixture
        .store()
        .list_feature_sets(&never)
        .unwrap()
        .is_empty());
    let id = prepared.manifest.feature_set_id.clone();
    assert_eq!(id, format!("fs_{}", prepared.manifest.identity_sha256));
    assert_eq!(prepared.preview.sample_rows.len(), PREVIEW_ROWS);
    let result = fixture.store().commit_prepared(prepared).unwrap();
    assert_eq!(result.disposition, FeatureCommitDisposition::Materialized);
    assert_eq!(result.status, FeatureSetStatus::Ready);

    let catalog = fixture.store().list_feature_sets(&never).unwrap();
    assert_eq!(catalog.len(), 1);
    let entry = &catalog[0];
    assert_eq!(entry.feature_set_id, id);
    assert!(entry.integrity.manifest_valid && entry.integrity.output_valid);
    assert!(entry.integrity.lineage_valid);
    let manifest = entry.manifest.as_ref().unwrap();
    assert_eq!(manifest.identity.engine_id, "rg-native-features");
    assert_eq!(manifest.lineage.len(), 1);
    assert_eq!(
        manifest.lineage[0].series_used,
        vec!["EURUSD.m H1".to_owned(), "GBPUSD.m H1".to_owned()]
    );
    assert_eq!(manifest.row_count, 48);
    assert_eq!(
        file_sha256(&fixture.store().output_path(&id).unwrap()),
        manifest.output_sha256
    );
    assert_eq!(fixture.staging_entries(), 0);
    let attempts = fixture.store().list_attempts().unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].status, "MATERIALIZED");
}

#[test]
fn gate_feature_materialization_replay_is_idempotent_and_bit_identical() {
    let fixture = Fixture::new();
    let dataset = standard_dataset(&fixture, "replay");
    let first = fixture.prepare(&standard_request(&dataset)).unwrap();
    let second = fixture.prepare(&standard_request(&dataset)).unwrap();
    assert_eq!(
        first.manifest.feature_set_id,
        second.manifest.feature_set_id
    );
    assert_eq!(first.manifest.output_sha256, second.manifest.output_sha256);
    assert_eq!(
        first.manifest.output_fingerprint,
        second.manifest.output_fingerprint
    );
    let first_result = fixture.store().commit_prepared(first).unwrap();
    let second_result = fixture.store().commit_prepared(second).unwrap();
    assert_eq!(second_result.disposition, FeatureCommitDisposition::Reused);
    assert_eq!(first_result.feature_set_id, second_result.feature_set_id);
    assert_eq!(
        fixture
            .store()
            .list_feature_sets(&AtomicBool::new(false))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(fixture.staging_entries(), 0);
    let statuses: Vec<_> = fixture
        .store()
        .list_attempts()
        .unwrap()
        .into_iter()
        .map(|attempt| attempt.status)
        .collect();
    assert_eq!(statuses.iter().filter(|s| *s == "REUSED").count(), 1);
}

#[test]
fn gate_feature_materialization_changed_parameters_or_inputs_create_new_identities() {
    let fixture = Fixture::new();
    let dataset = standard_dataset(&fixture, "identity-a");
    let base = standard_request(&dataset);
    let base_id = fixture.prepare(&base).unwrap().manifest.feature_set_id;

    let mut changed_period = base.clone();
    changed_period.features[0].parameters = period(4);
    let mut changed_price = base.clone();
    changed_price.features[0].parameters =
        parameters(&[("price", ParameterValue::Choice("HIGH".into()))]);
    let mut changed_staleness = base.clone();
    changed_staleness.features[2] = reference_feature(
        "gbp_ema_4",
        "EMA",
        period(4),
        selector(&dataset, "GBPUSD.m", "H1"),
        7200,
        0,
    );
    let mut equivalent_defaults = base.clone();
    equivalent_defaults.features[1].parameters = parameters(&[
        ("period", ParameterValue::Integer(5)),
        ("price", ParameterValue::Choice("CLOSE".into())),
    ]);
    let mut ids = BTreeSet::from([base_id.clone()]);
    for changed in [&changed_period, &changed_price, &changed_staleness] {
        assert!(ids.insert(fixture.prepare(changed).unwrap().manifest.feature_set_id));
    }
    assert_eq!(
        fixture
            .prepare(&equivalent_defaults)
            .unwrap()
            .manifest
            .feature_set_id,
        base_id
    );

    let mut other_rows = series(
        "EURUSD.m",
        Timeframe::H1,
        "2024-01-01T00:00:00Z",
        48,
        0.0,
        false,
    );
    other_rows.extend(series(
        "EURUSD.m",
        Timeframe::H4,
        "2024-01-01T00:00:00Z",
        12,
        0.5,
        false,
    ));
    let mut gbp = series(
        "GBPUSD.m",
        Timeframe::H1,
        "2024-01-01T00:00:00Z",
        48,
        1.5,
        false,
    );
    scale_prices(&mut gbp[47..], 1.01);
    other_rows.extend(gbp);
    let other = fixture.import("identity-b", &other_rows, CalendarPolicy::Continuous);
    assert!(ids.insert(
        fixture
            .prepare(&standard_request(&other))
            .unwrap()
            .manifest
            .feature_set_id
    ));
}

#[test]
fn gate_feature_materialization_blocks_sets_with_an_unusable_feature() {
    let fixture = Fixture::new();
    let rows = series(
        "EURUSD.m",
        Timeframe::H1,
        "2024-01-01T00:00:00Z",
        10,
        0.0,
        false,
    );
    let dataset = fixture.import("short", &rows, CalendarPolicy::Continuous);
    let prepared = fixture
        .prepare(&request(
            selector(&dataset, "EURUSD.m", "H1"),
            vec![
                target_feature("sma_3", "SMA", period(3)),
                target_feature("sma_50", "SMA", period(50)),
            ],
        ))
        .unwrap();
    assert_eq!(prepared.manifest.status, FeatureSetStatus::Blocked);
    assert_eq!(prepared.manifest.feature_summaries[1].ready_rows, 0);
    let result = fixture.store().commit_prepared(prepared).unwrap();
    assert_eq!(result.status, FeatureSetStatus::Blocked);
}

// ---------------------------------------------------------------------------------------------
// resource_controls

#[test]
fn gate_resource_controls_enforce_row_and_output_quotas_before_registration() {
    let fixture = Fixture::new();
    let dataset = standard_dataset(&fixture, "quota");
    let mut target_rows = standard_request(&dataset);
    target_rows.quotas.max_target_rows = 47;
    assert!(matches!(
        fixture.prepare(&target_rows),
        Err(FeatureError::ResourceQuota(_))
    ));
    let mut input_rows = standard_request(&dataset);
    input_rows.quotas.max_input_rows = 107;
    assert!(matches!(
        fixture.prepare(&input_rows),
        Err(FeatureError::ResourceQuota(_))
    ));
    let mut output_bytes = standard_request(&dataset);
    output_bytes.quotas.max_output_bytes = 64;
    assert!(matches!(
        fixture.prepare(&output_bytes),
        Err(FeatureError::ResourceQuota(_))
    ));
    assert_eq!(fixture.staging_entries(), 0);
    assert!(fixture
        .store()
        .list_feature_sets(&AtomicBool::new(false))
        .unwrap()
        .is_empty());
}

#[test]
fn gate_resource_controls_cancellation_stops_work_and_leaves_no_stage() {
    let fixture = Fixture::new();
    let dataset = standard_dataset(&fixture, "cancel");
    let cancel = AtomicBool::new(true);
    let progress = FeatureProgress::default();
    assert!(matches!(
        fixture
            .store()
            .prepare(&standard_request(&dataset), &cancel, &progress),
        Err(FeatureError::Cancelled)
    ));
    assert_eq!(progress.snapshot().phase, ProgressPhase::VerifyingInputs);
    assert_eq!(fixture.staging_entries(), 0);
    assert!(fixture
        .store()
        .list_feature_sets(&AtomicBool::new(false))
        .unwrap()
        .is_empty());
}

#[test]
fn gate_resource_controls_large_series_streams_in_bounded_row_groups() {
    let fixture = Fixture::new();
    let rows = series(
        "EURUSD.m",
        Timeframe::M1,
        "2024-01-01T00:00:00Z",
        70_000,
        0.0,
        false,
    );
    let dataset = fixture.import("large", &rows, CalendarPolicy::Continuous);
    let progress = Arc::new(FeatureProgress::default());
    let prepared = fixture
        .store()
        .prepare(
            &request(
                selector(&dataset, "EURUSD.m", "M1"),
                vec![
                    target_feature("ema_50", "EMA", period(50)),
                    target_feature("atr_14", "ATR_WILDER", period(14)),
                ],
            ),
            &AtomicBool::new(false),
            &progress,
        )
        .unwrap();
    assert_eq!(prepared.manifest.row_count, 70_000);
    assert_eq!(prepared.preview.sample_rows.len(), PREVIEW_ROWS);
    assert_eq!(
        prepared
            .preview
            .sample_rows
            .last()
            .unwrap()
            .decision_time_utc,
        "2024-02-18T14:40:00.000000Z"
    );
    let table = read_output(&prepared.staging_directory.join("features.parquet"));
    assert!(table.row_group_rows.len() >= 2);
    assert!(table.row_group_rows.iter().all(|rows| *rows <= 65_536));
    assert_eq!(progress.snapshot().phase, ProgressPhase::Finalizing);
}

// ---------------------------------------------------------------------------------------------
// adversarial_no_lookahead

#[test]
fn gate_adversarial_no_lookahead_future_and_unfinished_bars_cannot_change_history() {
    let fixture = Fixture::new();
    let build = |perturb: bool| {
        let mut target = series(
            "EURUSD.m",
            Timeframe::H1,
            "2024-01-01T00:00:00Z",
            48,
            0.0,
            false,
        );
        let mut h4 = series(
            "EURUSD.m",
            Timeframe::H4,
            "2024-01-01T00:00:00Z",
            12,
            0.5,
            false,
        );
        let mut gbp = series(
            "GBPUSD.m",
            Timeframe::H1,
            "2024-01-01T00:00:00Z",
            48,
            1.5,
            false,
        );
        if perturb {
            // Target bars from 30:00, the H4 bar covering 28:00-32:00, and reference bars
            // closing after 30:00 all change.
            scale_prices(&mut target[30..], 1.07);
            scale_prices(&mut h4[7..], 0.93);
            scale_prices(&mut gbp[30..], 1.11);
        }
        target.extend(h4);
        target.extend(gbp);
        target
    };
    let baseline = fixture.import("baseline", &build(false), CalendarPolicy::Continuous);
    let perturbed = fixture.import("perturbed", &build(true), CalendarPolicy::Continuous);
    let features = |dataset: &str| {
        request(
            selector(dataset, "EURUSD.m", "H1"),
            vec![
                target_feature("sma_5", "SMA", period(5)),
                target_feature("rsi_4", "RSI_WILDER", period(4)),
                target_feature("don_6", "DONCHIAN", period(6)),
                reference_feature(
                    "h4_bb",
                    "BOLLINGER",
                    period(2),
                    selector(dataset, "EURUSD.m", "H4"),
                    8 * 3600,
                    0,
                ),
                reference_feature(
                    "gbp_roc",
                    "ROC",
                    period(3),
                    selector(dataset, "GBPUSD.m", "H1"),
                    3600,
                    0,
                ),
            ],
        )
    };
    let a = fixture.prepare(&features(&baseline)).unwrap();
    let b = fixture.prepare(&features(&perturbed)).unwrap();
    let table_a = read_output(&a.staging_directory.join("features.parquet"));
    let table_b = read_output(&b.staging_directory.join("features.parquet"));
    // Rows 0..=29 decide at or before 30:00; the H4 bar 28:00-32:00 is unfinished at rows 28 and 29.
    for row in 0..30 {
        assert_eq!(table_a.row(row), table_b.row(row), "row {row}");
    }
    assert_ne!(table_a.row(30), table_b.row(30));
    assert_ne!(a.manifest.feature_set_id, b.manifest.feature_set_id);
}

// ---------------------------------------------------------------------------------------------
// recovery_and_tamper

fn journal(fixture: &Fixture, prepared: &rg_feature_studio::store::PreparedFeatureSet) {
    let connection = open_database(fixture.root.join("research.sqlite3")).unwrap();
    let manifest = &prepared.manifest;
    save_pending_feature_set_promotion(
        &connection,
        &PendingFeatureSetPromotion {
            staging_name: prepared
                .staging_directory
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .into(),
            feature_set: FeatureSetMetadata {
                feature_set_id: manifest.feature_set_id.clone(),
                identity_sha256: manifest.identity_sha256.clone(),
                manifest_json: prepared.manifest_json.clone(),
                manifest_sha256: prepared.manifest_sha256.clone(),
                output_sha256: manifest.output_sha256.clone(),
                created_at_utc: manifest.created_at_utc.clone(),
                status: manifest.status.as_str().into(),
                manifest_integrity_valid: true,
            },
            inputs: manifest
                .identity
                .inputs
                .iter()
                .map(|input| FeatureSetInput {
                    dataset_id: input.dataset_id.clone(),
                    dataset_fingerprint: input.fingerprint.clone(),
                })
                .collect(),
            attempt: FeatureAttemptRecord {
                attempt_id: "crash-fixture-attempt".into(),
                started_at_utc: manifest.created_at_utc.clone(),
                completed_at_utc: manifest.created_at_utc.clone(),
                status: "MATERIALIZED".into(),
                feature_set_id: Some(manifest.feature_set_id.clone()),
                diagnostic_code: None,
                diagnostic_message: None,
            },
        },
    )
    .unwrap();
}

#[test]
fn gate_recovery_and_tamper_restart_completes_a_journaled_promotion_before_and_after_rename() {
    for renamed in [false, true] {
        let mut fixture = Fixture::new();
        let dataset = standard_dataset(&fixture, "crash");
        let prepared = fixture.prepare(&standard_request(&dataset)).unwrap();
        let id = prepared.manifest.feature_set_id.clone();
        journal(&fixture, &prepared);
        if renamed {
            std::fs::rename(
                &prepared.staging_directory,
                fixture.root.join("feature-sets").join(&id),
            )
            .unwrap();
        }
        drop(prepared);
        fixture.reopen().unwrap();
        let catalog = fixture
            .store()
            .list_feature_sets(&AtomicBool::new(false))
            .unwrap();
        assert_eq!(catalog.len(), 1, "renamed={renamed}");
        assert_eq!(catalog[0].feature_set_id, id);
        assert_eq!(catalog[0].status, FeatureSetStatus::Ready);
        assert_eq!(fixture.staging_entries(), 0);
        let replay = fixture.prepare(&standard_request(&dataset)).unwrap();
        assert_eq!(
            fixture.store().commit_prepared(replay).unwrap().disposition,
            FeatureCommitDisposition::Reused
        );
    }
}

#[test]
fn gate_recovery_and_tamper_recovery_fails_closed_and_preserves_tampered_evidence() {
    let mut fixture = Fixture::new();
    let dataset = standard_dataset(&fixture, "tampered-journal");
    let prepared = fixture.prepare(&standard_request(&dataset)).unwrap();
    journal(&fixture, &prepared);
    let output = prepared.staging_directory.join("features.parquet");
    std::fs::write(&output, b"partial write").unwrap();
    drop(prepared);
    assert!(matches!(
        fixture.reopen(),
        Err(FeatureError::RecoveryBlocked { .. })
    ));
    assert_eq!(std::fs::read(&output).unwrap(), b"partial write");
}

#[test]
fn gate_recovery_and_tamper_abandoned_stage_does_not_block_restart_or_new_commits() {
    let mut fixture = Fixture::new();
    let dataset = standard_dataset(&fixture, "abandoned");
    let abandoned = fixture.prepare(&standard_request(&dataset)).unwrap();
    let abandoned_path = abandoned.staging_directory.clone();
    drop(abandoned);
    fixture.reopen().unwrap();
    assert!(abandoned_path.is_dir());
    commit(&fixture, &standard_request(&dataset));
    assert_eq!(
        fixture
            .store()
            .list_feature_sets(&AtomicBool::new(false))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn gate_recovery_and_tamper_registered_output_or_manifest_tamper_is_quarantined() {
    for file_name in ["features.parquet", "manifest.json"] {
        let fixture = Fixture::new();
        let dataset = standard_dataset(&fixture, "registered-tamper");
        let id = commit(&fixture, &standard_request(&dataset));
        std::fs::write(
            fixture.root.join("feature-sets").join(&id).join(file_name),
            b"tampered",
        )
        .unwrap();
        let entry = fixture.store().inspect_feature_set(&id).unwrap().unwrap();
        assert_eq!(entry.status, FeatureSetStatus::Quarantined, "{file_name}");
        assert!(entry.diagnostic.is_some());
        let replay = fixture.prepare(&standard_request(&dataset)).unwrap();
        assert!(matches!(
            fixture.store().commit_prepared(replay),
            Err(FeatureError::ExistingFeatureSetCorrupt(_))
        ));
    }
}

#[test]
fn gate_recovery_and_tamper_tampered_preview_cannot_be_confirmed() {
    let fixture = Fixture::new();
    let dataset = standard_dataset(&fixture, "preview-tamper");
    let prepared = fixture.prepare(&standard_request(&dataset)).unwrap();
    std::fs::write(prepared.staging_directory.join("features.parquet"), b"swap").unwrap();
    assert!(matches!(
        fixture.store().commit_prepared(prepared),
        Err(FeatureError::InvalidPrepared(_))
    ));
    assert!(fixture
        .store()
        .list_feature_sets(&AtomicBool::new(false))
        .unwrap()
        .is_empty());
    let attempts = fixture.store().list_attempts().unwrap();
    assert_eq!(attempts[0].status, "FAILED");
    assert_eq!(
        attempts[0].diagnostic_code.as_deref(),
        Some("INVALID_PREPARED_FEATURE_SET")
    );

    let changed_input = fixture.prepare(&standard_request(&dataset)).unwrap();
    std::fs::write(fixture.dataset_dir(&dataset).join("bars.parquet"), b"swap").unwrap();
    assert!(matches!(
        fixture.store().commit_prepared(changed_input),
        Err(FeatureError::InputIntegrity { .. })
    ));
    assert!(fixture
        .store()
        .list_feature_sets(&AtomicBool::new(false))
        .unwrap()
        .is_empty());
}

#[test]
fn gate_recovery_and_tamper_discarded_preview_is_removed_and_recorded() {
    let fixture = Fixture::new();
    let dataset = standard_dataset(&fixture, "discard");
    let prepared = fixture.prepare(&standard_request(&dataset)).unwrap();
    let stage = prepared.staging_directory.clone();
    fixture.store().discard_prepared(prepared).unwrap();
    assert!(!stage.exists());
    assert_eq!(
        fixture.store().list_attempts().unwrap()[0].status,
        "CANCELLED"
    );
    assert!(fixture
        .store()
        .list_feature_sets(&AtomicBool::new(false))
        .unwrap()
        .is_empty());
}

// ---------------------------------------------------------------------------------------------
// data_policy

#[test]
fn gate_data_policy_fixture_labels_flow_into_lineage_and_no_edge_is_claimed() {
    let readme = include_str!("../fixtures/README.md");
    assert!(readme.contains("synthetic fixtures"));
    assert!(readme.contains("not historical market data"));
    assert!(readme.contains("not evidence of any edge"));
    let fixture = Fixture::new();
    let dataset = standard_dataset(&fixture, "labels");
    let prepared = fixture.prepare(&standard_request(&dataset)).unwrap();
    let lineage = &prepared.manifest.lineage[0];
    assert!(lineage.source_id.starts_with(FIXTURE_SOURCE_PREFIX));
    assert_eq!(lineage.broker, "SYNTHETIC-FIXTURE");
    assert!(prepared
        .manifest
        .research_notice
        .contains("not trading signals"));
    assert!(prepared
        .manifest
        .research_notice
        .contains("evidence of edge"));
}

#[test]
fn gate_input_authority_feature_store_opens_only_beside_an_existing_dataset_registry() {
    let directory = tempfile::TempDir::new().unwrap();
    let elsewhere = directory.path().join("unrelated");
    let datasets = Arc::new(
        rg_data_ground::store::DatasetStore::open(directory.path().join("app-data")).unwrap(),
    );
    assert!(matches!(
        FeatureStore::open(&elsewhere, datasets),
        Err(FeatureError::UnsafeStorePath)
    ));
}

// ---------------------------------------------------------------------------------------------
// Control Room audit repairs (PR #6 review 5579914800)

/// Second READY dataset: GBPUSD.m H1 (48 bars) only.
fn second_dataset(fixture: &Fixture, name: &str) -> String {
    let rows = series(
        "GBPUSD.m",
        Timeframe::H1,
        "2024-01-01T00:00:00Z",
        48,
        2.5,
        false,
    );
    fixture.import(name, &rows, CalendarPolicy::Continuous)
}

fn flip_middle_byte(path: &std::path::Path) {
    let mut bytes = std::fs::read(path).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x5a;
    std::fs::write(path, bytes).unwrap();
}

fn remove_file(path: &std::path::Path) {
    std::fs::remove_file(path).unwrap();
}

fn not_usable_status(result: Result<std::path::PathBuf, FeatureError>) -> Option<String> {
    match result {
        Err(error) if error.code() == "FEATURE_SET_NOT_USABLE" => {
            Some(error.to_string().rsplit(' ').next().unwrap().to_owned())
        }
        _ => None,
    }
}

fn assert_ready_and_consumable(fixture: &Fixture, id: &str) {
    let entry = fixture.store().inspect_feature_set(id).unwrap().unwrap();
    assert_eq!(entry.status, FeatureSetStatus::Ready);
    assert!(entry.integrity.lineage_valid);
    assert!(entry.diagnostic.is_none());
    let catalog = fixture
        .store()
        .list_feature_sets(&AtomicBool::new(false))
        .unwrap();
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].status, FeatureSetStatus::Ready);
    let output = fixture.store().output_path(id).unwrap();
    assert_eq!(
        output,
        std::fs::canonicalize(fixture.root.join("feature-sets"))
            .unwrap()
            .join(id)
            .join("features.parquet")
    );
    assert_eq!(
        file_sha256(&output),
        entry.manifest.as_ref().unwrap().output_sha256
    );
}

#[test]
fn gate_recovery_and_tamper_source_tamper_after_registration_revokes_readiness() {
    type Tamper = fn(&std::path::Path);
    let cases: [(&str, &str, Tamper); 5] = [
        ("bars.parquet", "modified canonical bars", flip_middle_byte),
        ("bars.parquet", "missing canonical bars", remove_file),
        ("raw-source.csv", "modified raw source", flip_middle_byte),
        ("raw-source.csv", "missing raw source", remove_file),
        (
            "manifest.json",
            "modified dataset manifest",
            flip_middle_byte,
        ),
    ];
    for (file_name, label, tamper) in cases {
        let mut fixture = Fixture::new();
        let dataset = standard_dataset(&fixture, "post-registration-tamper");
        let id = commit(&fixture, &standard_request(&dataset));
        assert_ready_and_consumable(&fixture, &id);

        tamper(&fixture.dataset_dir(&dataset).join(file_name));

        let entry = fixture.store().inspect_feature_set(&id).unwrap().unwrap();
        assert_eq!(entry.status, FeatureSetStatus::Quarantined, "{label}");
        assert!(!entry.integrity.lineage_valid, "{label}");
        // The feature set's own manifest and output are untouched: only source lineage fails.
        assert!(entry.integrity.manifest_valid, "{label}");
        assert!(entry.integrity.output_valid, "{label}");
        assert!(
            entry.diagnostic.as_deref().unwrap_or("").contains(&dataset),
            "{label}"
        );
        let catalog = fixture
            .store()
            .list_feature_sets(&AtomicBool::new(false))
            .unwrap();
        assert_eq!(catalog[0].status, FeatureSetStatus::Quarantined, "{label}");
        assert!(!catalog[0].integrity.lineage_valid, "{label}");
        assert_eq!(
            not_usable_status(fixture.store().output_path(&id)).as_deref(),
            Some("QUARANTINED"),
            "{label}"
        );
        assert!(
            matches!(
                fixture.prepare(&standard_request(&dataset)),
                Err(FeatureError::InputIntegrity { .. })
            ),
            "{label}"
        );
        // Readiness stays revoked across a registry restart.
        fixture.reopen().unwrap();
        let entry = fixture.store().inspect_feature_set(&id).unwrap().unwrap();
        assert_eq!(entry.status, FeatureSetStatus::Quarantined, "{label}");
    }
}

#[test]
fn gate_recovery_and_tamper_unchanged_source_keeps_registered_set_ready_and_consumable() {
    let mut fixture = Fixture::new();
    let dataset = standard_dataset(&fixture, "unchanged-source");
    let id = commit(&fixture, &standard_request(&dataset));
    let bars = fixture.dataset_dir(&dataset).join("bars.parquet");
    let before = file_sha256(&bars);
    assert_ready_and_consumable(&fixture, &id);
    assert_ready_and_consumable(&fixture, &id);
    fixture.reopen().unwrap();
    assert_ready_and_consumable(&fixture, &id);
    assert_eq!(file_sha256(&bars), before);
}

#[test]
fn gate_feature_materialization_only_ready_feature_sets_cross_the_downstream_boundary() {
    let fixture = Fixture::new();
    let dataset = standard_dataset(&fixture, "blocked-boundary");
    let prepared = fixture
        .prepare(&request(
            selector(&dataset, "EURUSD.m", "H1"),
            vec![
                target_feature("sma_3", "SMA", period(3)),
                target_feature("sma_100", "SMA", period(100)),
            ],
        ))
        .unwrap();
    assert_eq!(prepared.manifest.status, FeatureSetStatus::Blocked);
    let id = fixture
        .store()
        .commit_prepared(prepared)
        .unwrap()
        .feature_set_id;
    assert_eq!(
        fixture
            .store()
            .inspect_feature_set(&id)
            .unwrap()
            .unwrap()
            .status,
        FeatureSetStatus::Blocked
    );
    assert_eq!(
        not_usable_status(fixture.store().output_path(&id)).as_deref(),
        Some("BLOCKED")
    );
    let unknown = format!("fs_{}", "0".repeat(64));
    assert_eq!(
        not_usable_status(fixture.store().output_path(&unknown)).as_deref(),
        Some("NOT_REGISTERED")
    );
}

#[test]
fn gate_recovery_and_tamper_catalogue_source_checks_do_not_collide_with_input_listing() {
    let fixture = Fixture::new();
    let dataset = standard_dataset(&fixture, "concurrent-verification");
    let id = commit(&fixture, &standard_request(&dataset));
    std::thread::scope(|scope| {
        let listing = scope.spawn(|| {
            for _ in 0..20 {
                let candidates = fixture
                    .store()
                    .list_input_candidates(&AtomicBool::new(false))
                    .unwrap();
                assert!(candidates[0].eligible);
            }
        });
        for _ in 0..20 {
            let entry = fixture.store().inspect_feature_set(&id).unwrap().unwrap();
            assert_eq!(entry.status, FeatureSetStatus::Ready);
            let catalog = fixture
                .store()
                .list_feature_sets(&AtomicBool::new(false))
                .unwrap();
            assert_eq!(catalog[0].status, FeatureSetStatus::Ready);
        }
        listing.join().unwrap();
    });
}

#[test]
fn gate_resource_controls_catalogue_source_hashing_honours_cancellation() {
    let fixture = Fixture::new();
    let dataset = standard_dataset(&fixture, "cancel-source-hash");
    commit(&fixture, &standard_request(&dataset));
    // A large raw source keeps Data Ground hashing long enough to cancel it mid-file.
    std::fs::OpenOptions::new()
        .write(true)
        .open(fixture.dataset_dir(&dataset).join("raw-source.csv"))
        .unwrap()
        .set_len(128 * 1024 * 1024)
        .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&cancel);
    let cancellation = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(5));
        signal.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let result = fixture.store().list_feature_sets(&cancel);
    cancellation.join().unwrap();
    assert!(matches!(result, Err(FeatureError::Cancelled)));
}

#[test]
fn gate_resource_controls_input_row_quota_is_aggregate_across_unique_datasets() {
    let fixture = Fixture::new();
    // 108 bars: EURUSD.m H1 48 + EURUSD.m H4 12 + GBPUSD.m H1 48.
    let first = standard_dataset(&fixture, "aggregate-first");
    // 48 bars.
    let second = second_dataset(&fixture, "aggregate-second");

    // Single input: the quota bounds that dataset's bars.
    let mut single = standard_request(&first);
    single.quotas.max_input_rows = 108;
    let prepared = fixture.prepare(&single).unwrap();
    fixture.store().discard_prepared(prepared).unwrap();
    single.quotas.max_input_rows = 107;
    assert!(matches!(
        fixture.prepare(&single),
        Err(FeatureError::ResourceQuota(ref message)) if message.contains("108")
    ));

    // Two physical datasets, each referenced more than once: 108 + 48 = 156 aggregate rows.
    let multi = |max_input_rows: u64| {
        let mut request = request(
            selector(&first, "EURUSD.m", "H1"),
            vec![
                target_feature("sma_3", "SMA", period(3)),
                reference_feature(
                    "own_gbp",
                    "EMA",
                    period(4),
                    selector(&first, "GBPUSD.m", "H1"),
                    3600,
                    0,
                ),
                reference_feature(
                    "second_gbp_ema",
                    "EMA",
                    period(4),
                    selector(&second, "GBPUSD.m", "H1"),
                    3600,
                    0,
                ),
                reference_feature(
                    "second_gbp_sma",
                    "SMA",
                    period(3),
                    selector(&second, "GBPUSD.m", "H1"),
                    3600,
                    0,
                ),
            ],
        );
        request.quotas.max_input_rows = max_input_rows;
        request
    };
    for below_or_equal in [157, 156] {
        let prepared = fixture.prepare(&multi(below_or_equal)).unwrap();
        assert_eq!(prepared.manifest.identity.inputs.len(), 2);
        fixture.store().discard_prepared(prepared).unwrap();
    }
    for above in [155, 109, 108] {
        assert!(
            matches!(
                fixture.prepare(&multi(above)),
                Err(FeatureError::ResourceQuota(ref message)) if message.contains("156")
            ),
            "max_input_rows {above}"
        );
    }
    // The target-row quota is still enforced on its own.
    let mut target = multi(156);
    target.quotas.max_target_rows = 47;
    assert!(matches!(
        fixture.prepare(&target),
        Err(FeatureError::ResourceQuota(ref message)) if message.contains("target")
    ));
    assert_eq!(fixture.staging_entries(), 0);
}
