use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

use rg_data_ground::store::DatasetStore;
use rg_feature_studio::{
    indicator_definitions,
    registry::MAX_PERIOD,
    spec::{MAX_AVAILABILITY_LAG_SECONDS, MAX_FEATURES, MAX_STALENESS_SECONDS, RESEARCH_NOTICE},
    store::{
        FeatureCommitResult, FeatureInputCandidate, FeatureSetCatalogEntry, FeatureSetPreview,
        FeatureStore, PreparedFeatureSet,
    },
    FeatureError, FeatureProgress, FeatureSetRequest, IndicatorDefinition, ProgressSnapshot,
    TemporalSemantics, PREVIEW_ROWS,
};
use rg_persistence::FeatureAttemptRecord;
use serde::Serialize;
use tauri::State;
use uuid::Uuid;

pub struct FeatureStudioState {
    store: Arc<FeatureStore>,
    start_lock: Mutex<()>,
    running: Mutex<Option<RunningFeatureJob>>,
    prepared: Mutex<Option<PreparedFeatureJob>>,
}

struct RunningFeatureJob {
    job_id: String,
    cancel: Arc<AtomicBool>,
    progress: Arc<FeatureProgress>,
}

struct PreparedFeatureJob {
    job_id: String,
    prepared: PreparedFeatureSet,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeatureRegistrySnapshot {
    pub engine_id: &'static str,
    pub engine_version: &'static str,
    pub temporal_semantics: TemporalSemantics,
    pub indicators: &'static [IndicatorDefinition],
    pub limits: FeatureLimits,
    pub research_notice: &'static str,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeatureLimits {
    pub max_features: usize,
    pub max_period: i64,
    pub max_staleness_seconds: u64,
    pub max_availability_lag_seconds: u64,
    pub preview_rows: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeaturePreviewSession {
    pub job_id: String,
    pub preview: FeatureSetPreview,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ActiveFeatureJob {
    Running {
        #[serde(rename = "jobId")]
        job_id: String,
        progress: ProgressSnapshot,
    },
    Prepared {
        session: Box<FeaturePreviewSession>,
    },
}

/// Handles that a started preview worker needs. Created by [`FeatureStudioState::begin_preview`].
pub struct PreviewWorker {
    pub store: Arc<FeatureStore>,
    pub cancel: Arc<AtomicBool>,
    pub progress: Arc<FeatureProgress>,
}

const UNAVAILABLE: &str = "feature job state is unavailable";

impl FeatureStudioState {
    pub fn open(root: impl AsRef<Path>, datasets: Arc<DatasetStore>) -> Result<Self, FeatureError> {
        Ok(Self {
            store: Arc::new(FeatureStore::open(root, datasets)?),
            start_lock: Mutex::new(()),
            running: Mutex::new(None),
            prepared: Mutex::new(None),
        })
    }

    fn guard<'a, T>(mutex: &'a Mutex<T>) -> Result<MutexGuard<'a, T>, String> {
        mutex.lock().map_err(|_| UNAVAILABLE.to_owned())
    }

    /// Registers a new preview job. Only one feature job may run or await confirmation at a time.
    pub fn begin_preview(&self, job_id: &str) -> Result<PreviewWorker, String> {
        validate_job_id(job_id)?;
        let _start = Self::guard(&self.start_lock)?;
        if Self::guard(&self.prepared)?.is_some() || Self::guard(&self.running)?.is_some() {
            return Err(
                "finish or cancel the existing feature preview before starting another".into(),
            );
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(FeatureProgress::default());
        *Self::guard(&self.running)? = Some(RunningFeatureJob {
            job_id: job_id.to_owned(),
            cancel: Arc::clone(&cancel),
            progress: Arc::clone(&progress),
        });
        Ok(PreviewWorker {
            store: Arc::clone(&self.store),
            cancel,
            progress,
        })
    }

    /// Settles a finished preview worker. A preview that finished after cancellation was requested
    /// is discarded rather than offered for confirmation.
    pub fn finish_preview(
        &self,
        job_id: &str,
        result: Result<PreparedFeatureSet, FeatureError>,
    ) -> Result<FeaturePreviewSession, String> {
        let _start = Self::guard(&self.start_lock)?;
        let running = Self::guard(&self.running)?.take();
        let cancelled = running
            .as_ref()
            .is_some_and(|job| job.cancel.load(Ordering::Relaxed));
        match result {
            Ok(prepared) if cancelled => {
                self.store
                    .discard_prepared(prepared)
                    .map_err(|error| format!("cancelled preview cleanup failed: {error}"))?;
                Err("feature computation cancelled".into())
            }
            Ok(prepared) => {
                let session = FeaturePreviewSession {
                    job_id: job_id.to_owned(),
                    preview: prepared.preview.clone(),
                };
                *Self::guard(&self.prepared)? = Some(PreparedFeatureJob {
                    job_id: job_id.to_owned(),
                    prepared,
                });
                Ok(session)
            }
            Err(error) => {
                let status = if matches!(error, FeatureError::Cancelled) {
                    "CANCELLED"
                } else {
                    "FAILED"
                };
                let _ = self
                    .store
                    .record_failure(status, error.code(), &error.to_string());
                Err(error.to_string())
            }
        }
    }

    fn active_job(&self) -> Result<Option<ActiveFeatureJob>, String> {
        let _start = Self::guard(&self.start_lock)?;
        if let Some(job) = Self::guard(&self.prepared)?.as_ref() {
            return Ok(Some(ActiveFeatureJob::Prepared {
                session: Box::new(FeaturePreviewSession {
                    job_id: job.job_id.clone(),
                    preview: job.prepared.preview.clone(),
                }),
            }));
        }
        Ok(Self::guard(&self.running)?
            .as_ref()
            .map(|job| ActiveFeatureJob::Running {
                job_id: job.job_id.clone(),
                progress: job.progress.snapshot(),
            }))
    }

    fn take_prepared(&self, job_id: &str) -> Result<PreparedFeatureSet, String> {
        validate_job_id(job_id)?;
        let _start = Self::guard(&self.start_lock)?;
        let mut prepared = Self::guard(&self.prepared)?;
        match prepared.take() {
            Some(job) if job.job_id == job_id => Ok(job.prepared),
            other => {
                *prepared = other;
                Err("no validated feature preview exists for this job".into())
            }
        }
    }

    fn cancel_job(&self, job_id: &str) -> Result<String, String> {
        validate_job_id(job_id)?;
        {
            let _start = Self::guard(&self.start_lock)?;
            if let Some(job) = Self::guard(&self.running)?
                .as_ref()
                .filter(|job| job.job_id == job_id)
            {
                job.cancel.store(true, Ordering::Relaxed);
                return Ok("CANCELLATION_REQUESTED".into());
            }
        }
        let prepared = self
            .take_prepared(job_id)
            .map_err(|_| "no running or prepared feature job exists for this job".to_owned())?;
        self.store
            .discard_prepared(prepared)
            .map_err(|error| error.to_string())?;
        Ok("CANCELLED".into())
    }
}

#[tauri::command]
pub fn get_feature_registry() -> FeatureRegistrySnapshot {
    FeatureRegistrySnapshot {
        engine_id: rg_feature_studio::spec::FEATURE_ENGINE_ID,
        engine_version: rg_feature_studio::spec::FEATURE_ENGINE_VERSION,
        temporal_semantics: TemporalSemantics::current(),
        indicators: indicator_definitions(),
        limits: FeatureLimits {
            max_features: MAX_FEATURES,
            max_period: MAX_PERIOD,
            max_staleness_seconds: MAX_STALENESS_SECONDS,
            max_availability_lag_seconds: MAX_AVAILABILITY_LAG_SECONDS,
            preview_rows: PREVIEW_ROWS,
        },
        research_notice: RESEARCH_NOTICE,
    }
}

#[tauri::command]
pub async fn list_feature_inputs(
    state: State<'_, FeatureStudioState>,
) -> Result<Vec<FeatureInputCandidate>, String> {
    let store = Arc::clone(&state.store);
    tauri::async_runtime::spawn_blocking(move || {
        store.list_input_candidates(&AtomicBool::new(false))
    })
    .await
    .map_err(|error| format!("feature input worker failed: {error}"))?
    .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn preview_feature_set(
    state: State<'_, FeatureStudioState>,
    job_id: String,
    request: FeatureSetRequest,
) -> Result<FeaturePreviewSession, String> {
    let worker = state.begin_preview(&job_id)?;
    let result = tauri::async_runtime::spawn_blocking(move || {
        worker
            .store
            .prepare(&request, &worker.cancel, &worker.progress)
    })
    .await
    .unwrap_or_else(|error| {
        Err(FeatureError::InvalidPrepared(format!(
            "feature worker failed: {error}"
        )))
    });
    state.finish_preview(&job_id, result)
}

#[tauri::command]
pub fn get_active_feature_job(
    state: State<'_, FeatureStudioState>,
) -> Result<Option<ActiveFeatureJob>, String> {
    state.active_job()
}

/// Commits the prepared preview. The result reports only the transactional commit; refreshing the
/// feature-set catalogue is a separate, non-transactional read.
#[tauri::command(rename_all = "camelCase")]
pub async fn confirm_feature_set(
    state: State<'_, FeatureStudioState>,
    job_id: String,
) -> Result<FeatureCommitResult, String> {
    let prepared = state.take_prepared(&job_id)?;
    let store = Arc::clone(&state.store);
    tauri::async_runtime::spawn_blocking(move || store.commit_prepared(prepared))
        .await
        .map_err(|error| format!("feature commit worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub fn cancel_feature_job(
    state: State<'_, FeatureStudioState>,
    job_id: String,
) -> Result<String, String> {
    state.cancel_job(&job_id)
}

#[tauri::command]
pub async fn list_feature_sets(
    state: State<'_, FeatureStudioState>,
) -> Result<Vec<FeatureSetCatalogEntry>, String> {
    let store = Arc::clone(&state.store);
    tauri::async_runtime::spawn_blocking(move || store.list_feature_sets(&AtomicBool::new(false)))
        .await
        .map_err(|error| format!("feature catalogue worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn inspect_feature_set(
    state: State<'_, FeatureStudioState>,
    feature_set_id: String,
) -> Result<Option<FeatureSetCatalogEntry>, String> {
    let store = Arc::clone(&state.store);
    tauri::async_runtime::spawn_blocking(move || store.inspect_feature_set(&feature_set_id))
        .await
        .map_err(|error| format!("feature inspection worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn list_feature_attempts(
    state: State<'_, FeatureStudioState>,
) -> Result<Vec<FeatureAttemptRecord>, String> {
    state
        .store
        .list_attempts()
        .map_err(|error| error.to_string())
}

fn validate_job_id(value: &str) -> Result<(), String> {
    Uuid::parse_str(value)
        .map(|_| ())
        .map_err(|_| "feature job identity is invalid".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{data_ground_commands::DataGroundState, register_commands};
    use rg_data_ground::{
        CalendarPolicy, CsvDelimiter, FieldMapping, ImportRequest, MarketRole, ResourceQuotas,
        TimestampSemantics,
    };
    use std::fs;
    use tauri::{
        ipc::{CallbackFn, InvokeBody},
        test::{mock_builder, mock_context, noop_assets, MockRuntime, INVOKE_KEY},
        webview::{InvokeRequest, Webview, WebviewWindowBuilder},
        Manager,
    };
    use tempfile::TempDir;

    const JOB_ID: &str = "018f4c4f-7d9b-7b9f-a70e-92bb7cf04c71";
    const OTHER_JOB_ID: &str = "018f4c4f-7d9b-7b9f-a70e-92bb7cf04c72";

    /// Labeled synthetic H1 bars for two markets; imported through Data Ground as a READY dataset.
    fn synthetic_csv() -> String {
        let mut text = String::from("timestamp,symbol,timeframe,open,high,low,close,volume\n");
        for (symbol, base) in [("EURUSD.m", 1.1), ("GBPUSD.m", 1.3)] {
            for hour in 0..30 {
                let close = base + 0.001 * ((hour as f64) * 0.5).sin();
                text.push_str(&format!(
                    "2024-01-{:02}T{:02}:00:00Z,{symbol},H1,{close},{},{},{close},1\n",
                    1 + hour / 24,
                    hour % 24,
                    close + 0.0005,
                    close - 0.0005
                ));
            }
        }
        text
    }

    fn import_fixture(data: &DataGroundState, directory: &TempDir) -> String {
        let source = directory.path().join("synthetic-fixture.csv");
        fs::write(&source, synthetic_csv()).unwrap();
        let store = data.store();
        let prepared = store
            .prepare_import(
                &source,
                ImportRequest {
                    mapping: FieldMapping {
                        timestamp: "timestamp".into(),
                        open: "open".into(),
                        high: "high".into(),
                        low: "low".into(),
                        close: "close".into(),
                        volume: "volume".into(),
                        symbol: Some("symbol".into()),
                        timeframe: Some("timeframe".into()),
                    },
                    timestamp_semantics: TimestampSemantics::BarOpensAt,
                    naive_timestamp_offset_minutes: 0,
                    default_symbol: "EURUSD.m".into(),
                    default_timeframe: "H1".into(),
                    default_market_role: MarketRole::Target,
                    market_roles: Default::default(),
                    source_id: "synthetic-fixture:ipc".into(),
                    broker: "SYNTHETIC-FIXTURE".into(),
                    server: "SYNTHETIC-FIXTURE".into(),
                    delimiter: CsvDelimiter::Comma,
                    calendar: CalendarPolicy::Continuous,
                    quotas: ResourceQuotas {
                        max_source_bytes: Some(1_000_000),
                        max_records: Some(1_000),
                    },
                },
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        store.commit_prepared(prepared).unwrap().dataset_id
    }

    fn request_json(dataset_id: &str) -> serde_json::Value {
        serde_json::json!({
            "target": {"dataset_id": dataset_id, "symbol": "EURUSD.m", "timeframe": "H1"},
            "features": [
                {"key": "sma_5", "indicator_id": "SMA", "indicator_version": 1,
                 "parameters": {"period": 5}, "source": {"kind": "TARGET"}},
                {"key": "gbp_rsi", "indicator_id": "RSI_WILDER", "indicator_version": 1,
                 "parameters": {"period": 3, "price": "CLOSE"},
                 "source": {"kind": "REFERENCE",
                            "series": {"dataset_id": dataset_id, "symbol": "GBPUSD.m", "timeframe": "H1"},
                            "max_staleness_seconds": 3600, "availability_lag_seconds": 0}}
            ],
            "quotas": {"max_target_rows": 1000, "max_input_rows": 1000, "max_output_bytes": 10000000}
        })
    }

    struct Harness {
        _directory: TempDir,
        app: tauri::App<MockRuntime>,
        webview: tauri::WebviewWindow<MockRuntime>,
        dataset_id: String,
    }

    fn harness() -> Harness {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("app-data");
        let data = DataGroundState::open(&root).unwrap();
        let dataset_id = import_fixture(&data, &directory);
        let features = FeatureStudioState::open(&root, data.store()).unwrap();
        let app = register_commands(mock_builder())
            .manage(data)
            .manage(features)
            .build(mock_context(noop_assets()))
            .expect("mock Tauri runtime should build");
        let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .expect("mock webview should build");
        Harness {
            _directory: directory,
            app,
            webview,
            dataset_id,
        }
    }

    fn ipc<W: AsRef<Webview<MockRuntime>>>(
        webview: &W,
        command: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, serde_json::Value> {
        tauri::test::get_ipc_response(
            webview,
            InvokeRequest {
                cmd: command.into(),
                callback: CallbackFn(0),
                error: CallbackFn(1),
                url: "http://tauri.localhost".parse().unwrap(),
                body: InvokeBody::Json(payload),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.to_string(),
            },
        )
        .map(|body| body.deserialize::<serde_json::Value>().unwrap())
    }

    #[test]
    fn gate_feature_studio_ui_ipc_previews_restores_and_confirms_a_source_backed_feature_set() {
        let harness = harness();
        let webview = &harness.webview;
        let registry = ipc(webview, "get_feature_registry", serde_json::json!({})).unwrap();
        assert_eq!(registry["indicators"].as_array().unwrap().len(), 9);
        assert_eq!(registry["temporalSemantics"]["version"], 1);
        assert_eq!(registry["limits"]["previewRows"], PREVIEW_ROWS);

        let inputs = ipc(webview, "list_feature_inputs", serde_json::json!({})).unwrap();
        assert_eq!(inputs[0]["dataset_id"], harness.dataset_id);
        assert_eq!(inputs[0]["eligible"], true);
        assert_eq!(inputs[0]["source_id"], "synthetic-fixture:ipc");

        let preview = ipc(
            webview,
            "preview_feature_set",
            serde_json::json!({"jobId": JOB_ID, "request": request_json(&harness.dataset_id)}),
        )
        .unwrap();
        assert_eq!(preview["jobId"], JOB_ID);
        assert_eq!(preview["preview"]["manifest"]["row_count"], 30);
        assert_eq!(preview["preview"]["manifest"]["status"], "READY");
        assert_eq!(
            preview["preview"]["sample_rows"].as_array().unwrap().len(),
            PREVIEW_ROWS
        );
        let catalog = ipc(webview, "list_feature_sets", serde_json::json!({})).unwrap();
        assert!(
            catalog.as_array().unwrap().is_empty(),
            "preview must not register"
        );

        let second = ipc(
            webview,
            "preview_feature_set",
            serde_json::json!({"jobId": OTHER_JOB_ID, "request": request_json(&harness.dataset_id)}),
        );
        assert!(second.is_err(), "only one feature job may be pending");

        let restored = ipc(webview, "get_active_feature_job", serde_json::json!({})).unwrap();
        assert_eq!(restored["state"], "PREPARED");
        assert_eq!(restored["session"]["jobId"], JOB_ID);
        assert_eq!(
            restored["session"]["preview"]["feature_set_id"],
            preview["preview"]["feature_set_id"]
        );

        let committed = ipc(
            webview,
            "confirm_feature_set",
            serde_json::json!({"jobId": JOB_ID}),
        )
        .unwrap();
        assert_eq!(committed["disposition"], "MATERIALIZED");
        assert_eq!(
            committed["feature_set_id"],
            preview["preview"]["feature_set_id"]
        );
        assert!(
            ipc(webview, "get_active_feature_job", serde_json::json!({}))
                .unwrap()
                .is_null()
        );

        let catalog = ipc(webview, "list_feature_sets", serde_json::json!({})).unwrap();
        assert_eq!(catalog.as_array().unwrap().len(), 1);
        assert_eq!(catalog[0]["integrity"]["lineage_valid"], true);
        let inspected = ipc(
            webview,
            "inspect_feature_set",
            serde_json::json!({"featureSetId": committed["feature_set_id"]}),
        )
        .unwrap();
        assert_eq!(
            inspected["manifest"]["lineage"][0]["dataset_id"],
            harness.dataset_id
        );
        let attempts = ipc(webview, "list_feature_attempts", serde_json::json!({})).unwrap();
        assert_eq!(attempts[0]["status"], "MATERIALIZED");
    }

    #[test]
    fn gate_feature_studio_ui_ipc_cancels_a_running_job_and_records_the_attempt() {
        let harness = harness();
        let state = harness.app.state::<FeatureStudioState>();
        let worker = state.begin_preview(JOB_ID).unwrap();
        let running = ipc(
            &harness.webview,
            "get_active_feature_job",
            serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(running["state"], "RUNNING");
        assert_eq!(running["progress"]["phase"], "QUEUED");
        let cancelled = ipc(
            &harness.webview,
            "cancel_feature_job",
            serde_json::json!({"jobId": JOB_ID}),
        )
        .unwrap();
        assert_eq!(cancelled, "CANCELLATION_REQUESTED");

        let request: FeatureSetRequest =
            serde_json::from_value(request_json(&harness.dataset_id)).unwrap();
        let result = worker
            .store
            .prepare(&request, &worker.cancel, &worker.progress);
        assert!(matches!(result, Err(FeatureError::Cancelled)));
        assert!(state.finish_preview(JOB_ID, result).is_err());
        assert!(ipc(
            &harness.webview,
            "get_active_feature_job",
            serde_json::json!({})
        )
        .unwrap()
        .is_null());
        let attempts = ipc(
            &harness.webview,
            "list_feature_attempts",
            serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(attempts[0]["status"], "CANCELLED");
        assert_eq!(attempts[0]["diagnostic_code"], "CANCELLED");
    }

    #[test]
    fn gate_feature_studio_ui_ipc_discards_a_preview_that_finishes_after_cancellation() {
        let harness = harness();
        let state = harness.app.state::<FeatureStudioState>();
        let worker = state.begin_preview(JOB_ID).unwrap();
        let request: FeatureSetRequest =
            serde_json::from_value(request_json(&harness.dataset_id)).unwrap();
        let result = worker
            .store
            .prepare(&request, &worker.cancel, &worker.progress);
        assert!(result.is_ok());
        assert_eq!(state.cancel_job(JOB_ID).unwrap(), "CANCELLATION_REQUESTED");
        assert!(state.finish_preview(JOB_ID, result).is_err());
        assert!(state.active_job().unwrap().is_none());
        assert!(worker
            .store
            .list_feature_sets(&AtomicBool::new(false))
            .unwrap()
            .is_empty());
        assert_eq!(
            fs::read_dir(worker.store.staging_directory_for_tests())
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn gate_feature_studio_ui_ipc_failure_after_confirm_leaves_no_stale_preview() {
        let harness = harness();
        let preview = ipc(
            &harness.webview,
            "preview_feature_set",
            serde_json::json!({"jobId": JOB_ID, "request": request_json(&harness.dataset_id)}),
        )
        .unwrap();
        let state = harness.app.state::<FeatureStudioState>();
        let stage = state
            .prepared
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .prepared
            .staging_directory
            .clone();
        fs::write(stage.join("features.parquet"), b"changed after preview").unwrap();

        let failed = ipc(
            &harness.webview,
            "confirm_feature_set",
            serde_json::json!({"jobId": JOB_ID}),
        );
        assert!(failed.is_err());
        assert!(ipc(
            &harness.webview,
            "get_active_feature_job",
            serde_json::json!({})
        )
        .unwrap()
        .is_null());
        assert!(ipc(
            &harness.webview,
            "confirm_feature_set",
            serde_json::json!({"jobId": JOB_ID})
        )
        .is_err());
        let catalog = ipc(&harness.webview, "list_feature_sets", serde_json::json!({})).unwrap();
        assert!(catalog.as_array().unwrap().is_empty());
        let inspected = ipc(
            &harness.webview,
            "inspect_feature_set",
            serde_json::json!({"featureSetId": preview["preview"]["feature_set_id"]}),
        )
        .unwrap();
        assert!(inspected.is_null());
    }

    #[test]
    fn gate_feature_studio_ui_ipc_catalogue_revokes_readiness_after_source_tamper() {
        let harness = harness();
        ipc(
            &harness.webview,
            "preview_feature_set",
            serde_json::json!({"jobId": JOB_ID, "request": request_json(&harness.dataset_id)}),
        )
        .unwrap();
        let committed = ipc(
            &harness.webview,
            "confirm_feature_set",
            serde_json::json!({"jobId": JOB_ID}),
        )
        .unwrap();
        let catalog = ipc(&harness.webview, "list_feature_sets", serde_json::json!({})).unwrap();
        assert_eq!(catalog[0]["status"], "READY");

        let bars = harness
            ._directory
            .path()
            .join("app-data")
            .join("datasets")
            .join(&harness.dataset_id)
            .join("bars.parquet");
        let mut bytes = fs::read(&bars).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0x5a;
        fs::write(&bars, bytes).unwrap();

        let catalog = ipc(&harness.webview, "list_feature_sets", serde_json::json!({})).unwrap();
        assert_eq!(catalog[0]["status"], "QUARANTINED");
        assert_eq!(catalog[0]["integrity"]["lineage_valid"], false);
        let inspected = ipc(
            &harness.webview,
            "inspect_feature_set",
            serde_json::json!({"featureSetId": committed["feature_set_id"]}),
        )
        .unwrap();
        assert_eq!(inspected["status"], "QUARANTINED");
        assert_eq!(inspected["integrity"]["lineage_valid"], false);
    }

    #[test]
    fn gate_data_policy_feature_ipc_accepts_registry_identities_never_filesystem_paths() {
        let harness = harness();
        let mut with_path = request_json(&harness.dataset_id);
        with_path["target"]["dataset_id"] = serde_json::json!(
            r"C:\Users\fixture\AppData\Roaming\MetaQuotes\Terminal\Common\Files\EURUSD.csv"
        );
        assert!(ipc(
            &harness.webview,
            "preview_feature_set",
            serde_json::json!({"jobId": JOB_ID, "request": with_path}),
        )
        .is_err());
        let mut extra_field = request_json(&harness.dataset_id);
        extra_field["source_path"] = serde_json::json!("C:/MT5/Common/Files/EURUSD.csv");
        assert!(ipc(
            &harness.webview,
            "preview_feature_set",
            serde_json::json!({"jobId": OTHER_JOB_ID, "request": extra_field}),
        )
        .is_err());
        assert!(ipc(
            &harness.webview,
            "get_active_feature_job",
            serde_json::json!({})
        )
        .unwrap()
        .is_null());
        let attempts = ipc(
            &harness.webview,
            "list_feature_attempts",
            serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(attempts[0]["diagnostic_code"], "INVALID_REQUEST");
    }
}
