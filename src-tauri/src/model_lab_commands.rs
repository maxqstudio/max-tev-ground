use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

use rg_model_registry::{
    compare::{ComparisonReport, ComparisonRequest, COMPARISON_NOTICE},
    onnx::{MAX_OPSET, MIN_OPSET, SUPPORTED_OPERATORS},
    spec::{
        InferenceSemantics, INFERENCE_NOTICE, LOCKED_AUTHORITIES, MAX_REQUEST_TIMEOUT_MS,
        MIN_REQUEST_TIMEOUT_MS, MODEL_NOTICE, MODEL_REGISTRY_VERSION, OLLAMA_CONTEXT_TOKENS,
        OLLAMA_OUTPUT_TOKENS,
    },
    store::{
        InferenceCommitResult, InferenceInputCandidate, InferencePreview,
        InferenceRecordCatalogEntry, ModelCatalogEntry, ModelLabStore, OllamaDiscovery,
        PreparedInference, RegistrationPreview, RegistrationResult,
    },
    InferenceProgress, InferenceRequest, ModelCandidate, ModelError, ModelRegistrationRequest,
    ProgressSnapshot, DEFAULT_OLLAMA_ENDPOINT, PREVIEW_ROWS,
};
use rg_persistence::ModelLabAttemptRecord;
use rg_strategy_engine::store::StrategyStore;
use serde::Serialize;
use tauri::{AppHandle, Runtime, State};
use tauri_plugin_dialog::DialogExt;
use uuid::Uuid;

pub struct ModelLabState {
    store: Arc<ModelLabStore>,
    start_lock: Mutex<()>,
    running: Mutex<Option<RunningInferenceJob>>,
    prepared: Mutex<Option<PreparedInferenceJob>>,
}

struct RunningInferenceJob {
    job_id: String,
    cancel: Arc<AtomicBool>,
    progress: Arc<InferenceProgress>,
}

struct PreparedInferenceJob {
    job_id: String,
    prepared: PreparedInference,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterDescription {
    pub id: &'static str,
    pub version: &'static str,
    pub description: &'static str,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelLabRegistrySnapshot {
    pub registry_version: &'static str,
    pub adapters: Vec<AdapterDescription>,
    pub default_ollama_endpoint: &'static str,
    pub onnx_operators: &'static [&'static str],
    pub onnx_opsets: (i64, i64),
    pub max_onnx_bytes: u64,
    pub ollama_output_tokens: (u32, u32),
    pub ollama_context_tokens: (u32, u32),
    pub request_timeout_ms: (u64, u64),
    pub executable_authority: &'static str,
    pub locked_authorities: &'static [&'static str],
    pub inference_semantics: InferenceSemantics,
    pub preview_rows: usize,
    pub model_notice: &'static str,
    pub inference_notice: &'static str,
    pub comparison_notice: &'static str,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InferencePreviewSession {
    pub job_id: String,
    pub preview: InferencePreview,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ActiveInferenceJob {
    Running {
        #[serde(rename = "jobId")]
        job_id: String,
        progress: ProgressSnapshot,
    },
    Prepared {
        session: Box<InferencePreviewSession>,
    },
}

/// Handles that a started preview worker needs. Created by [`ModelLabState::begin_preview`].
pub struct PreviewWorker {
    pub store: Arc<ModelLabStore>,
    pub cancel: Arc<AtomicBool>,
    pub progress: Arc<InferenceProgress>,
}

const UNAVAILABLE: &str = "model lab job state is unavailable";

impl ModelLabState {
    pub fn open(
        root: impl AsRef<Path>,
        strategies: Arc<StrategyStore>,
    ) -> Result<Self, ModelError> {
        Ok(Self {
            store: Arc::new(ModelLabStore::open(root, strategies)?),
            start_lock: Mutex::new(()),
            running: Mutex::new(None),
            prepared: Mutex::new(None),
        })
    }

    pub fn store(&self) -> Arc<ModelLabStore> {
        Arc::clone(&self.store)
    }

    fn guard<'a, T>(mutex: &'a Mutex<T>) -> Result<MutexGuard<'a, T>, String> {
        mutex.lock().map_err(|_| UNAVAILABLE.to_owned())
    }

    /// Registers a new inference preview job. Only one may run or await confirmation at a time.
    pub fn begin_preview(&self, job_id: &str) -> Result<PreviewWorker, String> {
        validate_job_id(job_id)?;
        let _start = Self::guard(&self.start_lock)?;
        if Self::guard(&self.prepared)?.is_some() || Self::guard(&self.running)?.is_some() {
            return Err(
                "finish or cancel the existing inference preview before starting another".into(),
            );
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(InferenceProgress::default());
        *Self::guard(&self.running)? = Some(RunningInferenceJob {
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
        result: Result<PreparedInference, ModelError>,
    ) -> Result<InferencePreviewSession, String> {
        let _start = Self::guard(&self.start_lock)?;
        let running = Self::guard(&self.running)?.take();
        let cancelled = running
            .as_ref()
            .is_some_and(|job| job.cancel.load(Ordering::Relaxed));
        match result {
            Ok(prepared) if cancelled => {
                self.store
                    .discard_inference(prepared)
                    .map_err(|error| format!("cancelled preview cleanup failed: {error}"))?;
                Err("inference cancelled".into())
            }
            Ok(prepared) => {
                let session = InferencePreviewSession {
                    job_id: job_id.to_owned(),
                    preview: prepared.preview.clone(),
                };
                *Self::guard(&self.prepared)? = Some(PreparedInferenceJob {
                    job_id: job_id.to_owned(),
                    prepared,
                });
                Ok(session)
            }
            Err(error) => {
                let status = if matches!(error, ModelError::Cancelled) {
                    "CANCELLED"
                } else {
                    "FAILED"
                };
                let _ = self.store.record_failure(
                    "INFERENCE",
                    status,
                    error.code(),
                    &error.to_string(),
                );
                Err(error.to_string())
            }
        }
    }

    pub fn active_job(&self) -> Result<Option<ActiveInferenceJob>, String> {
        let _start = Self::guard(&self.start_lock)?;
        if let Some(job) = Self::guard(&self.prepared)?.as_ref() {
            return Ok(Some(ActiveInferenceJob::Prepared {
                session: Box::new(InferencePreviewSession {
                    job_id: job.job_id.clone(),
                    preview: job.prepared.preview.clone(),
                }),
            }));
        }
        Ok(Self::guard(&self.running)?
            .as_ref()
            .map(|job| ActiveInferenceJob::Running {
                job_id: job.job_id.clone(),
                progress: job.progress.snapshot(),
            }))
    }

    fn take_prepared(&self, job_id: &str) -> Result<PreparedInference, String> {
        validate_job_id(job_id)?;
        let _start = Self::guard(&self.start_lock)?;
        let mut prepared = Self::guard(&self.prepared)?;
        match prepared.take() {
            Some(job) if job.job_id == job_id => Ok(job.prepared),
            other => {
                *prepared = other;
                Err("no validated inference preview exists for this job".into())
            }
        }
    }

    pub fn cancel_job(&self, job_id: &str) -> Result<String, String> {
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
            .map_err(|_| "no running or prepared inference job exists for this job".to_owned())?;
        self.store
            .discard_inference(prepared)
            .map_err(|error| error.to_string())?;
        Ok("CANCELLED".into())
    }
}

async fn blocking<T: Send + 'static>(
    what: &str,
    work: impl FnOnce() -> Result<T, ModelError> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|error| format!("{what} worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn get_model_lab_registry(state: State<'_, ModelLabState>) -> ModelLabRegistrySnapshot {
    ModelLabRegistrySnapshot {
        registry_version: MODEL_REGISTRY_VERSION,
        adapters: vec![
            AdapterDescription {
                id: "OLLAMA_LOOPBACK",
                version: "ollama-generate-json/1",
                description: "Models already installed in an Ollama runtime on this computer, reached only through a loopback address. Models are never pulled or downloaded.",
            },
            AdapterDescription {
                id: "ONNX_NATIVE",
                version: "rg-native-onnx/1",
                description: "A selected .onnx file copied into application storage and evaluated by the built-in interpreter, for the listed operators only.",
            },
        ],
        default_ollama_endpoint: DEFAULT_OLLAMA_ENDPOINT,
        onnx_operators: SUPPORTED_OPERATORS,
        onnx_opsets: (MIN_OPSET, MAX_OPSET),
        max_onnx_bytes: state.store.limits().max_onnx_bytes,
        ollama_output_tokens: OLLAMA_OUTPUT_TOKENS,
        ollama_context_tokens: OLLAMA_CONTEXT_TOKENS,
        request_timeout_ms: (MIN_REQUEST_TIMEOUT_MS, MAX_REQUEST_TIMEOUT_MS),
        executable_authority: "ADVISORY_ONLY",
        locked_authorities: LOCKED_AUTHORITIES,
        inference_semantics: InferenceSemantics::current(),
        preview_rows: PREVIEW_ROWS,
        model_notice: MODEL_NOTICE,
        inference_notice: INFERENCE_NOTICE,
        comparison_notice: COMPARISON_NOTICE,
    }
}

#[tauri::command]
pub fn get_ollama_endpoint(state: State<'_, ModelLabState>) -> Result<String, String> {
    state
        .store
        .ollama_endpoint()
        .map(|endpoint| endpoint.to_string())
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn set_ollama_endpoint(
    state: State<'_, ModelLabState>,
    endpoint: String,
) -> Result<String, String> {
    state
        .store
        .set_ollama_endpoint(&endpoint)
        .map(|endpoint| endpoint.to_string())
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn discover_ollama_models(
    state: State<'_, ModelLabState>,
) -> Result<OllamaDiscovery, String> {
    let store = state.store();
    blocking("model discovery", move || store.discover_ollama()).await
}

#[tauri::command]
pub async fn select_onnx_model<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, ModelLabState>,
) -> Result<Option<ModelCandidate>, String> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    app.dialog()
        .file()
        .add_filter("ONNX model", &["onnx"])
        .pick_file(move |selection| {
            let result = selection
                .map(|selection| {
                    selection
                        .into_path()
                        .map_err(|error| format!("selected path could not be resolved: {error}"))
                })
                .transpose();
            let _ = sender.send(result);
        });
    let selection = tauri::async_runtime::spawn_blocking(move || receiver.recv())
        .await
        .map_err(|error| format!("file dialog worker failed: {error}"))?
        .map_err(|error| format!("file dialog callback failed: {error}"))??;
    let Some(path) = selection else {
        return Ok(None);
    };
    let store = state.store();
    blocking("ONNX staging", move || {
        store.stage_onnx_candidate(&path, &AtomicBool::new(false))
    })
    .await
    .map(Some)
}

#[tauri::command]
pub fn list_onnx_candidates(
    state: State<'_, ModelLabState>,
) -> Result<Vec<ModelCandidate>, String> {
    state
        .store
        .list_onnx_candidates()
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub fn discard_onnx_candidate(
    state: State<'_, ModelLabState>,
    candidate_id: String,
) -> Result<(), String> {
    state
        .store
        .discard_onnx_candidate(&candidate_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn preview_model_registration(
    state: State<'_, ModelLabState>,
    request: ModelRegistrationRequest,
) -> Result<RegistrationPreview, String> {
    let store = state.store();
    blocking("registration preview", move || {
        store.preview_registration(&request)
    })
    .await
}

/// Registers the model the Owner confirmed. The request must reproduce the confirmed model ID.
#[tauri::command(rename_all = "camelCase")]
pub async fn register_model(
    state: State<'_, ModelLabState>,
    request: ModelRegistrationRequest,
    confirmed_model_id: String,
) -> Result<RegistrationResult, String> {
    let store = state.store();
    blocking("model registration", move || {
        store.register_model(&request, &confirmed_model_id)
    })
    .await
}

#[tauri::command]
pub async fn list_models(
    state: State<'_, ModelLabState>,
) -> Result<Vec<ModelCatalogEntry>, String> {
    let store = state.store();
    blocking("model catalogue", move || {
        store.list_models(&AtomicBool::new(false))
    })
    .await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn inspect_model(
    state: State<'_, ModelLabState>,
    model_id: String,
) -> Result<Option<ModelCatalogEntry>, String> {
    let store = state.store();
    blocking("model inspection", move || store.inspect_model(&model_id)).await
}

#[tauri::command]
pub async fn list_inference_inputs(
    state: State<'_, ModelLabState>,
) -> Result<Vec<InferenceInputCandidate>, String> {
    let store = state.store();
    blocking("inference input", move || {
        store.list_inference_inputs(&AtomicBool::new(false))
    })
    .await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn preview_inference(
    state: State<'_, ModelLabState>,
    job_id: String,
    request: InferenceRequest,
) -> Result<InferencePreviewSession, String> {
    let worker = state.begin_preview(&job_id)?;
    let result = tauri::async_runtime::spawn_blocking(move || {
        worker
            .store
            .prepare_inference(&request, &worker.cancel, &worker.progress)
    })
    .await
    .unwrap_or_else(|error| {
        Err(ModelError::InvalidPrepared(format!(
            "inference worker failed: {error}"
        )))
    });
    state.finish_preview(&job_id, result)
}

#[tauri::command]
pub fn get_active_inference_job(
    state: State<'_, ModelLabState>,
) -> Result<Option<ActiveInferenceJob>, String> {
    state.active_job()
}

/// Commits the prepared preview. The result reports only the transactional commit; refreshing the
/// record catalogue is a separate, non-transactional read.
#[tauri::command(rename_all = "camelCase")]
pub async fn confirm_inference(
    state: State<'_, ModelLabState>,
    job_id: String,
) -> Result<InferenceCommitResult, String> {
    let prepared = state.take_prepared(&job_id)?;
    let store = state.store();
    blocking("inference commit", move || store.commit_inference(prepared)).await
}

#[tauri::command(rename_all = "camelCase")]
pub fn cancel_inference_job(
    state: State<'_, ModelLabState>,
    job_id: String,
) -> Result<String, String> {
    state.cancel_job(&job_id)
}

#[tauri::command]
pub async fn list_inference_records(
    state: State<'_, ModelLabState>,
) -> Result<Vec<InferenceRecordCatalogEntry>, String> {
    let store = state.store();
    blocking("inference catalogue", move || {
        store.list_inference_records(&AtomicBool::new(false))
    })
    .await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn inspect_inference_record(
    state: State<'_, ModelLabState>,
    record_id: String,
) -> Result<Option<InferenceRecordCatalogEntry>, String> {
    let store = state.store();
    blocking("inference inspection", move || {
        store.inspect_inference_record(&record_id)
    })
    .await
}

#[tauri::command]
pub async fn compare_inference_records(
    state: State<'_, ModelLabState>,
    request: ComparisonRequest,
) -> Result<ComparisonReport, String> {
    let store = state.store();
    blocking("comparison", move || {
        store.compare(&request, &AtomicBool::new(false))
    })
    .await
}

#[tauri::command]
pub fn list_model_lab_attempts(
    state: State<'_, ModelLabState>,
) -> Result<Vec<ModelLabAttemptRecord>, String> {
    state
        .store
        .list_attempts()
        .map_err(|error| error.to_string())
}

fn validate_job_id(value: &str) -> Result<(), String> {
    Uuid::parse_str(value)
        .map(|_| ())
        .map_err(|_| "inference job identity is invalid".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        data_ground_commands::DataGroundState, feature_studio_commands::FeatureStudioState,
        register_commands, strategy_studio_commands::StrategyStudioState,
    };
    use rg_data_ground::{
        CalendarPolicy, CsvDelimiter, FieldMapping, ImportRequest, MarketRole, ResourceQuotas,
        TimestampSemantics,
    };
    use rg_feature_studio::{FeatureProgress, FeatureSetRequest};
    use rg_model_registry::fixtures::{
        ollama::{MockBehavior, MockOllama, FIXTURE_DIGEST, FIXTURE_MODEL},
        onnx,
    };
    use std::fs;
    use tauri::{
        ipc::{CallbackFn, InvokeBody},
        test::{mock_builder, mock_context, noop_assets, MockRuntime, INVOKE_KEY},
        webview::{InvokeRequest, Webview, WebviewWindowBuilder},
        Manager,
    };
    use tempfile::TempDir;

    const JOB_ID: &str = "018f4c4f-7d9b-7b9f-a70e-92bb7cf04ca1";
    const OTHER_JOB_ID: &str = "018f4c4f-7d9b-7b9f-a70e-92bb7cf04ca2";

    /// Labeled synthetic bars: EURUSD.m H1, 48 bars from a closed-form path.
    fn synthetic_csv() -> String {
        let mut text = String::from("timestamp,symbol,timeframe,open,high,low,close,volume\n");
        for index in 0..48 {
            let x = f64::from(index);
            let close = 1.1 + 0.012 * (x * 0.31).sin() + 0.003 * (x * 0.083).cos();
            text.push_str(&format!(
                "2024-01-{:02}T{:02}:00:00Z,EURUSD.m,H1,{close},{},{},{close},1\n",
                1 + index / 24,
                index % 24,
                close + 0.0005,
                close - 0.0005
            ));
        }
        text
    }

    fn import_fixture(data: &DataGroundState, directory: &TempDir) -> String {
        let source = directory.path().join("synthetic-fixture-model-lab.csv");
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
                    source_id: "synthetic-fixture:model-lab-ipc".into(),
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
                 "parameters": {"period": 8}, "source": {"kind": "TARGET"}}
            ],
            "quotas": {"max_target_rows": 1000, "max_input_rows": 1000, "max_output_bytes": 10000000}
        }))
        .unwrap()
    }

    fn desktop_app(root: &Path) -> (tauri::App<MockRuntime>, tauri::WebviewWindow<MockRuntime>) {
        let data = DataGroundState::open(root).unwrap();
        let features = FeatureStudioState::open(root, data.store()).unwrap();
        let strategies = StrategyStudioState::open(root, features.store()).unwrap();
        let models = ModelLabState::open(root, strategies.store()).unwrap();
        let app = register_commands(mock_builder())
            .manage(data)
            .manage(features)
            .manage(strategies)
            .manage(models)
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

        /// Stages an ONNX fixture the way `select_onnx_model` does after the native dialog.
        fn stage(&self, name: &str, bytes: &[u8]) -> serde_json::Value {
            let path = self.directory.path().join(name);
            fs::write(&path, bytes).unwrap();
            let candidate = self
                .app
                .state::<ModelLabState>()
                .store()
                .stage_onnx_candidate(&path, &AtomicBool::new(false))
                .unwrap();
            serde_json::to_value(candidate).unwrap()
        }

        fn register_spread(&self, name: &str, bias: f32) -> String {
            let candidate = self.stage(name, &onnx::linear_model(2, &[1.0, -1.0], &[bias]));
            let request = onnx_registration(candidate["candidate_id"].as_str().unwrap());
            let preview = ipc(
                &self.webview,
                "preview_model_registration",
                serde_json::json!({"request": request}),
            )
            .unwrap();
            let registered = ipc(
                &self.webview,
                "register_model",
                serde_json::json!({"request": request, "confirmedModelId": preview["model_id"]}),
            )
            .unwrap();
            registered["model_id"].as_str().unwrap().to_owned()
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

    fn onnx_registration(candidate_id: &str) -> serde_json::Value {
        serde_json::json!({
            "candidate_id": candidate_id,
            "input_fields": ["fast", "slow"],
            "outputs": [{"name": "spread", "kind": {"kind": "FLOAT64", "minimum": -10.0, "maximum": 10.0}}],
            "runtime": {"adapter": "ONNX_NATIVE"},
            "authority": "ADVISORY_ONLY"
        })
    }

    fn inference_request(model_id: &str, feature_set_id: &str) -> serde_json::Value {
        serde_json::json!({
            "model_id": model_id,
            "input": {"kind": "FEATURE_SET", "feature_set_id": feature_set_id},
            "bindings": {
                "fast": {"source": "FEATURE", "feature_key": "fast", "output": "value"},
                "slow": {"source": "FEATURE", "feature_key": "slow", "output": "value"}
            },
            "authority": "ADVISORY_ONLY",
            "quotas": {"max_rows": 1000, "max_output_bytes": 10000000, "request_timeout_ms": 5000}
        })
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
    fn gate_desktop_model_lab_ipc_registers_previews_confirms_compares_and_inspects_lineage() {
        let harness = harness();
        let webview = &harness.webview;
        let registry = empty(webview, "get_model_lab_registry");
        assert_eq!(registry["executableAuthority"], "ADVISORY_ONLY");
        assert_eq!(registry["adapters"].as_array().unwrap().len(), 2);
        assert!(registry["onnxOperators"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("Gemm")));
        assert!(registry["comparisonNotice"]
            .as_str()
            .unwrap()
            .contains("not accuracy against outcomes"));

        let first = harness.register_spread("spread.onnx", 0.0);
        let second = harness.register_spread("shifted.onnx", 0.25);
        let models = empty(webview, "list_models");
        assert_eq!(models.as_array().unwrap().len(), 2);
        assert!(models
            .as_array()
            .unwrap()
            .iter()
            .all(|model| model["status"] == "READY"));

        let inputs = empty(webview, "list_inference_inputs");
        assert_eq!(inputs[0]["id"], harness.feature_set_id.as_str());
        assert_eq!(inputs[0]["eligible"], true);

        let preview = ipc(
            webview,
            "preview_inference",
            serde_json::json!({"jobId": JOB_ID, "request": inference_request(&first, &harness.feature_set_id)}),
        )
        .unwrap();
        assert_eq!(preview["jobId"], JOB_ID);
        assert_eq!(preview["preview"]["manifest"]["row_count"], 48);
        assert_eq!(preview["preview"]["manifest"]["authority"], "ADVISORY_ONLY");
        assert_eq!(
            preview["preview"]["sample_rows"].as_array().unwrap().len(),
            PREVIEW_ROWS
        );
        assert!(empty(webview, "list_inference_records")
            .as_array()
            .unwrap()
            .is_empty());
        assert!(ipc(
            webview,
            "preview_inference",
            serde_json::json!({"jobId": OTHER_JOB_ID, "request": inference_request(&second, &harness.feature_set_id)}),
        )
        .is_err());
        let restored = empty(webview, "get_active_inference_job");
        assert_eq!(restored["state"], "PREPARED");
        let committed = ipc(
            webview,
            "confirm_inference",
            serde_json::json!({"jobId": JOB_ID}),
        )
        .unwrap();
        assert_eq!(committed["disposition"], "MATERIALIZED");
        assert!(empty(webview, "get_active_inference_job").is_null());

        ipc(
            webview,
            "preview_inference",
            serde_json::json!({"jobId": OTHER_JOB_ID, "request": inference_request(&second, &harness.feature_set_id)}),
        )
        .unwrap();
        let other = ipc(
            webview,
            "confirm_inference",
            serde_json::json!({"jobId": OTHER_JOB_ID}),
        )
        .unwrap();
        let report = ipc(
            webview,
            "compare_inference_records",
            serde_json::json!({"request": {"record_ids": [committed["record_id"], other["record_id"]]}}),
        )
        .unwrap();
        assert_eq!(report["rows"], 48);
        let field = &report["pairs"][0]["fields"][0];
        assert!((field["mean_absolute_difference"].as_f64().unwrap() - 0.25).abs() < 1e-6);

        let inspected = ipc(
            webview,
            "inspect_inference_record",
            serde_json::json!({"recordId": committed["record_id"]}),
        )
        .unwrap();
        assert_eq!(inspected["status"], "READY");
        assert_eq!(inspected["integrity"]["lineage_valid"], true);
        assert_eq!(
            inspected["manifest"]["lineage"]["feature_set_id"],
            harness.feature_set_id.as_str()
        );
        assert_eq!(inspected["manifest"]["lineage"]["model_id"], first.as_str());
        assert_eq!(
            empty(webview, "list_model_lab_attempts")[0]["status"],
            "MATERIALIZED"
        );
    }

    #[test]
    fn gate_desktop_model_lab_ipc_reports_runtime_unavailability_truthfully() {
        let harness = harness();
        let webview = &harness.webview;
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(ipc(
            webview,
            "set_ollama_endpoint",
            serde_json::json!({"endpoint": "http://ollama.example.com:11434"})
        )
        .is_err());
        let configured = ipc(
            webview,
            "set_ollama_endpoint",
            serde_json::json!({"endpoint": format!("http://127.0.0.1:{port}")}),
        )
        .unwrap();
        assert_eq!(empty(webview, "get_ollama_endpoint"), configured);
        let discovery = empty(webview, "discover_ollama_models");
        assert_eq!(discovery["availability"], "UNAVAILABLE");
        assert!(discovery["candidates"].as_array().unwrap().is_empty());
        let request = serde_json::json!({
            "candidate_id": format!("ollama:{FIXTURE_MODEL}@{FIXTURE_DIGEST}"),
            "input_fields": ["fast", "slow"],
            "outputs": [{"name": "direction", "kind": {"kind": "CATEGORY", "labels": ["UP", "DOWN"]}}],
            "runtime": {"adapter": "OLLAMA_LOOPBACK", "seed": 1, "max_output_tokens": 64,
                        "context_tokens": 2048, "instruction": "Classify."},
            "authority": "ADVISORY_ONLY"
        });
        let error = ipc(
            webview,
            "preview_model_registration",
            serde_json::json!({"request": request}),
        )
        .unwrap_err();
        assert!(error.as_str().unwrap().contains("unavailable"), "{error}");

        // A registered Ollama model whose runtime goes away is RUNTIME_UNAVAILABLE, not output.
        let mock = MockOllama::start(MockBehavior::Normal);
        ipc(
            webview,
            "set_ollama_endpoint",
            serde_json::json!({"endpoint": mock.endpoint().to_string()}),
        )
        .unwrap();
        let preview = ipc(
            webview,
            "preview_model_registration",
            serde_json::json!({"request": request}),
        )
        .unwrap();
        let registered = ipc(
            webview,
            "register_model",
            serde_json::json!({"request": request, "confirmedModelId": preview["model_id"]}),
        )
        .unwrap();
        drop(mock);
        let models = empty(webview, "list_models");
        assert_eq!(models[0]["model_id"], registered["model_id"]);
        assert_eq!(models[0]["status"], "RUNTIME_UNAVAILABLE");
        let mut inference = inference_request(
            registered["model_id"].as_str().unwrap(),
            &harness.feature_set_id,
        );
        inference["bindings"] = serde_json::json!({
            "fast": {"source": "FEATURE", "feature_key": "fast", "output": "value"},
            "slow": {"source": "FEATURE", "feature_key": "slow", "output": "value"}
        });
        let error = ipc(
            webview,
            "preview_inference",
            serde_json::json!({"jobId": JOB_ID, "request": inference}),
        )
        .unwrap_err();
        assert!(
            error.as_str().unwrap().contains("RUNTIME_UNAVAILABLE"),
            "{error}"
        );
        assert!(empty(webview, "get_active_inference_job").is_null());
        assert!(empty(webview, "list_inference_records")
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn gate_desktop_model_lab_ipc_cancels_a_running_job_and_discards_a_late_preview() {
        let harness = harness();
        let model_id = harness.register_spread("spread.onnx", 0.0);
        let request: InferenceRequest =
            serde_json::from_value(inference_request(&model_id, &harness.feature_set_id)).unwrap();
        let state = harness.app.state::<ModelLabState>();
        let worker = state.begin_preview(JOB_ID).unwrap();
        assert_eq!(
            empty(&harness.webview, "get_active_inference_job")["state"],
            "RUNNING"
        );
        assert_eq!(
            ipc(
                &harness.webview,
                "cancel_inference_job",
                serde_json::json!({"jobId": JOB_ID})
            )
            .unwrap(),
            "CANCELLATION_REQUESTED"
        );
        let result = worker
            .store
            .prepare_inference(&request, &worker.cancel, &worker.progress);
        assert!(matches!(result, Err(ModelError::Cancelled)));
        assert!(state.finish_preview(JOB_ID, result).is_err());
        assert!(empty(&harness.webview, "get_active_inference_job").is_null());
        assert_eq!(
            empty(&harness.webview, "list_model_lab_attempts")[0]["status"],
            "CANCELLED"
        );

        let worker = state.begin_preview(OTHER_JOB_ID).unwrap();
        let result =
            worker
                .store
                .prepare_inference(&request, &AtomicBool::new(false), &worker.progress);
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
        assert!(empty(&harness.webview, "list_inference_records")
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn gate_desktop_model_lab_ipc_failure_after_confirm_leaves_no_stale_preview() {
        let harness = harness();
        let model_id = harness.register_spread("spread.onnx", 0.0);
        ipc(
            &harness.webview,
            "preview_inference",
            serde_json::json!({"jobId": JOB_ID, "request": inference_request(&model_id, &harness.feature_set_id)}),
        )
        .unwrap();
        let output = harness
            .root()
            .join("feature-sets")
            .join(&harness.feature_set_id)
            .join("features.parquet");
        let mut bytes = fs::read(&output).unwrap();
        bytes[100] ^= 0x01;
        fs::write(&output, bytes).unwrap();
        let error = ipc(
            &harness.webview,
            "confirm_inference",
            serde_json::json!({"jobId": JOB_ID}),
        )
        .unwrap_err();
        assert!(error.as_str().unwrap().contains("verification"), "{error}");
        assert!(empty(&harness.webview, "get_active_inference_job").is_null());
        assert!(ipc(
            &harness.webview,
            "confirm_inference",
            serde_json::json!({"jobId": JOB_ID})
        )
        .is_err());
        assert!(empty(&harness.webview, "list_inference_records")
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn gate_desktop_model_lab_ipc_accepts_registry_identities_never_paths_or_order_fields() {
        let harness = harness();
        let webview = &harness.webview;
        let model_id = harness.register_spread("spread.onnx", 0.0);
        let mut by_path = onnx_registration("C:/Users/owner/model.onnx");
        assert!(ipc(
            webview,
            "preview_model_registration",
            serde_json::json!({"request": by_path})
        )
        .is_err());
        by_path["candidate_id"] = serde_json::json!("onnx:../../models");
        assert!(ipc(
            webview,
            "preview_model_registration",
            serde_json::json!({"request": by_path})
        )
        .is_err());
        let mut with_order = onnx_registration("onnx:018f4c4f-7d9b-7b9f-a70e-92bb7cf04c63");
        with_order["order_type"] = serde_json::json!("MARKET");
        assert!(ipc(
            webview,
            "preview_model_registration",
            serde_json::json!({"request": with_order})
        )
        .is_err());
        let mut sizing = inference_request(&model_id, &harness.feature_set_id);
        sizing["position_size"] = serde_json::json!(1.0);
        assert!(ipc(
            webview,
            "preview_inference",
            serde_json::json!({"jobId": JOB_ID, "request": sizing})
        )
        .is_err());
        let mut veto = inference_request(&model_id, &harness.feature_set_id);
        veto["authority"] = serde_json::json!("DECISION_VETO");
        let refused = ipc(
            webview,
            "preview_inference",
            serde_json::json!({"jobId": JOB_ID, "request": veto}),
        )
        .unwrap_err();
        assert!(
            refused.as_str().unwrap().contains("ADVISORY_ONLY"),
            "{refused}"
        );
        let mut path_input = inference_request(&model_id, &harness.feature_set_id);
        path_input["input"] =
            serde_json::json!({"kind": "FEATURE_SET", "feature_set_id": "C:/features.parquet"});
        assert!(ipc(
            webview,
            "preview_inference",
            serde_json::json!({"jobId": OTHER_JOB_ID, "request": path_input})
        )
        .is_err());
        assert!(ipc(
            webview,
            "inspect_model",
            serde_json::json!({"modelId": "../models"})
        )
        .is_err());
        assert!(ipc(
            webview,
            "discard_onnx_candidate",
            serde_json::json!({"candidateId": "onnx:.."})
        )
        .is_err());
        assert!(empty(webview, "list_inference_records")
            .as_array()
            .unwrap()
            .is_empty());
    }
}
