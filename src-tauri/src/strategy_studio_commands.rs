use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

use rg_feature_studio::store::FeatureStore;
use rg_persistence::DecisionAttemptRecord;
use rg_strategy_engine::{
    spec::{DecisionSemantics, RESEARCH_NOTICE, STRATEGY_ENGINE_ID, STRATEGY_ENGINE_VERSION},
    store::{
        DecisionCommitResult, DecisionInputCandidate, DecisionSetCatalogEntry, DecisionSetPreview,
        PreparedDecisionSet, StrategyStore,
    },
    strategy_definitions, DecisionProgress, DecisionSetRequest, ProgressSnapshot,
    StrategyDefinition, StrategyError, PREVIEW_ROWS,
};
use serde::Serialize;
use tauri::State;
use uuid::Uuid;

pub struct StrategyStudioState {
    store: Arc<StrategyStore>,
    start_lock: Mutex<()>,
    running: Mutex<Option<RunningDecisionJob>>,
    prepared: Mutex<Option<PreparedDecisionJob>>,
}

struct RunningDecisionJob {
    job_id: String,
    cancel: Arc<AtomicBool>,
    progress: Arc<DecisionProgress>,
}

struct PreparedDecisionJob {
    job_id: String,
    prepared: PreparedDecisionSet,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StrategyRegistrySnapshot {
    pub engine_id: &'static str,
    pub engine_version: &'static str,
    pub decision_semantics: DecisionSemantics,
    pub strategies: &'static [StrategyDefinition],
    pub preview_rows: usize,
    pub research_notice: &'static str,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DecisionPreviewSession {
    pub job_id: String,
    pub preview: DecisionSetPreview,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ActiveDecisionJob {
    Running {
        #[serde(rename = "jobId")]
        job_id: String,
        progress: ProgressSnapshot,
    },
    Prepared {
        session: Box<DecisionPreviewSession>,
    },
}

/// Handles that a started preview worker needs. Created by [`StrategyStudioState::begin_preview`].
pub struct PreviewWorker {
    pub store: Arc<StrategyStore>,
    pub cancel: Arc<AtomicBool>,
    pub progress: Arc<DecisionProgress>,
}

const UNAVAILABLE: &str = "strategy job state is unavailable";

impl StrategyStudioState {
    pub fn open(
        root: impl AsRef<Path>,
        features: Arc<FeatureStore>,
    ) -> Result<Self, StrategyError> {
        Ok(Self {
            store: Arc::new(StrategyStore::open(root, features)?),
            start_lock: Mutex::new(()),
            running: Mutex::new(None),
            prepared: Mutex::new(None),
        })
    }

    pub fn store(&self) -> Arc<StrategyStore> {
        Arc::clone(&self.store)
    }

    fn guard<'a, T>(mutex: &'a Mutex<T>) -> Result<MutexGuard<'a, T>, String> {
        mutex.lock().map_err(|_| UNAVAILABLE.to_owned())
    }

    /// Registers a new preview job. Only one strategy job may run or await confirmation at a time.
    pub fn begin_preview(&self, job_id: &str) -> Result<PreviewWorker, String> {
        validate_job_id(job_id)?;
        let _start = Self::guard(&self.start_lock)?;
        if Self::guard(&self.prepared)?.is_some() || Self::guard(&self.running)?.is_some() {
            return Err(
                "finish or cancel the existing decision preview before starting another".into(),
            );
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(DecisionProgress::default());
        *Self::guard(&self.running)? = Some(RunningDecisionJob {
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
        result: Result<PreparedDecisionSet, StrategyError>,
    ) -> Result<DecisionPreviewSession, String> {
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
                Err("strategy evaluation cancelled".into())
            }
            Ok(prepared) => {
                let session = DecisionPreviewSession {
                    job_id: job_id.to_owned(),
                    preview: prepared.preview.clone(),
                };
                *Self::guard(&self.prepared)? = Some(PreparedDecisionJob {
                    job_id: job_id.to_owned(),
                    prepared,
                });
                Ok(session)
            }
            Err(error) => {
                let status = if matches!(error, StrategyError::Cancelled) {
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

    fn active_job(&self) -> Result<Option<ActiveDecisionJob>, String> {
        let _start = Self::guard(&self.start_lock)?;
        if let Some(job) = Self::guard(&self.prepared)?.as_ref() {
            return Ok(Some(ActiveDecisionJob::Prepared {
                session: Box::new(DecisionPreviewSession {
                    job_id: job.job_id.clone(),
                    preview: job.prepared.preview.clone(),
                }),
            }));
        }
        Ok(Self::guard(&self.running)?
            .as_ref()
            .map(|job| ActiveDecisionJob::Running {
                job_id: job.job_id.clone(),
                progress: job.progress.snapshot(),
            }))
    }

    fn take_prepared(&self, job_id: &str) -> Result<PreparedDecisionSet, String> {
        validate_job_id(job_id)?;
        let _start = Self::guard(&self.start_lock)?;
        let mut prepared = Self::guard(&self.prepared)?;
        match prepared.take() {
            Some(job) if job.job_id == job_id => Ok(job.prepared),
            other => {
                *prepared = other;
                Err("no validated decision preview exists for this job".into())
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
            .map_err(|_| "no running or prepared strategy job exists for this job".to_owned())?;
        self.store
            .discard_prepared(prepared)
            .map_err(|error| error.to_string())?;
        Ok("CANCELLED".into())
    }
}

#[tauri::command]
pub fn get_strategy_registry() -> StrategyRegistrySnapshot {
    StrategyRegistrySnapshot {
        engine_id: STRATEGY_ENGINE_ID,
        engine_version: STRATEGY_ENGINE_VERSION,
        decision_semantics: DecisionSemantics::current(),
        strategies: strategy_definitions(),
        preview_rows: PREVIEW_ROWS,
        research_notice: RESEARCH_NOTICE,
    }
}

#[tauri::command]
pub async fn list_decision_inputs(
    state: State<'_, StrategyStudioState>,
) -> Result<Vec<DecisionInputCandidate>, String> {
    let store = Arc::clone(&state.store);
    tauri::async_runtime::spawn_blocking(move || {
        store.list_input_candidates(&AtomicBool::new(false))
    })
    .await
    .map_err(|error| format!("decision input worker failed: {error}"))?
    .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn preview_decision_set(
    state: State<'_, StrategyStudioState>,
    job_id: String,
    request: DecisionSetRequest,
) -> Result<DecisionPreviewSession, String> {
    let worker = state.begin_preview(&job_id)?;
    let result = tauri::async_runtime::spawn_blocking(move || {
        worker
            .store
            .prepare(&request, &worker.cancel, &worker.progress)
    })
    .await
    .unwrap_or_else(|error| {
        Err(StrategyError::InvalidPrepared(format!(
            "strategy worker failed: {error}"
        )))
    });
    state.finish_preview(&job_id, result)
}

#[tauri::command]
pub fn get_active_decision_job(
    state: State<'_, StrategyStudioState>,
) -> Result<Option<ActiveDecisionJob>, String> {
    state.active_job()
}

/// Commits the prepared preview. The result reports only the transactional commit; refreshing the
/// decision-set catalogue is a separate, non-transactional read.
#[tauri::command(rename_all = "camelCase")]
pub async fn confirm_decision_set(
    state: State<'_, StrategyStudioState>,
    job_id: String,
) -> Result<DecisionCommitResult, String> {
    let prepared = state.take_prepared(&job_id)?;
    let store = Arc::clone(&state.store);
    tauri::async_runtime::spawn_blocking(move || store.commit_prepared(prepared))
        .await
        .map_err(|error| format!("decision commit worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub fn cancel_decision_job(
    state: State<'_, StrategyStudioState>,
    job_id: String,
) -> Result<String, String> {
    state.cancel_job(&job_id)
}

#[tauri::command]
pub async fn list_decision_sets(
    state: State<'_, StrategyStudioState>,
) -> Result<Vec<DecisionSetCatalogEntry>, String> {
    let store = Arc::clone(&state.store);
    tauri::async_runtime::spawn_blocking(move || store.list_decision_sets(&AtomicBool::new(false)))
        .await
        .map_err(|error| format!("decision catalogue worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn inspect_decision_set(
    state: State<'_, StrategyStudioState>,
    decision_set_id: String,
) -> Result<Option<DecisionSetCatalogEntry>, String> {
    let store = Arc::clone(&state.store);
    tauri::async_runtime::spawn_blocking(move || store.inspect_decision_set(&decision_set_id))
        .await
        .map_err(|error| format!("decision inspection worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn list_decision_attempts(
    state: State<'_, StrategyStudioState>,
) -> Result<Vec<DecisionAttemptRecord>, String> {
    state
        .store
        .list_attempts()
        .map_err(|error| error.to_string())
}

fn validate_job_id(value: &str) -> Result<(), String> {
    Uuid::parse_str(value)
        .map(|_| ())
        .map_err(|_| "strategy job identity is invalid".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        data_ground_commands::DataGroundState, feature_studio_commands::FeatureStudioState,
        register_commands,
    };
    use rg_data_ground::{
        CalendarPolicy, CsvDelimiter, FieldMapping, ImportRequest, MarketRole, ResourceQuotas,
        TimestampSemantics,
    };
    use rg_feature_studio::{FeatureProgress, FeatureSetRequest};
    use std::fs;
    use tauri::{
        ipc::{CallbackFn, InvokeBody},
        test::{mock_builder, mock_context, noop_assets, MockRuntime, INVOKE_KEY},
        webview::{InvokeRequest, Webview, WebviewWindowBuilder},
        Manager,
    };
    use tempfile::TempDir;

    const JOB_ID: &str = "018f4c4f-7d9b-7b9f-a70e-92bb7cf04c91";
    const OTHER_JOB_ID: &str = "018f4c4f-7d9b-7b9f-a70e-92bb7cf04c92";

    /// Labeled synthetic bars: EURUSD.m H1 target (48 bars) and GBPUSD.m H4 context (12 bars).
    fn synthetic_csv() -> String {
        let mut text = String::from("timestamp,symbol,timeframe,open,high,low,close,volume\n");
        for (symbol, timeframe, hours, count) in
            [("EURUSD.m", "H1", 1, 48), ("GBPUSD.m", "H4", 4, 12)]
        {
            for index in 0..count {
                let x = f64::from(index);
                let close = 1.1 + 0.012 * (x * 0.31).sin() + 0.003 * (x * 0.083).cos();
                let hour = index * hours;
                text.push_str(&format!(
                    "2024-01-{:02}T{:02}:00:00Z,{symbol},{timeframe},{close},{},{},{close},1\n",
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
        let source = directory.path().join("synthetic-fixture-strategy.csv");
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
                    source_id: "synthetic-fixture:strategy-ipc".into(),
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

    fn feature_request(dataset_id: &str) -> FeatureSetRequest {
        serde_json::from_value(serde_json::json!({
            "target": {"dataset_id": dataset_id, "symbol": "EURUSD.m", "timeframe": "H1"},
            "features": [
                {"key": "fast", "indicator_id": "SMA", "indicator_version": 1,
                 "parameters": {"period": 3}, "source": {"kind": "TARGET"}},
                {"key": "slow", "indicator_id": "SMA", "indicator_version": 1,
                 "parameters": {"period": 8}, "source": {"kind": "TARGET"}},
                {"key": "roc", "indicator_id": "ROC", "indicator_version": 1,
                 "parameters": {"period": 2}, "source": {"kind": "TARGET"}},
                {"key": "ctx", "indicator_id": "RSI_WILDER", "indicator_version": 1,
                 "parameters": {"period": 3},
                 "source": {"kind": "REFERENCE",
                            "series": {"dataset_id": dataset_id, "symbol": "GBPUSD.m", "timeframe": "H4"},
                            "max_staleness_seconds": 28800, "availability_lag_seconds": 0}}
            ],
            "quotas": {"max_target_rows": 1000, "max_input_rows": 1000, "max_output_bytes": 10000000}
        }))
        .unwrap()
    }

    fn band_request(feature_set_id: &str) -> serde_json::Value {
        serde_json::json!({
            "feature_set_id": feature_set_id,
            "strategy_id": "THRESHOLD_BAND",
            "strategy_version": 1,
            "parameters": {"upper": 0.3, "lower": -0.3},
            "bindings": {"signal": {"feature_key": "roc", "output": "value"}},
            "quotas": {"max_rows": 1000, "max_output_bytes": 10000000}
        })
    }

    fn trend_request(feature_set_id: &str) -> serde_json::Value {
        serde_json::json!({
            "feature_set_id": feature_set_id,
            "strategy_id": "TREND_CONTEXT",
            "strategy_version": 1,
            "parameters": {},
            "bindings": {
                "fast": {"feature_key": "fast", "output": "value"},
                "slow": {"feature_key": "slow", "output": "value"},
                "context": {"feature_key": "ctx", "output": "value"}
            },
            "quotas": {"max_rows": 1000, "max_output_bytes": 10000000}
        })
    }

    /// The desktop shell as `lib::run` assembles it, over `root` instead of the OS app-data
    /// directory.
    fn desktop_app(root: &Path) -> (tauri::App<MockRuntime>, tauri::WebviewWindow<MockRuntime>) {
        let data = DataGroundState::open(root).unwrap();
        let features = FeatureStudioState::open(root, data.store()).unwrap();
        let strategies = StrategyStudioState::open(root, features.store()).unwrap();
        let app = register_commands(mock_builder())
            .manage(data)
            .manage(features)
            .manage(strategies)
            .build(mock_context(noop_assets()))
            .expect("mock Tauri runtime should build");
        let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .expect("mock webview should build");
        (app, webview)
    }

    struct Harness {
        directory: TempDir,
        app: tauri::App<MockRuntime>,
        webview: tauri::WebviewWindow<MockRuntime>,
        feature_set_id: String,
    }

    impl Harness {
        fn root(&self) -> std::path::PathBuf {
            self.directory.path().join("app-data")
        }
    }

    fn harness() -> Harness {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("app-data");
        let (app, webview) = desktop_app(&root);
        let dataset_id = import_fixture(&app.state::<DataGroundState>(), &directory);
        let features = app.state::<FeatureStudioState>().store();
        let prepared = features
            .prepare(
                &feature_request(&dataset_id),
                &AtomicBool::new(false),
                &FeatureProgress::default(),
            )
            .unwrap();
        let feature_set_id = features.commit_prepared(prepared).unwrap().feature_set_id;
        Harness {
            directory,
            app,
            webview,
            feature_set_id,
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

    fn empty(webview: &tauri::WebviewWindow<MockRuntime>, command: &str) -> serde_json::Value {
        ipc(webview, command, serde_json::json!({})).unwrap()
    }

    #[test]
    fn gate_desktop_strategy_ipc_previews_restores_and_confirms_a_decision_set() {
        let harness = harness();
        let webview = &harness.webview;
        let registry = empty(webview, "get_strategy_registry");
        assert_eq!(registry["strategies"].as_array().unwrap().len(), 2);
        assert_eq!(registry["decisionSemantics"]["version"], 1);
        assert!(registry["researchNotice"]
            .as_str()
            .unwrap()
            .contains("not orders"));

        let inputs = empty(webview, "list_decision_inputs");
        assert_eq!(inputs[0]["feature_set_id"], harness.feature_set_id.as_str());
        assert_eq!(inputs[0]["eligible"], true);
        assert_eq!(inputs[0]["target"]["symbol"], "EURUSD.m");

        let preview = ipc(
            webview,
            "preview_decision_set",
            serde_json::json!({"jobId": JOB_ID, "request": band_request(&harness.feature_set_id)}),
        )
        .unwrap();
        assert_eq!(preview["jobId"], JOB_ID);
        assert_eq!(preview["preview"]["manifest"]["row_count"], 48);
        assert_eq!(
            preview["preview"]["sample_rows"].as_array().unwrap().len(),
            PREVIEW_ROWS
        );
        assert!(
            empty(webview, "list_decision_sets")
                .as_array()
                .unwrap()
                .is_empty(),
            "preview must not register"
        );
        assert!(
            ipc(
                webview,
                "preview_decision_set",
                serde_json::json!({"jobId": OTHER_JOB_ID, "request": band_request(&harness.feature_set_id)}),
            )
            .is_err(),
            "only one strategy job may be pending"
        );
        let restored = empty(webview, "get_active_decision_job");
        assert_eq!(restored["state"], "PREPARED");
        assert_eq!(restored["session"]["jobId"], JOB_ID);

        let committed = ipc(
            webview,
            "confirm_decision_set",
            serde_json::json!({"jobId": JOB_ID}),
        )
        .unwrap();
        assert_eq!(committed["disposition"], "MATERIALIZED");
        assert_eq!(committed["status"], "READY");
        assert_eq!(
            committed["decision_set_id"],
            preview["preview"]["decision_set_id"]
        );
        assert!(empty(webview, "get_active_decision_job").is_null());
        let catalog = empty(webview, "list_decision_sets");
        assert_eq!(catalog.as_array().unwrap().len(), 1);
        assert_eq!(catalog[0]["status"], "READY");
        let inspected = ipc(
            webview,
            "inspect_decision_set",
            serde_json::json!({"decisionSetId": committed["decision_set_id"]}),
        )
        .unwrap();
        assert_eq!(inspected["integrity"]["lineage_valid"], true);
        assert_eq!(
            inspected["manifest"]["lineage"]["feature_set_id"],
            harness.feature_set_id.as_str()
        );
        assert_eq!(
            empty(webview, "list_decision_attempts")[0]["status"],
            "MATERIALIZED"
        );
    }

    #[test]
    fn gate_desktop_strategy_ipc_context_markets_produce_no_separate_decisions() {
        let harness = harness();
        let preview = ipc(
            &harness.webview,
            "preview_decision_set",
            serde_json::json!({"jobId": JOB_ID, "request": trend_request(&harness.feature_set_id)}),
        )
        .unwrap();
        let manifest = &preview["preview"]["manifest"];
        assert_eq!(manifest["row_count"], 48);
        assert_eq!(manifest["lineage"]["target"]["symbol"], "EURUSD.m");
        assert_eq!(
            manifest["lineage"]["context_series"][0]["symbol"],
            "GBPUSD.m"
        );
        for row in preview["preview"]["sample_rows"].as_array().unwrap() {
            assert!(matches!(
                row["decision"].as_str(),
                Some("BUY" | "SELL" | "SKIP")
            ));
            let roles: Vec<_> = row["inputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|input| input["role"].as_str().unwrap())
                .collect();
            assert_eq!(roles, ["fast", "slow", "context"]);
        }
        let summary = &manifest["summary"];
        let total = summary["buy_rows"].as_u64().unwrap()
            + summary["sell_rows"].as_u64().unwrap()
            + summary["skip_rows"].as_u64().unwrap();
        assert_eq!(total, 48, "12 H4 context bars add no decision rows");
    }

    #[test]
    fn gate_desktop_strategy_ipc_cancels_a_running_job_and_discards_a_late_preview() {
        let harness = harness();
        let state = harness.app.state::<StrategyStudioState>();
        let worker = state.begin_preview(JOB_ID).unwrap();
        let running = empty(&harness.webview, "get_active_decision_job");
        assert_eq!(running["state"], "RUNNING");
        assert_eq!(
            ipc(
                &harness.webview,
                "cancel_decision_job",
                serde_json::json!({"jobId": JOB_ID})
            )
            .unwrap(),
            "CANCELLATION_REQUESTED"
        );
        let request: DecisionSetRequest =
            serde_json::from_value(band_request(&harness.feature_set_id)).unwrap();
        let result = worker
            .store
            .prepare(&request, &worker.cancel, &worker.progress);
        assert!(matches!(result, Err(StrategyError::Cancelled)));
        assert!(state.finish_preview(JOB_ID, result).is_err());
        assert!(empty(&harness.webview, "get_active_decision_job").is_null());
        assert_eq!(
            empty(&harness.webview, "list_decision_attempts")[0]["status"],
            "CANCELLED"
        );

        // A preview that completes after cancellation was requested is discarded, not offered.
        let worker = state.begin_preview(OTHER_JOB_ID).unwrap();
        let result = worker
            .store
            .prepare(&request, &AtomicBool::new(false), &worker.progress);
        assert!(result.is_ok());
        assert_eq!(
            state.cancel_job(OTHER_JOB_ID).unwrap(),
            "CANCELLATION_REQUESTED"
        );
        assert!(state.finish_preview(OTHER_JOB_ID, result).is_err());
        assert!(state.active_job().unwrap().is_none());
        assert_eq!(
            fs::read_dir(worker.store.staging_directory_for_tests())
                .unwrap()
                .count(),
            0
        );
        assert!(empty(&harness.webview, "list_decision_sets")
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn gate_desktop_strategy_ipc_failure_after_confirm_leaves_no_stale_preview() {
        let harness = harness();
        let preview = ipc(
            &harness.webview,
            "preview_decision_set",
            serde_json::json!({"jobId": JOB_ID, "request": band_request(&harness.feature_set_id)}),
        )
        .unwrap();
        let stage = harness
            .app
            .state::<StrategyStudioState>()
            .prepared
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .prepared
            .staging_directory
            .clone();
        fs::write(stage.join("decisions.parquet"), b"changed after preview").unwrap();
        assert!(ipc(
            &harness.webview,
            "confirm_decision_set",
            serde_json::json!({"jobId": JOB_ID})
        )
        .is_err());
        assert!(empty(&harness.webview, "get_active_decision_job").is_null());
        assert!(ipc(
            &harness.webview,
            "confirm_decision_set",
            serde_json::json!({"jobId": JOB_ID})
        )
        .is_err());
        assert!(ipc(
            &harness.webview,
            "inspect_decision_set",
            serde_json::json!({"decisionSetId": preview["preview"]["decision_set_id"]}),
        )
        .unwrap()
        .is_null());
    }

    #[test]
    fn gate_desktop_strategy_ipc_end_to_end_workflow_survives_restart_and_revokes_on_tamper() {
        let harness = harness();
        let root = harness.root();
        let decision_set_id = {
            let webview = &harness.webview;
            ipc(
                webview,
                "preview_decision_set",
                serde_json::json!({"jobId": JOB_ID, "request": trend_request(&harness.feature_set_id)}),
            )
            .unwrap();
            ipc(
                webview,
                "confirm_decision_set",
                serde_json::json!({"jobId": JOB_ID}),
            )
            .unwrap()["decision_set_id"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        let Harness {
            directory,
            app,
            webview,
            feature_set_id,
        } = harness;
        drop(webview);
        drop(app);

        // Close and reopen the desktop shell over the same storage.
        let (app, webview) = desktop_app(&root);
        let catalog = empty(&webview, "list_decision_sets");
        assert_eq!(catalog.as_array().unwrap().len(), 1);
        assert_eq!(catalog[0]["decision_set_id"], decision_set_id.as_str());
        assert_eq!(catalog[0]["status"], "READY");
        assert!(empty(&webview, "get_active_decision_job").is_null());
        assert_eq!(
            empty(&webview, "list_decision_attempts")[0]["status"],
            "MATERIALIZED"
        );
        ipc(
            &webview,
            "preview_decision_set",
            serde_json::json!({"jobId": OTHER_JOB_ID, "request": trend_request(&feature_set_id)}),
        )
        .unwrap();
        let reused = ipc(
            &webview,
            "confirm_decision_set",
            serde_json::json!({"jobId": OTHER_JOB_ID}),
        )
        .unwrap();
        assert_eq!(reused["disposition"], "REUSED");

        // Tampering with the consumed feature set revokes the decision set and its eligibility.
        let output = root
            .join("feature-sets")
            .join(&feature_set_id)
            .join("features.parquet");
        let mut bytes = fs::read(&output).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0x5a;
        fs::write(&output, bytes).unwrap();
        let inspected = ipc(
            &webview,
            "inspect_decision_set",
            serde_json::json!({"decisionSetId": decision_set_id}),
        )
        .unwrap();
        assert_eq!(inspected["status"], "QUARANTINED");
        assert_eq!(inspected["integrity"]["lineage_valid"], false);
        assert_eq!(
            empty(&webview, "list_decision_inputs")[0]["eligible"],
            false
        );
        let blocked = ipc(
            &webview,
            "preview_decision_set",
            serde_json::json!({"jobId": JOB_ID, "request": trend_request(&feature_set_id)}),
        )
        .unwrap_err();
        assert!(blocked.as_str().unwrap().contains(&feature_set_id));
        drop(app);
        drop(directory);
    }

    #[test]
    fn gate_desktop_strategy_ipc_accepts_registry_identities_never_paths_or_order_fields() {
        let harness = harness();
        let mut path = band_request(&harness.feature_set_id);
        path["feature_set_id"] = serde_json::json!(
            r"C:\Users\fixture\AppData\Roaming\MetaQuotes\Terminal\Common\Files\EURUSD.csv"
        );
        assert!(ipc(
            &harness.webview,
            "preview_decision_set",
            serde_json::json!({"jobId": JOB_ID, "request": path}),
        )
        .is_err());
        let mut order = band_request(&harness.feature_set_id);
        order["volume_lots"] = serde_json::json!(1.0);
        assert!(ipc(
            &harness.webview,
            "preview_decision_set",
            serde_json::json!({"jobId": OTHER_JOB_ID, "request": order}),
        )
        .is_err());
        assert!(empty(&harness.webview, "get_active_decision_job").is_null());
        assert_eq!(
            empty(&harness.webview, "list_decision_attempts")[0]["diagnostic_code"],
            "INVALID_REQUEST"
        );
    }
}
