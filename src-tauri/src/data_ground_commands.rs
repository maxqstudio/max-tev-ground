use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use rg_data_ground::{
    inspect_source_with_limit,
    store::{DatasetCatalogEntry, DatasetStore, DatasetStoreError, ImportCommitResult},
    CsvDelimiter, ImportPreview, ImportRequest, PreparedDataset, SourceInspection,
};
use serde::Serialize;
use tauri::{AppHandle, Runtime, State};
use tauri_plugin_dialog::DialogExt;
use uuid::Uuid;

const MAX_CONCURRENT_IMPORTS: usize = 1;

pub struct DataGroundState {
    store: Arc<DatasetStore>,
    selected_sources: Mutex<HashMap<String, PathBuf>>,
    start_lock: Mutex<()>,
    running_imports: Mutex<HashMap<String, RunningImport>>,
    prepared_imports: Mutex<HashMap<String, PreparedImport>>,
    catalog_job: Mutex<Option<CatalogVerificationJob>>,
    #[cfg(test)]
    catalog_worker_gate: Mutex<Option<Arc<std::sync::Barrier>>>,
}

struct RunningImport {
    cancel: Arc<AtomicBool>,
}

struct PreparedImport {
    prepared: PreparedDataset,
}

struct CatalogVerificationJob {
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<DatasetCatalogProgress>>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatasetCatalogProgress {
    pub job_id: String,
    pub status: CatalogVerificationStatus,
    pub completed: usize,
    pub total: usize,
    pub datasets: Vec<DatasetCatalogEntry>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CatalogVerificationStatus {
    Running,
    Completed,
    Cancelled,
    Failed,
}

impl DataGroundState {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, DatasetStoreError> {
        Ok(Self {
            store: Arc::new(DatasetStore::open(root)?),
            selected_sources: Mutex::new(HashMap::new()),
            start_lock: Mutex::new(()),
            running_imports: Mutex::new(HashMap::new()),
            prepared_imports: Mutex::new(HashMap::new()),
            catalog_job: Mutex::new(None),
            #[cfg(test)]
            catalog_worker_gate: Mutex::new(None),
        })
    }

    /// The shared dataset registry, so Feature Studio reads the same verified snapshots.
    pub fn store(&self) -> Arc<DatasetStore> {
        Arc::clone(&self.store)
    }

    fn insert_selected_source(&self, path: PathBuf) -> Result<SelectedSource, String> {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "selected file name is invalid".to_owned())?
            .to_owned();
        let selection_id = Uuid::now_v7().to_string();
        let mut selections = self
            .selected_sources
            .lock()
            .map_err(|_| "source selection state is unavailable".to_owned())?;
        selections.clear();
        selections.insert(selection_id.clone(), path);
        Ok(SelectedSource {
            selection_id,
            file_name,
        })
    }

    fn selected_source(&self, selection_id: &str) -> Result<PathBuf, String> {
        self.selected_sources
            .lock()
            .map_err(|_| "source selection state is unavailable".to_owned())?
            .get(selection_id)
            .cloned()
            .ok_or_else(|| "selected source token is invalid or expired".to_owned())
    }

    /// Stands in for the native file picker in headless IPC tests.
    #[cfg(test)]
    pub(crate) fn select_for_test(&self, path: PathBuf) -> SelectedSource {
        self.insert_selected_source(path).unwrap()
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedSource {
    pub selection_id: String,
    pub file_name: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedSourceInspection {
    pub selection_id: String,
    pub inspection: SourceInspection,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportPreviewSession {
    pub job_id: String,
    pub preview: ImportPreview,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ActiveImportState {
    Running {
        #[serde(rename = "jobId")]
        job_id: String,
    },
    Prepared {
        session: Box<ImportPreviewSession>,
    },
}

impl DataGroundState {
    fn active_import(&self) -> Result<Option<ActiveImportState>, String> {
        let _start = self
            .start_lock
            .lock()
            .map_err(|_| "import state is unavailable".to_owned())?;
        if let Some((job_id, prepared)) = self
            .prepared_imports
            .lock()
            .map_err(|_| "import state is unavailable".to_owned())?
            .iter()
            .next()
        {
            return Ok(Some(ActiveImportState::Prepared {
                session: Box::new(ImportPreviewSession {
                    job_id: job_id.clone(),
                    preview: prepared.prepared.preview.clone(),
                }),
            }));
        }
        let running = self
            .running_imports
            .lock()
            .map_err(|_| "import state is unavailable".to_owned())?
            .keys()
            .next()
            .cloned();
        Ok(running.map(|job_id| ActiveImportState::Running { job_id }))
    }

    fn start_catalog_verification(&self) -> Result<DatasetCatalogProgress, String> {
        let mut current = self
            .catalog_job
            .lock()
            .map_err(|_| "catalog verification state is unavailable".to_owned())?;
        if let Some(job) = current.as_ref() {
            let progress = job
                .progress
                .lock()
                .map_err(|_| "catalog verification progress is unavailable".to_owned())?
                .clone();
            if progress.status == CatalogVerificationStatus::Running {
                return Ok(progress);
            }
        }

        let total = self
            .store
            .dataset_count()
            .map_err(|error| error.to_string())?;
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(DatasetCatalogProgress {
            job_id: Uuid::now_v7().to_string(),
            status: if total == 0 {
                CatalogVerificationStatus::Completed
            } else {
                CatalogVerificationStatus::Running
            },
            completed: 0,
            total,
            datasets: Vec::new(),
            error: None,
        }));
        *current = Some(CatalogVerificationJob {
            cancel: Arc::clone(&cancel),
            progress: Arc::clone(&progress),
        });
        if total == 0 {
            return Ok(progress
                .lock()
                .map_err(|_| "catalog verification progress is unavailable".to_owned())?
                .clone());
        }

        let store = Arc::clone(&self.store);
        #[cfg(test)]
        let test_gate = self
            .catalog_worker_gate
            .lock()
            .map_err(|_| "catalog test gate is unavailable".to_owned())?
            .clone();
        let worker_cancel = Arc::clone(&cancel);
        let worker_progress = Arc::clone(&progress);
        let spawn_result = std::thread::Builder::new()
            .name("rg-dataset-catalog-verifier".into())
            .spawn(move || {
                #[cfg(test)]
                if let Some(gate) = test_gate {
                    gate.wait();
                }
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    store.list_datasets_with_progress(&worker_cancel, |completed, total, entry| {
                        if let Ok(mut progress) = worker_progress.lock() {
                            progress.completed = completed;
                            progress.total = total;
                            progress.datasets.push(entry.clone());
                        }
                    })
                }));
                if let Ok(mut progress) = worker_progress.lock() {
                    match result {
                        Ok(Ok(entries)) => {
                            progress.completed = entries.len();
                            progress.total = entries.len();
                            progress.datasets = entries;
                            progress.status = CatalogVerificationStatus::Completed;
                        }
                        Ok(Err(rg_data_ground::store::DatasetStoreError::Data(
                            rg_data_ground::DataGroundError::Cancelled,
                        ))) => progress.status = CatalogVerificationStatus::Cancelled,
                        Ok(Err(error)) => {
                            progress.status = CatalogVerificationStatus::Failed;
                            progress.error = Some(error.to_string());
                        }
                        Err(_) => {
                            progress.status = CatalogVerificationStatus::Failed;
                            progress.error =
                                Some("catalog verifier terminated unexpectedly".into());
                        }
                    }
                }
            });
        if let Err(error) = spawn_result {
            if let Ok(mut progress) = progress.lock() {
                progress.status = CatalogVerificationStatus::Failed;
                progress.error = Some(format!("catalog verifier could not start: {error}"));
            }
        }
        let snapshot = progress
            .lock()
            .map_err(|_| "catalog verification progress is unavailable".to_owned())?
            .clone();
        Ok(snapshot)
    }

    fn catalog_progress(&self) -> Result<Option<DatasetCatalogProgress>, String> {
        self.catalog_job
            .lock()
            .map_err(|_| "catalog verification state is unavailable".to_owned())?
            .as_ref()
            .map(|job| {
                job.progress
                    .lock()
                    .map(|progress| progress.clone())
                    .map_err(|_| "catalog verification progress is unavailable".to_owned())
            })
            .transpose()
    }

    fn cancel_catalog_verification(&self) -> Result<String, String> {
        let job = self
            .catalog_job
            .lock()
            .map_err(|_| "catalog verification state is unavailable".to_owned())?;
        let Some(job) = job.as_ref() else {
            return Ok("NO_ACTIVE_VERIFICATION".into());
        };
        let progress = job
            .progress
            .lock()
            .map_err(|_| "catalog verification progress is unavailable".to_owned())?;
        if progress.status != CatalogVerificationStatus::Running {
            return Ok("NO_ACTIVE_VERIFICATION".into());
        }
        job.cancel.store(true, Ordering::Relaxed);
        Ok("CANCELLATION_REQUESTED".into())
    }
}

#[tauri::command]
pub async fn select_data_source<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, DataGroundState>,
) -> Result<Option<SelectedSource>, String> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    app.dialog()
        .file()
        .add_filter("Market data", &["csv", "parquet"])
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
    selection
        .map(|path| state.insert_selected_source(path))
        .transpose()
}

#[tauri::command(rename_all = "camelCase")]
pub fn inspect_selected_source(
    state: State<'_, DataGroundState>,
    selection_id: String,
    max_source_bytes: u64,
    delimiter: CsvDelimiter,
) -> Result<SelectedSourceInspection, String> {
    if max_source_bytes == 0 {
        return Err("source-byte quota must be positive".into());
    }
    let path = state.selected_source(&selection_id)?;
    let inspection = inspect_source_with_limit(path, max_source_bytes, delimiter)
        .map_err(|error| error.to_string())?;
    Ok(SelectedSourceInspection {
        selection_id,
        inspection,
    })
}

#[tauri::command(rename_all = "camelCase")]
pub async fn preview_data_import(
    state: State<'_, DataGroundState>,
    job_id: String,
    selection_id: String,
    request: ImportRequest,
) -> Result<ImportPreviewSession, String> {
    validate_job_id(&job_id)?;
    // Keep the opaque picker token available after validation failures so the Owner can correct
    // mappings and retry without re-selecting the same local file.
    let source_path = state.selected_source(&selection_id)?;
    let source_file_name = source_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("selected-source")
        .to_owned();
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let _start = state
            .start_lock
            .lock()
            .map_err(|_| "import state is unavailable".to_owned())?;
        let running_count = state
            .running_imports
            .lock()
            .map_err(|_| "import state is unavailable".to_owned())?
            .len();
        let prepared_count = state
            .prepared_imports
            .lock()
            .map_err(|_| "import state is unavailable".to_owned())?
            .len();
        if running_count + prepared_count >= MAX_CONCURRENT_IMPORTS {
            return Err(
                "finish or cancel the existing import preview before starting another".into(),
            );
        }
        let mut imports = state
            .running_imports
            .lock()
            .map_err(|_| "import state is unavailable".to_owned())?;
        if imports.contains_key(&job_id) {
            return Err("an import job with this identity already exists".into());
        }
        imports.insert(
            job_id.clone(),
            RunningImport {
                cancel: Arc::clone(&cancel),
            },
        );
    }

    let store = Arc::clone(&state.store);
    let worker_cancel = Arc::clone(&cancel);
    let worker = tauri::async_runtime::spawn_blocking(move || {
        store.prepare_import(source_path, request, worker_cancel)
    });
    let result = worker.await;
    let _start = state
        .start_lock
        .lock()
        .map_err(|_| "import state is unavailable".to_owned())?;
    match result {
        Ok(Ok(prepared)) => {
            if cancel.load(Ordering::Relaxed) {
                let cleanup = std::fs::remove_dir_all(&prepared.staging_directory);
                if let Err(error) = cleanup {
                    let _ = state.store.record_failure(
                        &source_file_name,
                        "FAILED",
                        "CANCEL_CLEANUP_FAILURE",
                        &error.to_string(),
                    );
                    if let Ok(mut imports) = state.running_imports.lock() {
                        imports.remove(&job_id);
                    }
                    return Err(format!("cancelled import staging cleanup failed: {error}"));
                }
                let attempt = state.store.record_failure(
                    &source_file_name,
                    "CANCELLED",
                    "OWNER_CANCELLED_IMPORT",
                    "Import was cancelled before preview confirmation",
                );
                if let Ok(mut imports) = state.running_imports.lock() {
                    imports.remove(&job_id);
                }
                attempt.map_err(|error| error.to_string())?;
                return Err("import cancelled".into());
            }
            let preview = prepared.preview.clone();
            state
                .prepared_imports
                .lock()
                .map_err(|_| "import state is unavailable".to_owned())?
                .insert(job_id.clone(), PreparedImport { prepared });
            if let Ok(mut imports) = state.running_imports.lock() {
                imports.remove(&job_id);
            }
            Ok(ImportPreviewSession { job_id, preview })
        }
        Ok(Err(error)) => {
            if let Ok(mut imports) = state.running_imports.lock() {
                imports.remove(&job_id);
            }
            let status = if matches!(
                error,
                DatasetStoreError::Data(rg_data_ground::DataGroundError::Cancelled)
            ) {
                "CANCELLED"
            } else {
                "FAILED"
            };
            let _ = state.store.record_failure(
                &source_file_name,
                status,
                store_error_code(&error),
                &error.to_string(),
            );
            Err(error.to_string())
        }
        Err(error) => {
            if let Ok(mut imports) = state.running_imports.lock() {
                imports.remove(&job_id);
            }
            let message = format!("import worker failed: {error}");
            let _ =
                state
                    .store
                    .record_failure(&source_file_name, "FAILED", "WORKER_FAILURE", &message);
            Err(message)
        }
    }
}

#[tauri::command(rename_all = "camelCase")]
pub async fn confirm_data_import(
    state: State<'_, DataGroundState>,
    job_id: String,
) -> Result<ImportCommitResult, String> {
    validate_job_id(&job_id)?;
    let prepared = state
        .prepared_imports
        .lock()
        .map_err(|_| "import state is unavailable".to_owned())?
        .remove(&job_id)
        .ok_or_else(|| "no validated preview exists for this import job".to_owned())?;
    let store = Arc::clone(&state.store);
    tauri::async_runtime::spawn_blocking(move || store.commit_prepared(prepared.prepared))
        .await
        .map_err(|error| format!("dataset commit worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn get_active_data_import(
    state: State<'_, DataGroundState>,
) -> Result<Option<ActiveImportState>, String> {
    state.active_import()
}

#[tauri::command(rename_all = "camelCase")]
pub fn cancel_data_import(
    state: State<'_, DataGroundState>,
    job_id: String,
) -> Result<String, String> {
    validate_job_id(&job_id)?;
    let _start = state
        .start_lock
        .lock()
        .map_err(|_| "import state is unavailable".to_owned())?;
    if let Some(running) = state
        .running_imports
        .lock()
        .map_err(|_| "import state is unavailable".to_owned())?
        .get(&job_id)
    {
        running.cancel.store(true, Ordering::Relaxed);
        return Ok("CANCELLATION_REQUESTED".into());
    }
    let prepared = state
        .prepared_imports
        .lock()
        .map_err(|_| "import state is unavailable".to_owned())?
        .remove(&job_id)
        .ok_or_else(|| "no active or prepared import exists for this job".to_owned())?;
    let file_name = prepared
        .prepared
        .manifest
        .provenance
        .source_file_name
        .clone();
    let stage = prepared.prepared.staging_directory;
    if stage.exists() {
        std::fs::remove_dir_all(stage).map_err(|error| error.to_string())?;
    }
    state
        .store
        .record_failure(
            &file_name,
            "CANCELLED",
            "OWNER_CANCELLED_PREVIEW",
            "Import preview was cancelled before explicit confirmation",
        )
        .map_err(|error| error.to_string())?;
    Ok("CANCELLED".into())
}

#[tauri::command]
pub fn start_dataset_catalog_verification(
    state: State<'_, DataGroundState>,
) -> Result<DatasetCatalogProgress, String> {
    state.start_catalog_verification()
}

#[tauri::command]
pub fn get_dataset_catalog_progress(
    state: State<'_, DataGroundState>,
) -> Result<Option<DatasetCatalogProgress>, String> {
    state.catalog_progress()
}

#[tauri::command]
pub fn cancel_dataset_catalog_verification(
    state: State<'_, DataGroundState>,
) -> Result<String, String> {
    state.cancel_catalog_verification()
}

#[tauri::command]
pub async fn list_datasets(
    state: State<'_, DataGroundState>,
) -> Result<Vec<DatasetCatalogEntry>, String> {
    let store = Arc::clone(&state.store);
    tauri::async_runtime::spawn_blocking(move || store.list_datasets())
        .await
        .map_err(|error| format!("dataset catalogue worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn inspect_dataset(
    state: State<'_, DataGroundState>,
    dataset_id: String,
) -> Result<Option<DatasetCatalogEntry>, String> {
    let store = Arc::clone(&state.store);
    tauri::async_runtime::spawn_blocking(move || store.inspect_dataset(&dataset_id))
        .await
        .map_err(|error| format!("dataset inspection worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn list_import_attempts(
    state: State<'_, DataGroundState>,
) -> Result<Vec<rg_data_ground::store::ImportAttemptSummary>, String> {
    state
        .store
        .list_attempts()
        .map_err(|error| error.to_string())
}

fn validate_job_id(value: &str) -> Result<(), String> {
    Uuid::parse_str(value)
        .map(|_| ())
        .map_err(|_| "import job identity is invalid".to_owned())
}

fn store_error_code(error: &DatasetStoreError) -> &'static str {
    use rg_data_ground::store::DatasetStoreError;
    use rg_data_ground::DataGroundError;
    match error {
        DatasetStoreError::Data(DataGroundError::Cancelled) => "CANCELLED",
        DatasetStoreError::Data(DataGroundError::ResourceQuota(_)) => "RESOURCE_QUOTA",
        DatasetStoreError::Data(DataGroundError::InvalidRequest(_)) => "INVALID_REQUEST",
        DatasetStoreError::Data(DataGroundError::Schema(_)) => "SOURCE_SCHEMA",
        DatasetStoreError::Data(DataGroundError::InvalidRow { .. }) => "INVALID_ROW",
        DatasetStoreError::Data(DataGroundError::DuplicateBar { .. }) => "DUPLICATE_BAR",
        DatasetStoreError::Data(DataGroundError::OutOfOrderBar { .. }) => "OUT_OF_ORDER_BAR",
        DatasetStoreError::Data(DataGroundError::UnsafeSourcePath) => "UNSAFE_SOURCE_PATH",
        DatasetStoreError::InvalidPrepared(_) => "INVALID_PREPARED_SNAPSHOT",
        DatasetStoreError::ExistingDatasetCorrupt => "EXISTING_DATASET_CORRUPT",
        DatasetStoreError::OrphanDirectoryExists => "ORPHAN_DIRECTORY_EXISTS",
        DatasetStoreError::RecoveryBlocked { .. } => "PROMOTION_RECOVERY_BLOCKED",
        DatasetStoreError::VerificationBusy => "CATALOG_VERIFICATION_BUSY",
        DatasetStoreError::InvalidDatasetId => "INVALID_DATASET_ID",
        DatasetStoreError::Persistence(_) => "PERSISTENCE_FAILURE",
        DatasetStoreError::Io(_) => "FILESYSTEM_FAILURE",
        DatasetStoreError::UnsafeStorePath => "UNSAFE_STORE_PATH",
        DatasetStoreError::Poisoned => "STORE_UNAVAILABLE",
        DatasetStoreError::Data(_) => "IMPORT_FAILURE",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::register_commands;
    use rg_data_ground::{
        CalendarPolicy, CsvDelimiter, FieldMapping, MarketRole, ResourceQuotas, TimestampSemantics,
    };
    use std::fs;
    use tauri::{
        ipc::{CallbackFn, InvokeBody},
        test::{mock_builder, mock_context, noop_assets, MockRuntime, INVOKE_KEY},
        webview::{InvokeRequest, Webview, WebviewWindowBuilder},
        Manager,
    };
    use tempfile::TempDir;

    const FIXTURE: &str =
        include_str!("../../crates/rg-data-ground/fixtures/eurusd-gbpusd-h1-h4.csv");
    const SELECTED_ID: &str = "selected-fixture-source";
    const JOB_ID: &str = "018f4c4f-7d9b-7b9f-a70e-92bb7cf04c61";

    fn request() -> ImportRequest {
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
            source_id: "ipc-fixture".into(),
            broker: "FIXTURE".into(),
            server: "FIXTURE-SERVER".into(),
            delimiter: CsvDelimiter::Comma,
            calendar: CalendarPolicy::WeekendsUtc,
            quotas: ResourceQuotas {
                max_source_bytes: Some(1_000_000),
                max_records: Some(100),
            },
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
    fn ipc_import_catalog_and_inspection_use_source_backed_fixture_data() {
        let directory = TempDir::new().unwrap();
        let app_data = directory.path().join("app-data");
        let source = directory.path().join("bars.csv");
        fs::write(&source, FIXTURE).unwrap();
        let state = DataGroundState::open(app_data).unwrap();
        state.select_for_test(source);
        let app = register_commands(mock_builder())
            .manage(state)
            .build(mock_context(noop_assets()))
            .expect("mock Tauri runtime should build");
        let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .expect("mock webview should build");

        let inspected = ipc(
            &webview,
            "inspect_selected_source",
            serde_json::json!({"selectionId": SELECTED_ID, "maxSourceBytes": 1_000_000, "delimiter": "COMMA"}),
        );
        assert!(inspected.is_err(), "opaque selection IDs cannot be guessed");
        let token = app
            .state::<DataGroundState>()
            .selected_sources
            .lock()
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .clone();
        let inspected = ipc(
            &webview,
            "inspect_selected_source",
            serde_json::json!({"selectionId": token, "maxSourceBytes": 1_000_000, "delimiter": "COMMA"}),
        )
        .unwrap();
        assert_eq!(inspected["inspection"]["headers"][0], "timestamp");

        let preview = ipc(
            &webview,
            "preview_data_import",
            serde_json::json!({"jobId": JOB_ID, "selectionId": token, "request": request()}),
        )
        .unwrap();
        assert_eq!(preview["preview"]["bar_count"], 5);
        assert!(app
            .state::<DataGroundState>()
            .store
            .list_datasets()
            .unwrap()
            .is_empty());

        let result = ipc(
            &webview,
            "confirm_data_import",
            serde_json::json!({"jobId": JOB_ID}),
        )
        .unwrap();
        assert_eq!(result["disposition"], "IMPORTED");
        let catalog = ipc(&webview, "list_datasets", serde_json::json!({})).unwrap();
        assert_eq!(catalog.as_array().unwrap().len(), 1);
        assert_eq!(catalog[0]["integrity"]["canonical_parquet_valid"], true);

        let inspected_dataset = ipc(
            &webview,
            "inspect_dataset",
            serde_json::json!({"datasetId": result["dataset_id"]}),
        )
        .unwrap();
        assert_eq!(inspected_dataset["status"], "BLOCKED");
        let attempts = ipc(&webview, "list_import_attempts", serde_json::json!({})).unwrap();
        assert_eq!(attempts.as_array().unwrap().len(), 1);
        assert_eq!(attempts[0]["status"], "IMPORTED");

        let delimited_source = directory.path().join("semicolon.csv");
        fs::write(
            &delimited_source,
            "timestamp;open;high;low;close;volume\n2024-01-08T00:00:00Z;1.1;1.2;1.0;1.15;4\n",
        )
        .unwrap();
        let delimited_token = app
            .state::<DataGroundState>()
            .select_for_test(delimited_source)
            .selection_id;
        let delimited_inspection = ipc(
            &webview,
            "inspect_selected_source",
            serde_json::json!({
                "selectionId": delimited_token,
                "maxSourceBytes": 1_000_000,
                "delimiter": "SEMICOLON"
            }),
        )
        .unwrap();
        assert_eq!(delimited_inspection["inspection"]["headers"][1], "open");
    }

    #[test]
    fn ipc_rejects_unselected_source_tokens_without_registry_rows() {
        let directory = TempDir::new().unwrap();
        let state = DataGroundState::open(directory.path().join("app-data")).unwrap();
        let app = register_commands(mock_builder())
            .manage(state)
            .build(mock_context(noop_assets()))
            .expect("mock Tauri runtime should build");
        let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        let result = ipc(
            &webview,
            "preview_data_import",
            serde_json::json!({
                "jobId": JOB_ID,
                "selectionId": SELECTED_ID,
                "request": request()
            }),
        );
        assert!(result.is_err());
        assert!(app
            .state::<DataGroundState>()
            .store
            .list_datasets()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn ipc_records_invalid_source_attempt_without_registering_partial_dataset() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("invalid-schema.csv");
        fs::write(&source, "date,open\n2024-01-08,1\n").unwrap();
        let state = DataGroundState::open(directory.path().join("app-data")).unwrap();
        state.select_for_test(source);
        let app = register_commands(mock_builder())
            .manage(state)
            .build(mock_context(noop_assets()))
            .expect("mock Tauri runtime should build");
        let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        let token = app
            .state::<DataGroundState>()
            .selected_sources
            .lock()
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .clone();

        let result = ipc(
            &webview,
            "preview_data_import",
            serde_json::json!({"jobId": JOB_ID, "selectionId": token, "request": request()}),
        );
        assert!(result.is_err());
        let state = app.state::<DataGroundState>();
        assert!(state.store.list_datasets().unwrap().is_empty());
        let attempts = state.store.list_attempts().unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].status, "FAILED");
        assert_eq!(attempts[0].source_file_name, "invalid-schema.csv");
    }

    #[test]
    fn ipc_cancel_of_prepared_preview_removes_stage_without_registry_entry() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("cancel-fixture.csv");
        fs::write(&source, FIXTURE).unwrap();
        let state = DataGroundState::open(directory.path().join("app-data")).unwrap();
        state.select_for_test(source);
        let app = register_commands(mock_builder())
            .manage(state)
            .build(mock_context(noop_assets()))
            .expect("mock Tauri runtime should build");
        let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        let token = app
            .state::<DataGroundState>()
            .selected_sources
            .lock()
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .clone();

        ipc(
            &webview,
            "preview_data_import",
            serde_json::json!({"jobId": JOB_ID, "selectionId": token, "request": request()}),
        )
        .unwrap();
        let resumed = ipc(&webview, "get_active_data_import", serde_json::json!({})).unwrap();
        assert_eq!(resumed["state"], "PREPARED");
        assert_eq!(resumed["session"]["jobId"], JOB_ID);
        assert_eq!(resumed["session"]["preview"]["bar_count"], 5);
        let cancelled = ipc(
            &webview,
            "cancel_data_import",
            serde_json::json!({"jobId": JOB_ID}),
        )
        .unwrap();
        assert_eq!(cancelled, "CANCELLED");
        let state = app.state::<DataGroundState>();
        assert!(state.store.list_datasets().unwrap().is_empty());
        let attempts = state.store.list_attempts().unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].status, "CANCELLED");
        assert_eq!(
            std::fs::read_dir(directory.path().join("app-data/.staging"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn catalog_start_returns_before_background_work_and_exposes_cancelable_progress() {
        let directory = TempDir::new().unwrap();
        let app_data = directory.path().join("app-data");
        let source = directory.path().join("catalog-fixture.csv");
        fs::write(&source, FIXTURE).unwrap();
        let state = DataGroundState::open(&app_data).unwrap();
        let prepared = state
            .store
            .prepare_import(&source, request(), Arc::new(AtomicBool::new(false)))
            .unwrap();
        state.store.commit_prepared(prepared).unwrap();
        let gate = Arc::new(std::sync::Barrier::new(2));
        *state.catalog_worker_gate.lock().unwrap() = Some(Arc::clone(&gate));
        let app = register_commands(mock_builder())
            .manage(state)
            .build(mock_context(noop_assets()))
            .expect("mock Tauri runtime should build");
        let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .expect("mock webview should build");

        let started = ipc(
            &webview,
            "start_dataset_catalog_verification",
            serde_json::json!({}),
        )
        .expect("catalog start should return while hashing remains gated");
        assert_eq!(started["status"], "RUNNING");
        assert_eq!(started["total"], 1);
        assert_eq!(started["completed"], 0);
        let cancellation = ipc(
            &webview,
            "cancel_dataset_catalog_verification",
            serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(cancellation, "CANCELLATION_REQUESTED");

        gate.wait();
        let mut final_progress = None;
        for _ in 0..200 {
            let value = ipc(
                &webview,
                "get_dataset_catalog_progress",
                serde_json::json!({}),
            )
            .unwrap();
            if value["status"] != "RUNNING" {
                final_progress = Some(value);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(final_progress.unwrap()["status"], "CANCELLED");
    }
}
