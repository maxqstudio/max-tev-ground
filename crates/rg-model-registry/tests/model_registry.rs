//! RG-R04 gate tests over registered synthetic inputs, labeled fixture ONNX models, and a loopback
//! mock Ollama runtime. No real model runtime, network, or market data is used.

mod support;

use std::sync::atomic::AtomicBool;

use rg_model_registry::{
    compare::ComparisonRequest,
    fixtures::{
        ollama::{
            MockBehavior, MockModel, MockOllama, FIXTURE_DIGEST, FIXTURE_DIGEST_CHANGED,
            FIXTURE_MODEL,
        },
        onnx,
    },
    store::{Determinism, InferenceDisposition, RegistrationDisposition, RuntimeAvailability},
    InferenceProgress, InferenceRecordStatus, InputColumn, ModelError, ModelRegistrationRequest,
    ModelStatus, RuntimeConfig,
};
use rg_persistence::{
    save_pending_model_lab_promotion, ModelLabAttemptRecord, ModelLabRecordKind,
    ModelLabRecordMetadata, PendingModelLabPromotion,
};
use support::*;

fn never() -> AtomicBool {
    AtomicBool::new(false)
}

/// y = fast - slow, bounded generously.
fn spread_model() -> Vec<u8> {
    onnx::linear_model(2, &[1.0, -1.0], &[0.0])
}

fn spread_outputs() -> Vec<rg_model_registry::OutputField> {
    vec![float_output("spread", -10.0, 10.0)]
}

fn register_spread(fixture: &Fixture) -> String {
    fixture.register_onnx(
        "spread.onnx",
        &spread_model(),
        &["fast", "slow"],
        spread_outputs(),
    )
}

fn spread_request(model_id: &str, feature_set_id: &str) -> rg_model_registry::InferenceRequest {
    feature_request(
        model_id,
        feature_set_id,
        &[("fast", feature("fast")), ("slow", feature("slow"))],
    )
}

// ---------------------------------------------------------------- model_discovery

#[test]
fn gate_model_discovery_absent_empty_duplicate_and_malformed_runtimes_register_nothing() {
    let fixture = Fixture::new();
    // Absent runtime: a port nobody listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    fixture
        .lab()
        .set_ollama_endpoint(&format!("http://127.0.0.1:{port}"))
        .unwrap();
    let absent = fixture.lab().discover_ollama().unwrap();
    assert_eq!(absent.availability, RuntimeAvailability::Unavailable);
    assert!(absent.candidates.is_empty());
    assert!(absent.diagnostic.is_some());

    let mock = MockOllama::start(MockBehavior::Normal);
    fixture
        .lab()
        .set_ollama_endpoint(&mock.endpoint().to_string())
        .unwrap();
    mock.set_models(vec![]);
    let empty = fixture.lab().discover_ollama().unwrap();
    assert_eq!(empty.availability, RuntimeAvailability::Available);
    assert!(empty.candidates.is_empty());

    mock.set_models(vec![
        MockModel {
            name: FIXTURE_MODEL.into(),
            digest: FIXTURE_DIGEST.into(),
        },
        MockModel {
            name: FIXTURE_MODEL.into(),
            digest: FIXTURE_DIGEST_CHANGED.into(),
        },
    ]);
    let duplicate = fixture.lab().discover_ollama().unwrap();
    assert_eq!(duplicate.candidates.len(), 2);
    assert!(duplicate
        .candidates
        .iter()
        .all(|candidate| !candidate.eligible));

    mock.set_behavior(MockBehavior::MalformedListing);
    let malformed = fixture.lab().discover_ollama().unwrap();
    assert_eq!(malformed.availability, RuntimeAvailability::Malformed);
    assert!(malformed.candidates.is_empty());

    // Discovery alone registered nothing and ran nothing.
    assert_eq!(fixture.registry_rows("model_registrations"), 0);
    assert_eq!(fixture.registry_rows("inference_records"), 0);
    assert_eq!(mock.generations(), 0);
    assert!(mock
        .requests()
        .iter()
        .all(|request| request == "GET /api/tags"));
}

#[test]
fn gate_model_discovery_changed_listing_is_reported_and_blocks_stale_registration() {
    let fixture = Fixture::new();
    let mock = MockOllama::start(MockBehavior::Normal);
    fixture
        .lab()
        .set_ollama_endpoint(&mock.endpoint().to_string())
        .unwrap();
    let first = fixture.lab().discover_ollama().unwrap();
    assert_eq!(first.candidates.len(), 1);
    assert!(first.candidates[0].eligible);
    assert_eq!(first.candidates[0].content_digest, FIXTURE_DIGEST);
    let request = ollama_registration(7, vec![category_output("direction", &["UP", "DOWN"])]);
    let preview = fixture.lab().preview_registration(&request).unwrap();

    // The runtime's content changes between review and confirmation.
    mock.set_models(vec![MockModel {
        name: FIXTURE_MODEL.into(),
        digest: FIXTURE_DIGEST_CHANGED.into(),
    }]);
    let changed = fixture.lab().discover_ollama().unwrap();
    assert_eq!(changed.candidates[0].content_digest, FIXTURE_DIGEST_CHANGED);
    assert!(matches!(
        fixture.lab().register_model(&request, &preview.model_id),
        Err(ModelError::CandidateChanged { .. })
    ));
    assert_eq!(fixture.registry_rows("model_registrations"), 0);
}

#[test]
fn gate_model_discovery_onnx_candidates_are_private_copies_that_register_nothing() {
    let fixture = Fixture::new();
    let path = fixture.write_file("spread.onnx", &spread_model());
    let candidate = fixture.lab().stage_onnx_candidate(&path, &never()).unwrap();
    assert!(candidate.eligible);
    assert_eq!(candidate.onnx.as_ref().unwrap().input.width, 2);
    assert!(candidate.registered_model_ids.is_empty());

    // Changing the original after selection does not change the candidate.
    std::fs::write(&path, onnx::classifier_model(2)).unwrap();
    let listed = fixture.lab().list_onnx_candidates().unwrap();
    assert_eq!(listed, vec![candidate.clone()]);

    let unsupported = fixture.write_file("custom.onnx", &onnx::external_data_model());
    let refused = fixture
        .lab()
        .stage_onnx_candidate(&unsupported, &never())
        .unwrap();
    assert!(!refused.eligible);
    assert!(refused.reason.unwrap().contains("external"));
    let not_onnx = fixture.write_file("model.bin", &spread_model());
    assert!(fixture
        .lab()
        .stage_onnx_candidate(&not_onnx, &never())
        .is_err());

    assert_eq!(fixture.registry_rows("model_registrations"), 0);
    assert_eq!(fixture.registry_rows("model_lab_attempts"), 0);

    // Candidates are session-scoped: a restart discards them, never registers them.
    let mut fixture = fixture;
    fixture.reopen().unwrap();
    assert!(fixture.lab().list_onnx_candidates().unwrap().is_empty());
    assert_eq!(fixture.registry_rows("model_registrations"), 0);
}

// ---------------------------------------------------------------- immutable_model_identity

#[test]
fn gate_immutable_model_identity_ids_are_deterministic_and_change_with_any_bound_property() {
    let fixture = Fixture::new();
    let path = fixture.write_file("spread.onnx", &spread_model());
    let candidate = fixture.lab().stage_onnx_candidate(&path, &never()).unwrap();
    let request = onnx_registration(&candidate.candidate_id, &["fast", "slow"], spread_outputs());
    let first = fixture.lab().preview_registration(&request).unwrap();
    let second = fixture.lab().preview_registration(&request).unwrap();
    assert_eq!(first.model_id, second.model_id);
    assert!(!first.already_registered);

    let mut renamed = request.clone();
    renamed.input_fields = vec!["fast".into(), "slower".into()];
    let mut rebounded = request.clone();
    rebounded.outputs = vec![float_output("spread", -1.0, 1.0)];
    let other_path = fixture.write_file(
        "spread-v2.onnx",
        &onnx::linear_model(2, &[1.0, -0.5], &[0.0]),
    );
    let other = fixture
        .lab()
        .stage_onnx_candidate(&other_path, &never())
        .unwrap();
    let redigested = onnx_registration(&other.candidate_id, &["fast", "slow"], spread_outputs());
    let ids = [
        first.model_id.clone(),
        fixture
            .lab()
            .preview_registration(&renamed)
            .unwrap()
            .model_id,
        fixture
            .lab()
            .preview_registration(&rebounded)
            .unwrap()
            .model_id,
        fixture
            .lab()
            .preview_registration(&redigested)
            .unwrap()
            .model_id,
    ];
    let unique = ids.iter().collect::<std::collections::BTreeSet<_>>();
    assert_eq!(unique.len(), ids.len(), "{ids:?}");

    let mock = MockOllama::start(MockBehavior::Normal);
    fixture
        .lab()
        .set_ollama_endpoint(&mock.endpoint().to_string())
        .unwrap();
    let seed_a = fixture
        .lab()
        .preview_registration(&ollama_registration(
            1,
            vec![category_output("direction", &["UP", "DOWN"])],
        ))
        .unwrap();
    let seed_b = fixture
        .lab()
        .preview_registration(&ollama_registration(
            2,
            vec![category_output("direction", &["UP", "DOWN"])],
        ))
        .unwrap();
    assert_ne!(
        seed_a.model_id, seed_b.model_id,
        "runtime configuration is part of identity"
    );

    // Registration requires the confirmed identity, and a repeat registration is idempotent.
    assert!(matches!(
        fixture.lab().register_model(&request, &seed_a.model_id),
        Err(ModelError::CandidateChanged { .. })
    ));
    let registered = fixture
        .lab()
        .register_model(&request, &first.model_id)
        .unwrap();
    assert_eq!(registered.disposition, RegistrationDisposition::Registered);
    let again = fixture
        .lab()
        .register_model(&request, &first.model_id)
        .unwrap();
    assert_eq!(
        again.disposition,
        RegistrationDisposition::AlreadyRegistered
    );
    assert_eq!(fixture.registry_rows("model_registrations"), 1);
    assert!(
        fixture
            .lab()
            .preview_registration(&request)
            .unwrap()
            .already_registered
    );
}

#[test]
fn gate_immutable_model_identity_changed_runtime_digest_quarantines_and_is_never_reidentified() {
    let fixture = Fixture::new();
    let mock = MockOllama::start(MockBehavior::Normal);
    let model_id = fixture.register_ollama(
        &mock,
        3,
        vec![category_output("direction", &["UP", "DOWN"])],
    );
    let ready = fixture.lab().inspect_model(&model_id).unwrap().unwrap();
    assert_eq!(ready.status, ModelStatus::Ready);

    mock.set_models(vec![MockModel {
        name: FIXTURE_MODEL.into(),
        digest: FIXTURE_DIGEST_CHANGED.into(),
    }]);
    let drifted = fixture.lab().inspect_model(&model_id).unwrap().unwrap();
    assert_eq!(drifted.status, ModelStatus::Quarantined);
    let diagnostic = drifted.diagnostic.unwrap();
    assert!(
        diagnostic.contains(&model_id) && diagnostic.contains(FIXTURE_MODEL),
        "{diagnostic}"
    );
    assert!(diagnostic.contains(FIXTURE_DIGEST_CHANGED));
    // The registration itself is unchanged: it still names the registered digest.
    assert_eq!(
        drifted.manifest.unwrap().identity.content_digest,
        FIXTURE_DIGEST
    );
    assert_eq!(fixture.registry_rows("model_registrations"), 1);

    mock.set_models(vec![]);
    let missing = fixture.lab().inspect_model(&model_id).unwrap().unwrap();
    assert_eq!(missing.status, ModelStatus::Quarantined);
    assert!(missing.diagnostic.unwrap().contains("no longer installed"));
}

#[test]
fn gate_immutable_model_identity_registration_survives_restart_and_tampered_file_quarantines() {
    let mut fixture = Fixture::new();
    let model_id = register_spread(&fixture);
    fixture.reopen().unwrap();
    let models = fixture.lab().list_models(&never()).unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].status, ModelStatus::Ready);

    let file = fixture.model_dir(&model_id).join("model.onnx");
    let mut bytes = std::fs::read(&file).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    std::fs::write(&file, bytes).unwrap();
    let tampered = fixture.lab().inspect_model(&model_id).unwrap().unwrap();
    assert_eq!(tampered.status, ModelStatus::Quarantined);
    assert!(!tampered.integrity.content_valid);
    assert!(tampered.diagnostic.unwrap().contains(&model_id));
}

// ---------------------------------------------------------------- typed_runtime_adapters

#[test]
fn gate_typed_runtime_adapters_ollama_schema_mismatches_are_recorded_as_invalid_output() {
    let fixture = Fixture::new();
    let feature_set = fixture.standard_feature_set();
    let mock = MockOllama::start(MockBehavior::Normal);
    let outputs = vec![
        category_output("direction", &["UP", "DOWN"]),
        float_output("confidence", -1.0, 1.0),
    ];
    let model_id = fixture.register_ollama(&mock, 11, outputs);
    let request = feature_request(
        &model_id,
        &feature_set,
        &[("fast", feature("fast")), ("slow", feature("slow"))],
    );
    let ok = fixture.prepare(&request).unwrap();
    let summary = &ok.manifest.summary;
    assert!(summary.ok_rows > 0);
    assert_eq!(summary.invalid_output_rows, 0);
    assert_eq!(summary.skipped_rows, 7, "SMA(8) warms up for seven rows");
    assert_eq!(mock.generations(), summary.ok_rows);
    fixture.lab().discard_inference(ok).unwrap();

    for (behavior, reason) in [
        (MockBehavior::UnknownOutputField, "SCHEMA_MISMATCH"),
        (MockBehavior::MalformedOutput, "SCHEMA_MISMATCH"),
        (MockBehavior::OutOfRangeOutput, "OUT_OF_RANGE"),
    ] {
        mock.set_behavior(behavior);
        let prepared = fixture.prepare(&request).unwrap();
        let summary = &prepared.manifest.summary;
        assert_eq!(summary.ok_rows, 0);
        assert_eq!(summary.invalid_output_rows, 53);
        assert_eq!(summary.reasons.get(reason), Some(&53));
        let statuses = text_values(
            &prepared.staging_directory.join("inference.parquet"),
            "row_status",
        );
        assert!(statuses
            .iter()
            .all(|status| status.as_deref() != Some("OK")));
        fixture.lab().discard_inference(prepared).unwrap();
    }
}

#[test]
fn gate_typed_runtime_adapters_onnx_non_finite_outputs_are_invalid_never_values() {
    let fixture = Fixture::new();
    let dataset = fixture.import("standard", &bars(30, 0.0));
    let feature_set = fixture.feature_set(&dataset);
    // A model whose output is always 1 / (x * 0) = inf: never recorded as a value.
    let zero = onnx::FixtureGraph::new(1)
        .floats("one", &[1], &[1.0])
        .floats("zero", &[1, 1], &[0.0])
        .node(onnx::FixtureNode::new("Mul", &["input", "zero"], "nothing"))
        .node(onnx::FixtureNode::new("Div", &["one", "nothing"], "output"))
        .encode(&[1, 1]);
    let infinite = fixture.register_onnx(
        "infinite.onnx",
        &zero,
        &["x"],
        vec![float_output("inverse", -1.0, 1.0)],
    );
    let prepared = fixture
        .prepare(&feature_request(
            &infinite,
            &feature_set,
            &[("x", feature("roc"))],
        ))
        .unwrap();
    let summary = &prepared.manifest.summary;
    assert_eq!(summary.ok_rows, 0);
    assert_eq!(
        summary.reasons.get("NON_FINITE_OUTPUT"),
        Some(&summary.invalid_output_rows)
    );
    assert!(summary.invalid_output_rows > 0);
    let values = output_values(
        &prepared.staging_directory.join("inference.parquet"),
        "inverse",
    );
    assert!(values.iter().all(Option::is_none));
}

#[test]
fn gate_typed_runtime_adapters_registration_checks_onnx_schema_against_the_model() {
    let fixture = Fixture::new();
    let path = fixture.write_file("spread.onnx", &spread_model());
    let candidate = fixture.lab().stage_onnx_candidate(&path, &never()).unwrap();
    for request in [
        onnx_registration(&candidate.candidate_id, &["fast"], spread_outputs()),
        onnx_registration(
            &candidate.candidate_id,
            &["fast", "slow"],
            vec![float_output("a", 0.0, 1.0), float_output("b", 0.0, 1.0)],
        ),
        onnx_registration(
            &candidate.candidate_id,
            &["fast", "slow"],
            vec![category_output("direction", &["UP", "DOWN"])],
        ),
        ModelRegistrationRequest {
            runtime: RuntimeConfig::OllamaLoopback {
                seed: 1,
                max_output_tokens: 64,
                context_tokens: 2048,
                instruction: "x".into(),
            },
            ..onnx_registration(&candidate.candidate_id, &["fast", "slow"], spread_outputs())
        },
    ] {
        assert!(
            fixture.lab().preview_registration(&request).is_err(),
            "{request:?}"
        );
    }
    assert_eq!(fixture.registry_rows("model_registrations"), 0);
}

// ---------------------------------------------------------------- verified_model_input

#[test]
fn gate_verified_model_input_consumes_feature_and_decision_sets_with_hand_checked_values() {
    let fixture = Fixture::new();
    let feature_set = fixture.standard_feature_set();
    let decision_set = fixture.decision_set(&feature_set);
    let model_id = register_spread(&fixture);

    let result = fixture.infer(&spread_request(&model_id, &feature_set));
    assert_eq!(result.disposition, InferenceDisposition::Materialized);
    let record = fixture
        .lab()
        .inspect_inference_record(&result.record_id)
        .unwrap()
        .unwrap();
    assert_eq!(record.status, InferenceRecordStatus::Ready);
    let manifest = record.manifest.unwrap();
    assert_eq!(manifest.row_count, 60);
    assert_eq!(manifest.summary.skipped_rows, 7);
    assert_eq!(manifest.summary.reasons.get("WARMUP"), Some(&7));
    // Each OK output equals f32(fast) - f32(slow) of the same row.
    let output = fixture
        .record_dir(&result.record_id)
        .join("inference.parquet");
    let spreads = output_values(&output, "spread");
    let fast = float_column(&output, "input__fast");
    let slow = float_column(&output, "input__slow");
    for ((spread, fast), slow) in spreads.iter().zip(&fast).zip(&slow) {
        match (spread, fast, slow) {
            (Some(spread), Some(fast), Some(slow)) => {
                assert_eq!(*spread, f64::from(*fast as f32 - *slow as f32));
            }
            (None, _, _) => {}
            other => panic!("unexpected row {other:?}"),
        }
    }

    let decision_model = fixture.register_onnx(
        "decision.onnx",
        &onnx::linear_model(2, &[1.0, 1.0], &[0.0]),
        &["decision", "signal"],
        vec![float_output("combined", -1000.0, 1000.0)],
    );
    let decision_result = fixture.infer(&decision_request(
        &decision_model,
        &decision_set,
        &[
            ("decision", InputColumn::Decision {}),
            (
                "signal",
                InputColumn::DecisionInput {
                    role: "signal".into(),
                },
            ),
        ],
    ));
    let record = fixture
        .lab()
        .inspect_inference_record(&decision_result.record_id)
        .unwrap()
        .unwrap();
    assert_eq!(record.status, InferenceRecordStatus::Ready);
    let manifest = record.manifest.unwrap();
    assert_eq!(manifest.lineage.feature_set_id, feature_set);
    assert_eq!(manifest.identity.input.id, decision_set);
}

#[test]
fn gate_verified_model_input_rejects_unregistered_mismatched_and_tampered_inputs() {
    let fixture = Fixture::new();
    let feature_set = fixture.standard_feature_set();
    let decision_set = fixture.decision_set(&feature_set);
    let model_id = register_spread(&fixture);

    let missing = format!("fs_{}", "0".repeat(64));
    assert!(matches!(
        fixture.prepare(&spread_request(&model_id, &missing)),
        Err(ModelError::InputMissing(_))
    ));
    assert!(matches!(
        fixture.prepare(&spread_request(&model_id, "C:/data/features.parquet")),
        Err(ModelError::InvalidRequest(_))
    ));
    // Decision-set bindings on a feature set, and unknown fields, are refused.
    assert!(fixture
        .prepare(&feature_request(
            &model_id,
            &feature_set,
            &[
                ("fast", InputColumn::Decision {}),
                ("slow", feature("slow"))
            ]
        ))
        .is_err());
    assert!(fixture
        .prepare(&feature_request(
            &model_id,
            &feature_set,
            &[
                ("fast", feature("fast")),
                ("slow", feature("slow")),
                ("extra", feature("roc"))
            ]
        ))
        .is_err());
    assert!(fixture
        .prepare(&feature_request(
            &model_id,
            &feature_set,
            &[("fast", feature("fast"))]
        ))
        .is_err());
    assert!(fixture
        .prepare(&decision_request(
            &model_id,
            &decision_set,
            &[("fast", feature("fast")), ("slow", feature("slow"))]
        ))
        .is_err());

    // Tampering with the feature output quarantines it upstream; inference refuses it.
    let output = fixture.feature_output(&feature_set);
    let mut bytes = std::fs::read(&output).unwrap();
    bytes[100] ^= 0x10;
    std::fs::write(&output, bytes).unwrap();
    assert!(matches!(
        fixture.prepare(&spread_request(&model_id, &feature_set)),
        Err(ModelError::InputNotUsable { .. })
    ));
    assert!(matches!(
        fixture.prepare(&decision_request(
            &model_id,
            &decision_set,
            &[
                ("fast", InputColumn::Decision {}),
                ("slow", InputColumn::Decision {})
            ]
        )),
        Err(ModelError::InputNotUsable { .. }) | Err(ModelError::Strategy(_))
    ));
    assert_eq!(fixture.registry_rows("inference_records"), 0);
    assert_eq!(fixture.staging_entries(), 0);
}

#[test]
fn gate_verified_model_input_input_changed_after_preview_is_not_committed() {
    let fixture = Fixture::new();
    let feature_set = fixture.standard_feature_set();
    let model_id = register_spread(&fixture);
    let prepared = fixture
        .prepare(&spread_request(&model_id, &feature_set))
        .unwrap();
    let output = fixture.feature_output(&feature_set);
    let mut bytes = std::fs::read(&output).unwrap();
    bytes[120] ^= 0x01;
    std::fs::write(&output, bytes).unwrap();
    assert!(matches!(
        fixture.lab().commit_inference(prepared),
        Err(ModelError::InputIntegrity { .. })
    ));
    assert_eq!(fixture.registry_rows("inference_records"), 0);
    assert_eq!(fixture.staging_entries(), 0);
}

#[test]
fn gate_verified_model_input_future_rows_cannot_change_earlier_inputs_or_outputs() {
    let fixture = Fixture::new();
    let base = bars(60, 0.0);
    let mut perturbed = base.clone();
    perturb_from(&mut perturbed, 40, 1.07);
    let base_set = fixture.feature_set(&fixture.import("base", &base));
    let perturbed_set = fixture.feature_set(&fixture.import("perturbed", &perturbed));
    let model_id = register_spread(&fixture);
    let first = fixture.infer(&spread_request(&model_id, &base_set));
    let second = fixture.infer(&spread_request(&model_id, &perturbed_set));
    assert_ne!(
        first.record_id, second.record_id,
        "different inputs give different identities"
    );
    let a = fixture
        .record_dir(&first.record_id)
        .join("inference.parquet");
    let b = fixture
        .record_dir(&second.record_id)
        .join("inference.parquet");
    let (spread_a, spread_b) = (output_values(&a, "spread"), output_values(&b, "spread"));
    assert_eq!(
        spread_a[..40],
        spread_b[..40],
        "rows before the perturbation are identical"
    );
    assert_ne!(
        spread_a[40..],
        spread_b[40..],
        "the perturbation is visible afterwards"
    );
    let fast_a = float_column(&a, "input__fast");
    let fast_b = float_column(&b, "input__fast");
    assert_eq!(fast_a[..40], fast_b[..40]);
}

// ---------------------------------------------------------------- model_authority_boundary

#[test]
fn gate_model_authority_boundary_decision_sets_stay_byte_identical_and_other_authorities_are_refused(
) {
    let fixture = Fixture::new();
    let feature_set = fixture.standard_feature_set();
    let decision_set = fixture.decision_set(&feature_set);
    let before = snapshot(&fixture.decision_dir(&decision_set));
    let feature_before = snapshot(&fixture.root.join("feature-sets").join(&feature_set));

    let model_a = register_spread(&fixture);
    let model_b = fixture.register_onnx(
        "half.onnx",
        &onnx::linear_model(2, &[0.5, -0.5], &[0.0]),
        &["fast", "slow"],
        spread_outputs(),
    );
    let bindings = [
        (
            "fast",
            InputColumn::DecisionInput {
                role: "signal".into(),
            },
        ),
        ("slow", InputColumn::Decision {}),
    ];
    let a = fixture.infer(&decision_request(&model_a, &decision_set, &bindings));
    let b = fixture.infer(&decision_request(&model_b, &decision_set, &bindings));
    fixture
        .lab()
        .compare(
            &ComparisonRequest {
                record_ids: vec![a.record_id.clone(), b.record_id.clone()],
            },
            &never(),
        )
        .unwrap();
    assert_eq!(snapshot(&fixture.decision_dir(&decision_set)), before);
    assert_eq!(
        snapshot(&fixture.root.join("feature-sets").join(&feature_set)),
        feature_before
    );
    let decision = fixture
        .strategies
        .inspect_decision_set(&decision_set)
        .unwrap()
        .unwrap();
    assert_eq!(
        decision.status,
        rg_strategy_engine::DecisionSetStatus::Ready
    );

    for authority in [
        "DECISION_FILTER",
        "DECISION_VETO",
        "DECISION_OVERRIDE",
        "POSITION_SIZING",
        "ORDER_GENERATION",
    ] {
        let mut request = spread_request(&model_a, &feature_set);
        request.authority = authority.into();
        assert!(
            matches!(
                fixture.prepare(&request),
                Err(ModelError::AuthorityRefused(_))
            ),
            "{authority}"
        );
    }
    let record = fixture
        .lab()
        .inspect_inference_record(&a.record_id)
        .unwrap()
        .unwrap()
        .manifest
        .unwrap();
    assert_eq!(record.authority, rg_model_registry::Authority::AdvisoryOnly);
    assert!(record
        .research_notice
        .contains("never create, alter, veto, or override"));
}

#[test]
fn gate_model_authority_boundary_no_order_broker_or_mt5_interface_exists() {
    let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut pending = vec![source_root];
    let forbidden = [
        "order_send",
        "ordersend",
        "submit_order",
        "place_order",
        "send_order",
        "position_size(",
        "metatrader",
        "mt5::",
        "broker_client",
        "trade_request",
    ];
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(&path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap().to_lowercase();
            for term in forbidden {
                assert!(!text.contains(term), "{} mentions {term}", path.display());
            }
        }
    }
    let manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
    )
    .unwrap();
    for crate_name in ["reqwest", "hyper", "ureq", "tokio", "ort", "tract"] {
        assert!(
            !manifest.contains(&format!("\n{crate_name}")),
            "{crate_name} is not a dependency"
        );
    }
}

// ---------------------------------------------------------------- inference_materialization

#[test]
fn gate_inference_materialization_preview_confirm_and_byte_identical_onnx_replay() {
    let mut fixture = Fixture::new();
    let feature_set = fixture.standard_feature_set();
    let model_id = register_spread(&fixture);
    let request = spread_request(&model_id, &feature_set);

    let prepared = fixture.prepare(&request).unwrap();
    assert_eq!(
        fixture.registry_rows("inference_records"),
        0,
        "preview registers nothing"
    );
    assert_eq!(prepared.preview.sample_rows.len(), 20);
    assert!(prepared.preview.existing.is_none());
    let first = fixture.lab().commit_inference(prepared).unwrap();
    assert_eq!(first.disposition, InferenceDisposition::Materialized);
    assert_eq!(fixture.staging_entries(), 0);
    let output = fixture
        .record_dir(&first.record_id)
        .join("inference.parquet");
    let original = std::fs::read(&output).unwrap();

    fixture.reopen().unwrap();
    let replay = fixture.prepare(&request).unwrap();
    assert_eq!(replay.manifest.record_id, first.record_id);
    assert_eq!(
        replay
            .preview
            .existing
            .as_ref()
            .map(|notice| notice.output_matches),
        Some(true)
    );
    assert_eq!(
        std::fs::read(replay.staging_directory.join("inference.parquet")).unwrap(),
        original,
        "ONNX replay is byte-identical"
    );
    let replayed = fixture.lab().commit_inference(replay).unwrap();
    assert_eq!(replayed.disposition, InferenceDisposition::ReplayReproduced);
    assert_eq!(replayed.determinism, Determinism::Reproduced);
    assert_eq!(std::fs::read(&output).unwrap(), original);
    assert_eq!(fixture.registry_rows("inference_records"), 1);
    assert_eq!(fixture.registry_rows("inference_replays"), 1);
}

#[test]
fn gate_inference_materialization_non_deterministic_runtime_is_recorded_never_overwritten() {
    let fixture = Fixture::new();
    let feature_set = fixture.standard_feature_set();
    let mock = MockOllama::start(MockBehavior::VaryingOutput);
    let model_id = fixture.register_ollama(&mock, 5, vec![float_output("score", -2.0, 2.0)]);
    let request = feature_request(
        &model_id,
        &feature_set,
        &[("fast", feature("fast")), ("slow", feature("slow"))],
    );
    let first = fixture.infer(&request);
    let output = fixture
        .record_dir(&first.record_id)
        .join("inference.parquet");
    let original = std::fs::read(&output).unwrap();

    let replay = fixture.prepare(&request).unwrap();
    assert_eq!(replay.manifest.record_id, first.record_id);
    assert_eq!(
        replay
            .preview
            .existing
            .as_ref()
            .map(|notice| notice.output_matches),
        Some(false)
    );
    let result = fixture.lab().commit_inference(replay).unwrap();
    assert_eq!(
        result.disposition,
        InferenceDisposition::ReplayNonDeterministic
    );
    assert_eq!(result.determinism, Determinism::NonDeterministic);
    assert_eq!(
        std::fs::read(&output).unwrap(),
        original,
        "stored record kept"
    );
    let entry = fixture
        .lab()
        .inspect_inference_record(&first.record_id)
        .unwrap()
        .unwrap();
    assert_eq!(entry.status, InferenceRecordStatus::Ready);
    assert_eq!(entry.determinism, Determinism::NonDeterministic);
    assert_eq!(entry.replays.len(), 1);
    assert_eq!(entry.replays[0].outcome, "NON_DETERMINISTIC");
    let attempts = fixture.lab().list_attempts().unwrap();
    assert!(attempts
        .iter()
        .any(|attempt| attempt.status == "NON_DETERMINISTIC"));
}

#[test]
fn gate_inference_materialization_changed_bindings_or_model_give_new_identities() {
    let fixture = Fixture::new();
    let feature_set = fixture.standard_feature_set();
    let model_id = register_spread(&fixture);
    let swapped = fixture.prepare(&feature_request(
        &model_id,
        &feature_set,
        &[("fast", feature("slow")), ("slow", feature("fast"))],
    ));
    let normal = fixture.prepare(&spread_request(&model_id, &feature_set));
    let other_model = fixture.register_onnx(
        "half.onnx",
        &onnx::linear_model(2, &[0.5, -0.5], &[0.0]),
        &["fast", "slow"],
        spread_outputs(),
    );
    let other = fixture.prepare(&spread_request(&other_model, &feature_set));
    let ids = [
        swapped.unwrap().manifest.record_id,
        normal.unwrap().manifest.record_id,
        other.unwrap().manifest.record_id,
    ];
    assert_eq!(
        ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
        3
    );
}

#[test]
fn gate_inference_materialization_interrupted_promotion_recovers_on_restart() {
    let mut fixture = Fixture::new();
    let feature_set = fixture.standard_feature_set();
    let model_id = register_spread(&fixture);
    let prepared = fixture
        .prepare(&spread_request(&model_id, &feature_set))
        .unwrap();
    // Simulate a crash after the journal was written but before the directory was promoted.
    let staging_name = prepared
        .staging_directory
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let manifest = &prepared.manifest;
    let pending = PendingModelLabPromotion {
        staging_name,
        record: ModelLabRecordMetadata {
            kind: ModelLabRecordKind::Inference,
            record_id: manifest.record_id.clone(),
            identity_sha256: manifest.identity_sha256.clone(),
            parent_id: model_id.clone(),
            input_id: Some(feature_set.clone()),
            manifest_json: prepared.manifest_json.clone(),
            manifest_sha256: prepared.manifest_sha256.clone(),
            content_sha256: manifest.output_sha256.clone(),
            created_at_utc: manifest.created_at_utc.clone(),
            status: "READY".into(),
            manifest_integrity_valid: true,
        },
        attempt: ModelLabAttemptRecord {
            attempt_id: "00000000-0000-7000-8000-000000000001".into(),
            started_at_utc: manifest.created_at_utc.clone(),
            completed_at_utc: manifest.created_at_utc.clone(),
            operation: "INFERENCE".into(),
            status: "MATERIALIZED".into(),
            subject_id: Some(manifest.record_id.clone()),
            diagnostic_code: None,
            diagnostic_message: None,
        },
    };
    let record_id = manifest.record_id.clone();
    {
        let connection = rusqlite::Connection::open(fixture.root.join("research.sqlite3")).unwrap();
        save_pending_model_lab_promotion(&connection, &pending).unwrap();
    }
    drop(prepared);
    fixture.reopen().unwrap();
    let entry = fixture
        .lab()
        .inspect_inference_record(&record_id)
        .unwrap()
        .unwrap();
    assert_eq!(entry.status, InferenceRecordStatus::Ready);
    assert_eq!(fixture.staging_entries(), 0);
    assert_eq!(fixture.registry_rows("model_lab_promotions"), 0);
}

#[test]
fn gate_inference_materialization_tampered_journal_blocks_recovery_and_preserves_evidence() {
    let mut fixture = Fixture::new();
    let feature_set = fixture.standard_feature_set();
    let model_id = register_spread(&fixture);
    let prepared = fixture
        .prepare(&spread_request(&model_id, &feature_set))
        .unwrap();
    let stage = prepared.staging_directory.clone();
    let manifest = &prepared.manifest;
    let pending = PendingModelLabPromotion {
        staging_name: stage.file_name().unwrap().to_str().unwrap().to_owned(),
        record: ModelLabRecordMetadata {
            kind: ModelLabRecordKind::Inference,
            record_id: manifest.record_id.clone(),
            identity_sha256: manifest.identity_sha256.clone(),
            parent_id: model_id.clone(),
            input_id: Some(feature_set.clone()),
            manifest_json: prepared.manifest_json.clone(),
            manifest_sha256: prepared.manifest_sha256.clone(),
            content_sha256: manifest.output_sha256.clone(),
            created_at_utc: manifest.created_at_utc.clone(),
            status: "READY".into(),
            manifest_integrity_valid: true,
        },
        attempt: ModelLabAttemptRecord {
            attempt_id: "00000000-0000-7000-8000-000000000002".into(),
            started_at_utc: manifest.created_at_utc.clone(),
            completed_at_utc: manifest.created_at_utc.clone(),
            operation: "INFERENCE".into(),
            status: "MATERIALIZED".into(),
            subject_id: Some(manifest.record_id.clone()),
            diagnostic_code: None,
            diagnostic_message: None,
        },
    };
    {
        let connection = rusqlite::Connection::open(fixture.root.join("research.sqlite3")).unwrap();
        save_pending_model_lab_promotion(&connection, &pending).unwrap();
    }
    let output = stage.join("inference.parquet");
    let mut bytes = std::fs::read(&output).unwrap();
    bytes[64] ^= 0x01;
    std::fs::write(&output, bytes).unwrap();
    drop(prepared);
    assert!(matches!(
        fixture.reopen(),
        Err(ModelError::RecoveryBlocked { .. })
    ));
    assert!(stage.exists(), "evidence was preserved");
    assert_eq!(fixture.registry_rows("inference_records"), 0);
}

// ---------------------------------------------------------------- model_comparison

#[test]
fn gate_model_comparison_reports_hand_calculated_coverage_and_differences() {
    let fixture = Fixture::new();
    let feature_set = fixture.standard_feature_set();
    let spread = register_spread(&fixture);
    // spread + 0.25 everywhere: every numeric difference is 0.25 (within f32 rounding).
    let shifted = fixture.register_onnx(
        "shifted.onnx",
        &onnx::linear_model(2, &[1.0, -1.0], &[0.25]),
        &["fast", "slow"],
        spread_outputs(),
    );
    let a = fixture.infer(&spread_request(&spread, &feature_set));
    let b = fixture.infer(&spread_request(&shifted, &feature_set));
    let report = fixture
        .lab()
        .compare(
            &ComparisonRequest {
                record_ids: vec![a.record_id.clone(), b.record_id.clone()],
            },
            &never(),
        )
        .unwrap();
    assert_eq!(report.rows, 60);
    assert_eq!(report.input_id, feature_set);
    for coverage in &report.records {
        assert_eq!(coverage.rows, 60);
        assert_eq!(coverage.ok_rows, 53);
        assert_eq!(coverage.skipped_rows, 7);
        assert!((coverage.coverage - 53.0 / 60.0).abs() < 1e-12);
        assert_eq!(coverage.invalid_output_rate, Some(0.0));
        assert!(coverage.mean_latency_us.is_some());
    }
    let pair = &report.pairs[0];
    assert_eq!(pair.fields.len(), 1);
    let field = &pair.fields[0];
    assert_eq!(field.rows_both_ok, 53);
    assert!((field.mean_absolute_difference.unwrap() - 0.25).abs() < 1e-6);
    assert!((field.max_absolute_difference.unwrap() - 0.25).abs() < 1e-6);
    assert!(field.agreement_rate.is_none());
}

#[test]
fn gate_model_comparison_category_agreement_matches_hand_count() {
    let fixture = Fixture::new();
    let feature_set = fixture.standard_feature_set();
    let mock = MockOllama::start(MockBehavior::Normal);
    let outputs = vec![category_output("direction", &["UP", "DOWN"])];
    // The mock answers UP when fast + slow > 0; with prices near 1.1 every answered row is UP.
    let first = fixture.register_ollama(&mock, 1, outputs.clone());
    let second = fixture.register_ollama(&mock, 2, outputs);
    let bindings = [("fast", feature("fast")), ("slow", feature("slow"))];
    let a = fixture.infer(&feature_request(&first, &feature_set, &bindings));
    // Binding `roc` (which changes sign) as fast flips some answers to DOWN.
    let b = fixture.infer(&feature_request(
        &second,
        &feature_set,
        &[("fast", feature("roc")), ("slow", feature("roc"))],
    ));
    let b_labels = text_values(
        &fixture.record_dir(&b.record_id).join("inference.parquet"),
        "output__direction",
    );
    let a_labels = text_values(
        &fixture.record_dir(&a.record_id).join("inference.parquet"),
        "output__direction",
    );
    let both = a_labels
        .iter()
        .zip(&b_labels)
        .filter(|(a, b)| a.is_some() && b.is_some())
        .count() as u64;
    let agreeing = a_labels
        .iter()
        .zip(&b_labels)
        .filter(|(a, b)| a.is_some() && a == b)
        .count() as u64;
    assert!(
        agreeing > 0 && agreeing < both,
        "fixture must produce partial agreement"
    );
    let report = fixture
        .lab()
        .compare(
            &ComparisonRequest {
                record_ids: vec![a.record_id, b.record_id],
            },
            &never(),
        )
        .unwrap();
    let field = &report.pairs[0].fields[0];
    assert_eq!(field.rows_both_ok, both);
    assert_eq!(field.agreeing_rows, Some(agreeing));
    assert_eq!(field.agreement_rate, Some(agreeing as f64 / both as f64));
    assert!(field.mean_absolute_difference.is_none());
}

#[test]
fn gate_model_comparison_refuses_different_inputs_and_has_no_performance_fields() {
    let fixture = Fixture::new();
    let first_set = fixture.standard_feature_set();
    let second_set = fixture.feature_set(&fixture.import("second", &bars(60, 1.0)));
    let model_id = register_spread(&fixture);
    let a = fixture.infer(&spread_request(&model_id, &first_set));
    let b = fixture.infer(&spread_request(&model_id, &second_set));
    assert!(matches!(
        fixture.lab().compare(
            &ComparisonRequest {
                record_ids: vec![a.record_id.clone(), b.record_id.clone()]
            },
            &never()
        ),
        Err(ModelError::ComparisonRefused(_))
    ));
    for ids in [
        vec![a.record_id.clone()],
        vec![a.record_id.clone(), a.record_id.clone()],
    ] {
        assert!(matches!(
            fixture
                .lab()
                .compare(&ComparisonRequest { record_ids: ids }, &never()),
            Err(ModelError::ComparisonRefused(_))
        ));
    }
    let other = fixture.register_onnx(
        "half.onnx",
        &onnx::linear_model(2, &[0.5, -0.5], &[0.0]),
        &["fast", "slow"],
        spread_outputs(),
    );
    let c = fixture.infer(&spread_request(&other, &first_set));
    let report = fixture
        .lab()
        .compare(
            &ComparisonRequest {
                record_ids: vec![a.record_id, c.record_id],
            },
            &never(),
        )
        .unwrap();
    let json = serde_json::to_value(&report).unwrap();
    let mut keys = Vec::new();
    collect_keys(&json, &mut keys);
    for key in keys {
        let key = key.to_lowercase();
        for forbidden in [
            "pnl", "profit", "win", "edge", "return", "sharpe", "drawdown", "equity", "accuracy",
            "hit_rate", "position",
        ] {
            assert!(
                !key.contains(forbidden),
                "comparison field {key} is not descriptive"
            );
        }
    }
}

fn collect_keys(value: &serde_json::Value, keys: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                keys.push(key.clone());
                collect_keys(value, keys);
            }
        }
        serde_json::Value::Array(values) => {
            values.iter().for_each(|value| collect_keys(value, keys))
        }
        _ => {}
    }
}

// ---------------------------------------------------------------- resource_cancel_recovery

#[test]
fn gate_resource_cancel_recovery_quotas_leave_no_partial_record() {
    let fixture = Fixture::new();
    let feature_set = fixture.standard_feature_set();
    let model_id = register_spread(&fixture);
    let mut request = spread_request(&model_id, &feature_set);
    request.quotas.max_rows = 59;
    assert!(matches!(
        fixture.prepare(&request),
        Err(ModelError::ResourceQuota(_))
    ));
    let mut request = spread_request(&model_id, &feature_set);
    request.quotas.max_output_bytes = 512;
    assert!(matches!(
        fixture.prepare(&request),
        Err(ModelError::ResourceQuota(_))
    ));
    let mut request = spread_request(&model_id, &feature_set);
    request.quotas.request_timeout_ms = 10;
    assert!(matches!(
        fixture.prepare(&request),
        Err(ModelError::InvalidRequest(_))
    ));
    assert_eq!(fixture.staging_entries(), 0);
    assert_eq!(fixture.registry_rows("inference_records"), 0);

    // Oversized model files are refused before they are copied.
    let small = rg_model_registry::store::ModelLabLimits {
        max_onnx_bytes: 64,
        ..rg_model_registry::store::DEFAULT_LIMITS
    };
    let lab = rg_model_registry::store::ModelLabStore::open_with_limits(
        &fixture.root,
        std::sync::Arc::clone(&fixture.strategies),
        small,
    )
    .unwrap();
    let path = fixture.write_file("big.onnx", &onnx::classifier_model(8));
    assert!(matches!(
        lab.stage_onnx_candidate(&path, &never()),
        Err(ModelError::ResourceQuota(_))
    ));
}

#[test]
fn gate_resource_cancel_recovery_cancellation_and_timeouts_leave_no_record_and_never_kill_the_runtime(
) {
    let fixture = Fixture::new();
    let feature_set = fixture.standard_feature_set();
    let model_id = register_spread(&fixture);
    let cancelled = AtomicBool::new(true);
    assert!(matches!(
        fixture.lab().prepare_inference(
            &spread_request(&model_id, &feature_set),
            &cancelled,
            &InferenceProgress::default()
        ),
        Err(ModelError::Cancelled)
    ));

    let mock = MockOllama::start(MockBehavior::Normal);
    let ollama_model = fixture.register_ollama(&mock, 9, vec![float_output("score", -2.0, 2.0)]);
    mock.set_behavior(MockBehavior::Stall);
    let mut request = feature_request(
        &ollama_model,
        &feature_set,
        &[("fast", feature("fast")), ("slow", feature("slow"))],
    );
    request.quotas.request_timeout_ms = 300;
    // The stalled runtime makes the model check itself unavailable or the generation time out.
    let result = fixture.prepare(&request);
    assert!(
        matches!(
            result,
            Err(ModelError::Runtime(_)) | Err(ModelError::ModelNotUsable { .. })
        ),
        "{result:?}"
    );
    // The runtime process was never signalled: it still answers once it behaves again.
    mock.set_behavior(MockBehavior::Normal);
    assert!(fixture.lab().discover_ollama().unwrap().candidates.len() == 1);
    assert_eq!(fixture.staging_entries(), 0);
    assert_eq!(fixture.registry_rows("inference_records"), 0);
}

#[test]
fn gate_resource_cancel_recovery_runtime_crash_mid_run_fails_closed() {
    let fixture = Fixture::new();
    let feature_set = fixture.standard_feature_set();
    let mock = MockOllama::start(MockBehavior::Normal);
    let model_id = fixture.register_ollama(&mock, 4, vec![float_output("score", -2.0, 2.0)]);
    mock.set_behavior(MockBehavior::CrashAfterGenerations(5));
    let request = feature_request(
        &model_id,
        &feature_set,
        &[("fast", feature("fast")), ("slow", feature("slow"))],
    );
    let result = fixture.prepare(&request);
    assert!(matches!(result, Err(ModelError::Runtime(_))), "{result:?}");
    mock.set_behavior(MockBehavior::GenerateError);
    assert!(matches!(
        fixture.prepare(&request),
        Err(ModelError::Runtime(_))
    ));
    assert_eq!(fixture.staging_entries(), 0);
    assert_eq!(fixture.registry_rows("inference_records"), 0);
    let attempts = fixture.lab().list_attempts().unwrap();
    assert!(attempts
        .iter()
        .all(|attempt| attempt.status != "MATERIALIZED"));
}

// ---------------------------------------------------------------- adversarial_tamper

#[test]
fn gate_adversarial_tamper_tampered_outputs_manifests_and_files_quarantine_records() {
    let fixture = Fixture::new();
    let feature_set = fixture.standard_feature_set();
    let model_id = register_spread(&fixture);
    let first = fixture.infer(&spread_request(&model_id, &feature_set));
    let second = fixture.infer(&feature_request(
        &model_id,
        &feature_set,
        &[("fast", feature("roc")), ("slow", feature("fast"))],
    ));
    let third = fixture.infer(&feature_request(
        &model_id,
        &feature_set,
        &[("fast", feature("slow")), ("slow", feature("roc"))],
    ));

    let output = fixture
        .record_dir(&first.record_id)
        .join("inference.parquet");
    let mut bytes = std::fs::read(&output).unwrap();
    bytes[80] ^= 0x01;
    std::fs::write(&output, bytes).unwrap();
    let manifest = fixture.record_dir(&second.record_id).join("manifest.json");
    let text = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(&manifest, text.replace("ADVISORY_ONLY", "DECISION_VETO")).unwrap();
    std::fs::remove_file(
        fixture
            .record_dir(&third.record_id)
            .join("inference.parquet"),
    )
    .unwrap();

    let records = fixture.lab().list_inference_records(&never()).unwrap();
    assert_eq!(records.len(), 3);
    for record in &records {
        assert_eq!(
            record.status,
            InferenceRecordStatus::Quarantined,
            "{}",
            record.record_id
        );
        assert!(record
            .diagnostic
            .as_ref()
            .unwrap()
            .contains(&record.record_id));
    }
    for record in [&first, &second, &third] {
        assert!(matches!(
            fixture.lab().usable_record(&record.record_id),
            Err(ModelError::RecordNotUsable { .. })
        ));
    }
    assert!(fixture
        .lab()
        .compare(
            &ComparisonRequest {
                record_ids: vec![first.record_id.clone(), second.record_id.clone()]
            },
            &never()
        )
        .is_err());
}

#[test]
fn gate_adversarial_tamper_upstream_and_model_tamper_revoke_ready_records() {
    let fixture = Fixture::new();
    let feature_set = fixture.standard_feature_set();
    let model_id = register_spread(&fixture);
    let record = fixture.infer(&spread_request(&model_id, &feature_set));

    let model_file = fixture.model_dir(&model_id).join("model.onnx");
    let original_model = std::fs::read(&model_file).unwrap();
    let mut tampered = original_model.clone();
    let last = tampered.len() - 5;
    tampered[last] ^= 0x20;
    std::fs::write(&model_file, &tampered).unwrap();
    let entry = fixture
        .lab()
        .inspect_inference_record(&record.record_id)
        .unwrap()
        .unwrap();
    assert_eq!(entry.status, InferenceRecordStatus::Quarantined);
    assert!(entry.diagnostic.unwrap().contains(&model_id));
    assert!(matches!(
        fixture.prepare(&spread_request(&model_id, &feature_set)),
        Err(ModelError::ModelNotUsable { .. })
    ));
    std::fs::write(&model_file, &original_model).unwrap();
    assert_eq!(
        fixture
            .lab()
            .inspect_inference_record(&record.record_id)
            .unwrap()
            .unwrap()
            .status,
        InferenceRecordStatus::Ready
    );

    let output = fixture.feature_output(&feature_set);
    let mut bytes = std::fs::read(&output).unwrap();
    bytes[90] ^= 0x02;
    std::fs::write(&output, bytes).unwrap();
    let entry = fixture
        .lab()
        .inspect_inference_record(&record.record_id)
        .unwrap()
        .unwrap();
    assert_eq!(entry.status, InferenceRecordStatus::Quarantined);
    assert!(entry.diagnostic.unwrap().contains(&feature_set));
}

#[test]
fn gate_adversarial_tamper_runtime_digest_drift_blocks_inference_with_named_diagnostic() {
    let fixture = Fixture::new();
    let feature_set = fixture.standard_feature_set();
    let mock = MockOllama::start(MockBehavior::Normal);
    let model_id = fixture.register_ollama(&mock, 6, vec![float_output("score", -2.0, 2.0)]);
    mock.set_models(vec![MockModel {
        name: FIXTURE_MODEL.into(),
        digest: FIXTURE_DIGEST_CHANGED.into(),
    }]);
    let error = fixture
        .prepare(&feature_request(
            &model_id,
            &feature_set,
            &[("fast", feature("fast")), ("slow", feature("slow"))],
        ))
        .unwrap_err();
    match error {
        ModelError::ModelNotUsable {
            model_id: named,
            status,
            reason,
        } => {
            assert_eq!(named, model_id);
            assert_eq!(status, "QUARANTINED");
            assert!(reason.unwrap().contains(FIXTURE_MODEL));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        mock.generations(),
        0,
        "no generation ran against drifted content"
    );
}
