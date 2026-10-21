//! Model registry and inference-record store: discovery, explicit registration, digest-drift
//! quarantine, staged inference, journaled promotion, crash recovery, replay recording, and
//! re-verification of every catalogue read, including the consumed input and model.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    thread,
    time::Duration,
};

use chrono::Utc;
use rg_feature_studio::{store::FeatureSetCatalogEntry, FeatureSetStatus, SeriesSelector};
use rg_persistence::{
    clear_pending_model_lab_promotion, complete_pending_model_lab_promotion, get_model_lab_record,
    get_model_lab_setting, list_inference_replays, list_model_lab_attempts,
    list_model_lab_records_after, list_pending_model_lab_promotions, open_database,
    save_inference_replay, save_model_lab_attempt, save_pending_model_lab_promotion,
    set_model_lab_setting, InferenceReplayRecord, ModelLabAttemptRecord, ModelLabRecordKind,
    ModelLabRecordMetadata, PendingModelLabPromotion,
};
use rg_strategy_engine::{
    store::{DecisionSetCatalogEntry, StrategyStore},
    DecisionSetManifest, DecisionSetStatus,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    http::{LoopbackEndpoint, RuntimeError, DEFAULT_OLLAMA_ENDPOINT},
    inference::{
        self, copy_hashed, sha256_file, BoundValue, InferenceProgress, OllamaRows, OnnxRows, Plan,
        PreviewRow, ProgressPhase,
    },
    input::InputTable,
    ollama::{self, GenerationSettings, OllamaListing},
    onnx::{NativeOnnxModel, OnnxLimits, OnnxModelInfo, DEFAULT_ONNX_LIMITS},
    spec::{
        hex, is_sha256, parse_authority, validate_prefixed_id, validate_registration, AdapterKind,
        Authority, Decoding, InferenceIdentity, InferenceLineage, InferenceRecordManifest,
        InferenceRecordStatus, InferenceRequest, InferenceSemantics, InputColumn, InputKind,
        InputSetIdentity, InputSetRef, ModelCandidate, ModelIdentity, ModelManifest,
        ModelReference, ModelRegistrationRequest, ModelSource, ModelStatus, NormalizedInputBinding,
        OutputKind, RuntimeConfig, INFERENCE_NOTICE, INFERENCE_OUTPUT_FILE,
        INFERENCE_SCHEMA_VERSION, MAX_REQUEST_TIMEOUT_MS, MIN_REQUEST_TIMEOUT_MS, MODEL_NOTICE,
        MODEL_REGISTRY_ID, MODEL_REGISTRY_VERSION, MODEL_SCHEMA_VERSION, ONNX_CONTENT_FILE,
    },
    ModelError,
};

const CATALOG_PAGE_SIZE: u64 = 32;
const ATTEMPT_HISTORY_LIMIT: u64 = 50;
const MANIFEST_FILE: &str = "manifest.json";
const CANDIDATE_FILE: &str = "candidate.json";
/// Private, digest-verified copy of the consumed input. Removed before promotion.
const INPUT_COPY_FILE: &str = "input.parquet";
const ENDPOINT_SETTING: &str = "ollama_endpoint";
pub const DEFAULT_MAX_ONNX_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelLabLimits {
    pub max_onnx_bytes: u64,
    pub onnx: OnnxLimits,
}

pub const DEFAULT_LIMITS: ModelLabLimits = ModelLabLimits {
    max_onnx_bytes: DEFAULT_MAX_ONNX_BYTES,
    onnx: DEFAULT_ONNX_LIMITS,
};

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RuntimeAvailability {
    Available,
    Unavailable,
    Malformed,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct OllamaDiscovery {
    pub endpoint: String,
    pub availability: RuntimeAvailability,
    pub diagnostic: Option<String>,
    pub candidates: Vec<ModelCandidate>,
}

/// What a staged ONNX candidate recorded when its file was copied into application storage.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct StagedOnnxCandidate {
    candidate_id: String,
    source_file_name: String,
    content_sha256: String,
    content_bytes: u64,
    info: Option<OnnxModelInfo>,
    refusal: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct RegistrationPreview {
    pub model_id: String,
    pub manifest: ModelManifest,
    pub already_registered: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RegistrationDisposition {
    Registered,
    AlreadyRegistered,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RegistrationResult {
    pub model_id: String,
    pub disposition: RegistrationDisposition,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ModelIntegrity {
    pub manifest_valid: bool,
    pub content_valid: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelCatalogEntry {
    pub model_id: String,
    pub adapter: Option<AdapterKind>,
    pub model_name: Option<String>,
    pub status: ModelStatus,
    pub manifest: Option<ModelManifest>,
    pub integrity: ModelIntegrity,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct InputColumnOption {
    pub binding: InputColumn,
    pub label: String,
}

/// A registered feature or decision set as offered to Model Lab. Only `eligible` ones may be used.
#[derive(Clone, Debug, Serialize)]
pub struct InferenceInputCandidate {
    pub kind: InputKind,
    pub id: String,
    pub status: String,
    pub eligible: bool,
    pub reason: Option<String>,
    pub target: Option<SeriesSelector>,
    pub row_count: Option<u64>,
    pub created_at_utc: Option<String>,
    pub columns: Vec<InputColumnOption>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ExistingRecordNotice {
    pub record_id: String,
    pub output_matches: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct InferencePreview {
    pub record_id: String,
    pub manifest_sha256: String,
    pub manifest: InferenceRecordManifest,
    pub sample_rows: Vec<PreviewRow>,
    pub existing: Option<ExistingRecordNotice>,
}

#[derive(Debug)]
pub struct PreparedInference {
    pub manifest: InferenceRecordManifest,
    pub manifest_json: String,
    pub manifest_sha256: String,
    pub preview: InferencePreview,
    pub staging_directory: PathBuf,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum InferenceDisposition {
    Materialized,
    ReplayReproduced,
    ReplayNonDeterministic,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Determinism {
    NotReplayed,
    Reproduced,
    NonDeterministic,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct InferenceCommitResult {
    pub record_id: String,
    pub disposition: InferenceDisposition,
    pub determinism: Determinism,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct InferenceRecordIntegrity {
    pub manifest_valid: bool,
    pub output_valid: bool,
    pub lineage_valid: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct InferenceRecordCatalogEntry {
    pub record_id: String,
    pub model_id: String,
    pub input_id: String,
    pub status: InferenceRecordStatus,
    pub determinism: Determinism,
    pub replays: Vec<InferenceReplayRecord>,
    pub manifest: Option<InferenceRecordManifest>,
    pub integrity: InferenceRecordIntegrity,
    pub diagnostic: Option<String>,
}

/// One live Ollama listing per store operation, so a catalogue read asks the runtime once.
#[derive(Default)]
pub(crate) struct RuntimeCheck {
    listing: Option<Result<Vec<OllamaListing>, RuntimeError>>,
}

/// Upstream inputs and models re-verified during one store operation.
#[derive(Default)]
struct LineageCheck {
    feature_sets: BTreeMap<String, Option<FeatureSetCatalogEntry>>,
    decision_sets: BTreeMap<String, Option<DecisionSetCatalogEntry>>,
    models: BTreeMap<String, Option<ModelCatalogEntry>>,
}

/// A verified, consumable input with its table layout.
struct ResolvedInput {
    identity: InputSetIdentity,
    table: InputTable,
    path: PathBuf,
    feature_set_id: String,
    datasets: Vec<rg_feature_studio::spec::InputLineage>,
    feature_outputs: BTreeMap<String, Vec<String>>,
}

pub struct ModelLabStore {
    strategies: Arc<StrategyStore>,
    candidates_directory: PathBuf,
    staging_directory: PathBuf,
    models_directory: PathBuf,
    records_directory: PathBuf,
    connection: Mutex<rusqlite::Connection>,
    operation_lock: Mutex<()>,
    limits: ModelLabLimits,
}

impl ModelLabStore {
    /// Opens the model registry beside an already opened decision store and finishes any
    /// interrupted promotion. Recovery fails closed and preserves evidence on any mismatch.
    /// Unregistered ONNX candidates from an earlier session are discarded.
    pub fn open(
        root: impl AsRef<Path>,
        strategies: Arc<StrategyStore>,
    ) -> Result<Self, ModelError> {
        Self::open_with_limits(root, strategies, DEFAULT_LIMITS)
    }

    pub fn open_with_limits(
        root: impl AsRef<Path>,
        strategies: Arc<StrategyStore>,
        limits: ModelLabLimits,
    ) -> Result<Self, ModelError> {
        fs::create_dir_all(root.as_ref())?;
        let root = fs::canonicalize(root.as_ref())?;
        let staging_root = ensure_directory(&root, ".model-lab-staging")?;
        let candidates_directory = ensure_directory(&staging_root, "candidates")?;
        let staging_directory = ensure_directory(&staging_root, "runs")?;
        let models_directory = ensure_directory(&root, "models")?;
        let records_directory = ensure_directory(&root, "inference-records")?;
        let database_path = root.join("research.sqlite3");
        match fs::symlink_metadata(&database_path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
            _ => return Err(ModelError::UnsafeStorePath),
        }
        for entry in fs::read_dir(&candidates_directory)? {
            let path = entry?.path();
            if directory_is_regular(&path) {
                fs::remove_dir_all(&path)?;
            }
        }
        let store = Self {
            strategies,
            candidates_directory,
            staging_directory,
            models_directory,
            records_directory,
            connection: Mutex::new(open_database(database_path)?),
            operation_lock: Mutex::new(()),
            limits,
        };
        store.recover_pending_promotions()?;
        Ok(store)
    }

    pub fn strategies(&self) -> &Arc<StrategyStore> {
        &self.strategies
    }

    pub fn limits(&self) -> ModelLabLimits {
        self.limits
    }

    /// The Owner-configured loopback endpoint, or the default when none is configured.
    pub fn ollama_endpoint(&self) -> Result<LoopbackEndpoint, ModelError> {
        let stored = {
            let connection = self.connection_guard()?;
            get_model_lab_setting(&connection, ENDPOINT_SETTING)?
        };
        Ok(LoopbackEndpoint::parse(
            stored.as_deref().unwrap_or(DEFAULT_OLLAMA_ENDPOINT),
        )?)
    }

    pub fn set_ollama_endpoint(&self, text: &str) -> Result<LoopbackEndpoint, ModelError> {
        let endpoint = LoopbackEndpoint::parse(text.trim())?;
        let connection = self.connection_guard()?;
        set_model_lab_setting(
            &connection,
            ENDPOINT_SETTING,
            &endpoint.to_string(),
            &timestamp(),
        )?;
        Ok(endpoint)
    }

    /// Lists the runtime's installed models as unregistered candidates. Nothing is registered,
    /// pulled, or run. An absent or malformed runtime is reported, never treated as empty.
    pub fn discover_ollama(&self) -> Result<OllamaDiscovery, ModelError> {
        let endpoint = self.ollama_endpoint()?;
        let listing = match ollama::list_models(&endpoint) {
            Ok(listing) => listing,
            Err(error) => {
                let availability = match error {
                    RuntimeError::Malformed(_) | RuntimeError::Oversized(_) => {
                        RuntimeAvailability::Malformed
                    }
                    _ => RuntimeAvailability::Unavailable,
                };
                return Ok(OllamaDiscovery {
                    endpoint: endpoint.to_string(),
                    availability,
                    diagnostic: Some(error.to_string()),
                    candidates: Vec::new(),
                });
            }
        };
        let mut counts = BTreeMap::<&str, usize>::new();
        for entry in &listing {
            *counts.entry(entry.name.as_str()).or_default() += 1;
        }
        let registered = self.registered_identities()?;
        let candidates = listing
            .iter()
            .map(|entry| {
                let duplicate = counts[entry.name.as_str()] > 1;
                ModelCandidate {
                    candidate_id: format!("ollama:{}@{}", entry.name, entry.digest),
                    adapter: AdapterKind::OllamaLoopback,
                    model_name: entry.name.clone(),
                    content_digest: entry.digest.clone(),
                    eligible: !duplicate,
                    reason: duplicate.then(|| {
                        "the runtime lists this name more than once, so its content is ambiguous"
                            .into()
                    }),
                    registered_model_ids: registered
                        .get(&(AdapterKind::OllamaLoopback, entry.digest.clone()))
                        .cloned()
                        .unwrap_or_default(),
                    ollama: Some(entry.clone()),
                    onnx: None,
                    content_bytes: entry.size_bytes,
                }
            })
            .collect();
        Ok(OllamaDiscovery {
            endpoint: endpoint.to_string(),
            availability: RuntimeAvailability::Available,
            diagnostic: None,
            candidates,
        })
    }

    /// Copies a user-selected ONNX file into private application storage, hashes the copy, and
    /// inspects it. The result is an unregistered candidate; the original file is never read again.
    pub fn stage_onnx_candidate(
        &self,
        source: &Path,
        cancel: &AtomicBool,
    ) -> Result<ModelCandidate, ModelError> {
        let metadata = fs::symlink_metadata(source)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(ModelError::InvalidRequest(
                "select a regular .onnx file, not a link or directory".into(),
            ));
        }
        let source_file_name = source
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| {
                name.len() <= 255
                    && Path::new(name)
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("onnx"))
            })
            .ok_or_else(|| ModelError::InvalidRequest("select a file ending in .onnx".into()))?
            .to_owned();
        if metadata.len() > self.limits.max_onnx_bytes {
            return Err(ModelError::ResourceQuota(format!(
                "ONNX file is {} bytes; the limit is {}",
                metadata.len(),
                self.limits.max_onnx_bytes
            )));
        }
        let candidate_uuid = Uuid::now_v7().to_string();
        let directory = self.candidates_directory.join(&candidate_uuid);
        fs::create_dir(&directory)?;
        let result = (|| {
            let copy = directory.join(ONNX_CONTENT_FILE);
            let (content_sha256, content_bytes) =
                copy_hashed(source, &copy, self.limits.max_onnx_bytes, cancel)?;
            let bytes = fs::read(&copy)?;
            if hex(Sha256::digest(&bytes)) != content_sha256 {
                return Err(ModelError::CandidateChanged {
                    candidate_id: format!("onnx:{candidate_uuid}"),
                    reason: "staged copy changed while it was inspected".into(),
                });
            }
            let (info, refusal) = match NativeOnnxModel::load(&bytes, &self.limits.onnx) {
                Ok(model) => (Some(model.info().clone()), None),
                Err(error) => (None, Some(error.to_string())),
            };
            let staged = StagedOnnxCandidate {
                candidate_id: format!("onnx:{candidate_uuid}"),
                source_file_name,
                content_sha256,
                content_bytes,
                info,
                refusal,
            };
            fs::write(directory.join(CANDIDATE_FILE), serde_json::to_vec(&staged)?)?;
            Ok(staged)
        })();
        match result {
            Ok(staged) => Ok(self.onnx_candidate(&staged)?),
            Err(error) => {
                let _ = fs::remove_dir_all(&directory);
                Err(error)
            }
        }
    }

    /// ONNX candidates staged in this session.
    pub fn list_onnx_candidates(&self) -> Result<Vec<ModelCandidate>, ModelError> {
        let mut candidates = Vec::new();
        for entry in fs::read_dir(&self.candidates_directory)? {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if let Ok(staged) = self.staged_candidate(&format!("onnx:{name}")) {
                candidates.push(self.onnx_candidate(&staged)?);
            }
        }
        candidates.sort_by(|a, b| a.candidate_id.cmp(&b.candidate_id));
        Ok(candidates)
    }

    pub fn discard_onnx_candidate(&self, candidate_id: &str) -> Result<(), ModelError> {
        let directory = self.candidate_directory(candidate_id)?;
        if !directory_is_regular(&directory) {
            return Err(ModelError::CandidateMissing(candidate_id.into()));
        }
        fs::remove_dir_all(directory)?;
        Ok(())
    }

    fn onnx_candidate(&self, staged: &StagedOnnxCandidate) -> Result<ModelCandidate, ModelError> {
        let registered = self.registered_identities()?;
        Ok(ModelCandidate {
            candidate_id: staged.candidate_id.clone(),
            adapter: AdapterKind::OnnxNative,
            model_name: staged.source_file_name.clone(),
            content_digest: staged.content_sha256.clone(),
            eligible: staged.info.is_some(),
            reason: staged.refusal.clone(),
            registered_model_ids: registered
                .get(&(AdapterKind::OnnxNative, staged.content_sha256.clone()))
                .cloned()
                .unwrap_or_default(),
            ollama: None,
            onnx: staged.info.clone(),
            content_bytes: Some(staged.content_bytes),
        })
    }

    fn candidate_directory(&self, candidate_id: &str) -> Result<PathBuf, ModelError> {
        let uuid = candidate_id
            .strip_prefix("onnx:")
            .filter(|uuid| Uuid::parse_str(uuid).is_ok_and(|parsed| parsed.to_string() == *uuid))
            .ok_or_else(|| ModelError::CandidateMissing(candidate_id.into()))?;
        Ok(self.candidates_directory.join(uuid))
    }

    fn staged_candidate(&self, candidate_id: &str) -> Result<StagedOnnxCandidate, ModelError> {
        let directory = self.candidate_directory(candidate_id)?;
        let bytes = regular_file_bytes(&directory.join(CANDIDATE_FILE))?
            .ok_or_else(|| ModelError::CandidateMissing(candidate_id.into()))?;
        let staged = serde_json::from_slice::<StagedOnnxCandidate>(&bytes)
            .map_err(|_| ModelError::CandidateMissing(candidate_id.into()))?;
        if staged.candidate_id != candidate_id {
            return Err(ModelError::CandidateMissing(candidate_id.into()));
        }
        Ok(staged)
    }

    /// Model IDs already registered for each adapter and content digest.
    fn registered_identities(
        &self,
    ) -> Result<BTreeMap<(AdapterKind, String), Vec<String>>, ModelError> {
        let mut identities = BTreeMap::<(AdapterKind, String), Vec<String>>::new();
        let mut cursor = String::new();
        loop {
            let page = {
                let connection = self.connection_guard()?;
                list_model_lab_records_after(
                    &connection,
                    ModelLabRecordKind::Model,
                    &cursor,
                    CATALOG_PAGE_SIZE,
                )?
            };
            if page.is_empty() {
                return Ok(identities);
            }
            for metadata in page {
                cursor.clone_from(&metadata.record_id);
                let adapter = match metadata.parent_id.as_str() {
                    "OLLAMA_LOOPBACK" => AdapterKind::OllamaLoopback,
                    _ => AdapterKind::OnnxNative,
                };
                identities
                    .entry((adapter, metadata.content_sha256.clone()))
                    .or_default()
                    .push(metadata.record_id.clone());
            }
        }
    }

    /// Builds the manifest a registration request would create, after checking it against the
    /// candidate's current content. Returns the manifest and, for ONNX, the staged model bytes.
    fn build_model_manifest(
        &self,
        request: &ModelRegistrationRequest,
    ) -> Result<(ModelManifest, Option<Vec<u8>>), ModelError> {
        let (input_fields, outputs, runtime, authority) = validate_registration(request)?;
        let adapter = runtime.adapter();
        let changed = |reason: String| ModelError::CandidateChanged {
            candidate_id: request.candidate_id.clone(),
            reason,
        };
        let (model_name, content_digest, onnx, source, content) = match adapter {
            AdapterKind::OllamaLoopback => {
                let (name, digest) = request
                    .candidate_id
                    .strip_prefix("ollama:")
                    .and_then(|rest| rest.rsplit_once('@'))
                    .ok_or_else(|| ModelError::CandidateMissing(request.candidate_id.clone()))?;
                let endpoint = self.ollama_endpoint()?;
                let listing = ollama::list_models(&endpoint)?;
                let matches = listing
                    .iter()
                    .filter(|entry| entry.name == name)
                    .collect::<Vec<_>>();
                let entry = match matches.as_slice() {
                    [] => return Err(changed("the runtime no longer lists this model".into())),
                    [entry] if entry.digest == digest => (*entry).clone(),
                    [entry] => {
                        return Err(changed(format!(
                            "the runtime now reports digest {} instead of {digest}",
                            entry.digest
                        )))
                    }
                    _ => return Err(changed("the runtime lists this name more than once".into())),
                };
                (
                    name.to_owned(),
                    digest.to_owned(),
                    None,
                    ModelSource::OllamaListing {
                        endpoint: endpoint.to_string(),
                        listing: entry,
                    },
                    None,
                )
            }
            AdapterKind::OnnxNative => {
                if !request.candidate_id.starts_with("onnx:") {
                    return Err(ModelError::InvalidRequest(
                        "ONNX runtime parameters require an ONNX candidate".into(),
                    ));
                }
                let staged = self.staged_candidate(&request.candidate_id)?;
                let directory = self.candidate_directory(&request.candidate_id)?;
                let bytes = regular_file_bytes(&directory.join(ONNX_CONTENT_FILE))?
                    .ok_or_else(|| ModelError::CandidateMissing(request.candidate_id.clone()))?;
                if hex(Sha256::digest(&bytes)) != staged.content_sha256
                    || bytes.len() as u64 != staged.content_bytes
                {
                    return Err(changed(
                        "the staged ONNX copy no longer matches its digest".into(),
                    ));
                }
                let model = NativeOnnxModel::load(&bytes, &self.limits.onnx)?;
                let info = model.info().clone();
                if info.input.width != input_fields.len() as u64 {
                    return Err(ModelError::InvalidRequest(format!(
                        "the model input {} has width {}; declare exactly that many input fields",
                        info.input.name, info.input.width
                    )));
                }
                if info.output_width() != outputs.len() as u64 {
                    return Err(ModelError::InvalidRequest(format!(
                        "the model outputs {} values; declare exactly that many output fields",
                        info.output_width()
                    )));
                }
                if outputs
                    .iter()
                    .any(|output| !matches!(output.kind, OutputKind::Float64 { .. }))
                {
                    return Err(ModelError::InvalidRequest(
                        "ONNX outputs are FLOAT tensors; declare them as FLOAT64 fields".into(),
                    ));
                }
                (
                    staged.source_file_name.clone(),
                    staged.content_sha256.clone(),
                    Some(info),
                    ModelSource::OnnxFile {
                        source_file_name: staged.source_file_name,
                    },
                    Some(bytes),
                )
            }
        };
        if request.candidate_id.starts_with("onnx:") != (adapter == AdapterKind::OnnxNative) {
            return Err(ModelError::InvalidRequest(
                "runtime parameters do not match the candidate's adapter".into(),
            ));
        }
        let identity = ModelIdentity {
            schema_version: MODEL_SCHEMA_VERSION,
            registry_id: MODEL_REGISTRY_ID.into(),
            registry_version: MODEL_REGISTRY_VERSION.into(),
            adapter,
            adapter_version: adapter.version().into(),
            model_name,
            content_digest,
            input_fields,
            outputs,
            runtime,
            decoding: Decoding::for_adapter(adapter),
            onnx,
            authority,
        };
        let identity_sha256 = identity.sha256()?;
        let content_bytes = content.as_ref().map(|bytes| bytes.len() as u64);
        let manifest = ModelManifest {
            schema_version: MODEL_SCHEMA_VERSION,
            model_id: format!("md_{identity_sha256}"),
            identity_sha256,
            identity,
            registered_at_utc: timestamp(),
            content_file: content.as_ref().map(|_| ONNX_CONTENT_FILE.into()),
            content_bytes,
            source,
            status: ModelStatus::Ready,
            research_notice: MODEL_NOTICE.into(),
        };
        Ok((manifest, content))
    }

    /// Shows the model identity a registration would create. Nothing is registered.
    pub fn preview_registration(
        &self,
        request: &ModelRegistrationRequest,
    ) -> Result<RegistrationPreview, ModelError> {
        let (manifest, _) = self.build_model_manifest(request)?;
        let already_registered = {
            let connection = self.connection_guard()?;
            get_model_lab_record(&connection, ModelLabRecordKind::Model, &manifest.model_id)?
                .is_some()
        };
        Ok(RegistrationPreview {
            model_id: manifest.model_id.clone(),
            manifest,
            already_registered,
        })
    }

    /// Registers the model the Owner confirmed. The request is rebuilt against the candidate's
    /// current content and must reproduce the confirmed model ID exactly.
    pub fn register_model(
        &self,
        request: &ModelRegistrationRequest,
        confirmed_model_id: &str,
    ) -> Result<RegistrationResult, ModelError> {
        let _operation = self.operation_guard()?;
        let result = self.register_locked(request, confirmed_model_id);
        if let Err(error) = &result {
            let pending = validate_prefixed_id(confirmed_model_id, "md_")
                && self
                    .has_pending_promotion(confirmed_model_id)
                    .unwrap_or(true);
            if !pending {
                let _ = self.save_attempt(
                    "MODEL_REGISTRATION",
                    "FAILED",
                    Some(confirmed_model_id),
                    Some(error.code()),
                    Some(&error.to_string()),
                );
            }
        }
        result
    }

    fn register_locked(
        &self,
        request: &ModelRegistrationRequest,
        confirmed_model_id: &str,
    ) -> Result<RegistrationResult, ModelError> {
        if !validate_prefixed_id(confirmed_model_id, "md_") {
            return Err(ModelError::InvalidRecordId);
        }
        let (manifest, content) = self.build_model_manifest(request)?;
        if manifest.model_id != confirmed_model_id {
            return Err(ModelError::CandidateChanged {
                candidate_id: request.candidate_id.clone(),
                reason: "the model identity differs from the one shown for confirmation".into(),
            });
        }
        let existing = {
            let connection = self.connection_guard()?;
            get_model_lab_record(&connection, ModelLabRecordKind::Model, &manifest.model_id)?
        };
        if let Some(existing) = existing {
            let entry = self.verify_model(&existing, None)?;
            if entry.status == ModelStatus::Quarantined {
                return Err(ModelError::ExistingRecordCorrupt(manifest.model_id));
            }
            return Ok(RegistrationResult {
                model_id: manifest.model_id,
                disposition: RegistrationDisposition::AlreadyRegistered,
            });
        }
        let destination = self.models_directory.join(&manifest.model_id);
        if path_exists(&destination)? {
            return Err(ModelError::OrphanDirectoryExists);
        }
        let staging_name = Uuid::now_v7().to_string();
        let stage = self.staging_directory.join(&staging_name);
        fs::create_dir(&stage)?;
        let staged = (|| -> Result<String, ModelError> {
            if let Some(bytes) = &content {
                write_new(&stage.join(ONNX_CONTENT_FILE), bytes)?;
            }
            let manifest_json = manifest.canonical_json()?;
            write_new(&stage.join(MANIFEST_FILE), manifest_json.as_bytes())?;
            Ok(manifest_json)
        })();
        let manifest_json = match staged {
            Ok(json) => json,
            Err(error) => {
                let _ = fs::remove_dir_all(&stage);
                return Err(error);
            }
        };
        let now = timestamp();
        let pending = PendingModelLabPromotion {
            staging_name,
            record: ModelLabRecordMetadata {
                kind: ModelLabRecordKind::Model,
                record_id: manifest.model_id.clone(),
                identity_sha256: manifest.identity_sha256.clone(),
                parent_id: manifest.identity.adapter.as_str().into(),
                input_id: None,
                manifest_sha256: hex(Sha256::digest(manifest_json.as_bytes())),
                manifest_json,
                content_sha256: manifest.identity.content_digest.clone(),
                created_at_utc: manifest.registered_at_utc.clone(),
                status: "READY".into(),
                manifest_integrity_valid: true,
            },
            attempt: ModelLabAttemptRecord {
                attempt_id: Uuid::now_v7().to_string(),
                started_at_utc: now.clone(),
                completed_at_utc: now,
                operation: "MODEL_REGISTRATION".into(),
                status: "REGISTERED".into(),
                subject_id: Some(manifest.model_id.clone()),
                diagnostic_code: None,
                diagnostic_message: None,
            },
        };
        self.journal_and_promote(&pending, &stage, &destination)?;
        Ok(RegistrationResult {
            model_id: manifest.model_id,
            disposition: RegistrationDisposition::Registered,
        })
    }

    fn journal_and_promote(
        &self,
        pending: &PendingModelLabPromotion,
        stage: &Path,
        destination: &Path,
    ) -> Result<(), ModelError> {
        {
            let connection = self.connection_guard()?;
            if let Err(error) = save_pending_model_lab_promotion(&connection, pending) {
                let _ = fs::remove_dir_all(stage);
                return Err(error.into());
            }
        }
        if let Err(error) = promote_with_bounded_retry(stage, destination) {
            if !path_exists(destination)? {
                let connection = self.connection_guard()?;
                clear_pending_model_lab_promotion(&connection, &pending.record.record_id)?;
                let _ = fs::remove_dir_all(stage);
            }
            return Err(error);
        }
        let mut connection = self.connection_guard()?;
        complete_pending_model_lab_promotion(&mut connection, &pending.record.record_id)?;
        Ok(())
    }

    /// Lists every registered model after re-verifying its manifest and content. For Ollama
    /// models the runtime is asked once for its current listing: a changed or missing digest
    /// quarantines the model, and an unreachable runtime is reported as RUNTIME_UNAVAILABLE.
    pub fn list_models(&self, cancel: &AtomicBool) -> Result<Vec<ModelCatalogEntry>, ModelError> {
        let mut runtime = RuntimeCheck::default();
        let mut entries = Vec::new();
        let mut cursor = String::new();
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(ModelError::Cancelled);
            }
            let page = {
                let connection = self.connection_guard()?;
                list_model_lab_records_after(
                    &connection,
                    ModelLabRecordKind::Model,
                    &cursor,
                    CATALOG_PAGE_SIZE,
                )?
            };
            if page.is_empty() {
                return Ok(entries);
            }
            for metadata in page {
                cursor.clone_from(&metadata.record_id);
                entries.push(self.verify_model(&metadata, Some(&mut runtime))?);
            }
        }
    }

    pub fn inspect_model(&self, model_id: &str) -> Result<Option<ModelCatalogEntry>, ModelError> {
        if !validate_prefixed_id(model_id, "md_") {
            return Err(ModelError::InvalidRecordId);
        }
        let metadata = {
            let connection = self.connection_guard()?;
            get_model_lab_record(&connection, ModelLabRecordKind::Model, model_id)?
        };
        metadata
            .map(|metadata| self.verify_model(&metadata, Some(&mut RuntimeCheck::default())))
            .transpose()
    }

    /// Re-verifies one registration. With `runtime`, an Ollama model is also checked against the
    /// runtime's live listing; without it, only the stored registration is checked.
    fn verify_model(
        &self,
        metadata: &ModelLabRecordMetadata,
        runtime: Option<&mut RuntimeCheck>,
    ) -> Result<ModelCatalogEntry, ModelError> {
        let manifest = serde_json::from_str::<ModelManifest>(&metadata.manifest_json).ok();
        let directory = self.models_directory.join(&metadata.record_id);
        let id_valid = validate_prefixed_id(&metadata.record_id, "md_");
        let identity_valid = manifest.as_ref().is_some_and(|manifest| {
            manifest.identity_is_valid()
                && manifest.model_id == metadata.record_id
                && manifest.identity_sha256 == metadata.identity_sha256
                && manifest.identity.adapter.as_str() == metadata.parent_id
                && manifest.identity.content_digest == metadata.content_sha256
                && manifest
                    .canonical_json()
                    .is_ok_and(|json| json == metadata.manifest_json)
        });
        let manifest_file_valid = id_valid
            && directory_is_regular(&directory)
            && regular_file_bytes(&directory.join(MANIFEST_FILE))?
                .is_some_and(|bytes| bytes == metadata.manifest_json.as_bytes());
        let manifest_valid =
            metadata.manifest_integrity_valid && identity_valid && manifest_file_valid;
        let model_name = manifest
            .as_ref()
            .map(|manifest| manifest.identity.model_name.clone());
        let adapter = manifest.as_ref().map(|manifest| manifest.identity.adapter);
        let (content_valid, mut diagnostic) = match manifest.as_ref() {
            Some(manifest) if manifest.identity.adapter == AdapterKind::OnnxNative => {
                let path = directory.join(ONNX_CONTENT_FILE);
                let valid = id_valid
                    && is_regular_file(&path)?
                    && fs::metadata(&path)?.len() == manifest.content_bytes.unwrap_or(u64::MAX)
                    && sha256_file(&path, &AtomicBool::new(false))? == metadata.content_sha256;
                let diagnostic = (!valid).then(|| {
                    format!(
                        "model {} ({}) file is missing or no longer matches its registered SHA-256",
                        metadata.record_id, manifest.identity.model_name
                    )
                });
                (valid, diagnostic)
            }
            _ => (true, None),
        };
        if !manifest_valid {
            diagnostic = Some(format!(
                "model {} manifest does not match the immutable registry",
                metadata.record_id
            ));
        }
        let mut status = if manifest_valid && content_valid {
            ModelStatus::Ready
        } else {
            ModelStatus::Quarantined
        };
        if let (ModelStatus::Ready, Some(manifest), Some(runtime)) =
            (&status, manifest.as_ref(), runtime)
        {
            if manifest.identity.adapter == AdapterKind::OllamaLoopback {
                let endpoint = self.ollama_endpoint()?;
                let listing = runtime
                    .listing
                    .get_or_insert_with(|| ollama::list_models(&endpoint));
                let name = &manifest.identity.model_name;
                let registered = &manifest.identity.content_digest;
                match listing {
                    Err(error) => {
                        status = ModelStatus::RuntimeUnavailable;
                        diagnostic = Some(format!(
                            "model {} ({name}) cannot be checked: {error}",
                            metadata.record_id
                        ));
                    }
                    Ok(listing) => {
                        let listed = listing
                            .iter()
                            .filter(|entry| entry.name == *name)
                            .collect::<Vec<_>>();
                        let problem = match listed.as_slice() {
                            [] => Some("is no longer installed in the runtime".to_owned()),
                            [entry] if entry.digest == *registered => None,
                            [entry] => Some(format!(
                                "now has digest {} instead of registered {registered}; the registration is quarantined and never re-identified",
                                entry.digest
                            )),
                            _ => Some("is listed more than once by the runtime".to_owned()),
                        };
                        if let Some(problem) = problem {
                            status = ModelStatus::Quarantined;
                            diagnostic =
                                Some(format!("model {} ({name}) {problem}", metadata.record_id));
                        }
                    }
                }
            }
        }
        Ok(ModelCatalogEntry {
            model_id: metadata.record_id.clone(),
            adapter,
            model_name,
            status,
            manifest,
            integrity: ModelIntegrity {
                manifest_valid,
                content_valid,
            },
            diagnostic,
        })
    }

    /// Model boundary for inference: the registration must verify and, for Ollama, the runtime
    /// must currently list the registered digest. Returns the manifest and ONNX bytes.
    fn usable_model(&self, model_id: &str) -> Result<(ModelManifest, Option<Vec<u8>>), ModelError> {
        if !validate_prefixed_id(model_id, "md_") {
            return Err(ModelError::InvalidRecordId);
        }
        let metadata = {
            let connection = self.connection_guard()?;
            get_model_lab_record(&connection, ModelLabRecordKind::Model, model_id)?
        }
        .ok_or_else(|| ModelError::ModelMissing(model_id.into()))?;
        let entry = self.verify_model(&metadata, Some(&mut RuntimeCheck::default()))?;
        let manifest = match (&entry.status, entry.manifest) {
            (ModelStatus::Ready, Some(manifest)) => manifest,
            (status, _) => {
                return Err(ModelError::ModelNotUsable {
                    model_id: model_id.into(),
                    status: status.as_str().into(),
                    reason: entry.diagnostic,
                })
            }
        };
        let content = match manifest.identity.adapter {
            AdapterKind::OllamaLoopback => None,
            AdapterKind::OnnxNative => {
                let path = self.models_directory.join(model_id).join(ONNX_CONTENT_FILE);
                let bytes = regular_file_bytes(&path)?.unwrap_or_default();
                if hex(Sha256::digest(&bytes)) != manifest.identity.content_digest {
                    return Err(ModelError::ModelNotUsable {
                        model_id: model_id.into(),
                        status: "QUARANTINED".into(),
                        reason: Some("model file changed after verification".into()),
                    });
                }
                Some(bytes)
            }
        };
        Ok((manifest, content))
    }

    /// Registered feature and decision sets with their re-verified status and bindable columns.
    pub fn list_inference_inputs(
        &self,
        cancel: &AtomicBool,
    ) -> Result<Vec<InferenceInputCandidate>, ModelError> {
        let mut candidates = Vec::new();
        for entry in self.strategies.features().list_feature_sets(cancel)? {
            let manifest = entry.manifest.as_ref();
            let reason = match entry.status {
                FeatureSetStatus::Ready if manifest.is_some() => None,
                _ => Some(entry.diagnostic.clone().unwrap_or_else(|| {
                    format!("feature set is {}, not READY", entry.status.as_str())
                })),
            };
            candidates.push(InferenceInputCandidate {
                kind: InputKind::FeatureSet,
                id: entry.feature_set_id.clone(),
                status: entry.status.as_str().into(),
                eligible: reason.is_none(),
                reason,
                target: manifest.map(|m| m.identity.target.clone()),
                row_count: manifest.map(|m| m.row_count),
                created_at_utc: manifest.map(|m| m.created_at_utc.clone()),
                columns: manifest
                    .map(|m| {
                        m.identity
                            .features
                            .iter()
                            .flat_map(|feature| {
                                feature.outputs.iter().map(|output| InputColumnOption {
                                    binding: InputColumn::Feature {
                                        feature_key: feature.key.clone(),
                                        output: output.clone(),
                                    },
                                    label: if feature.outputs.len() > 1 {
                                        format!(
                                            "{} · {output} ({})",
                                            feature.key, feature.indicator_id
                                        )
                                    } else {
                                        format!("{} ({})", feature.key, feature.indicator_id)
                                    },
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            });
        }
        for entry in self.strategies.list_decision_sets(cancel)? {
            let manifest = entry.manifest.as_ref();
            let reason = match entry.status {
                DecisionSetStatus::Ready if manifest.is_some() => None,
                _ => Some(entry.diagnostic.clone().unwrap_or_else(|| {
                    format!("decision set is {}, not READY", entry.status.as_str())
                })),
            };
            let mut columns = vec![InputColumnOption {
                binding: InputColumn::Decision {},
                label: "decision (BUY = 1, SELL = -1, SKIP = 0)".into(),
            }];
            if let Some(manifest) = manifest {
                for binding in &manifest.identity.strategy.bindings {
                    columns.push(InputColumnOption {
                        binding: InputColumn::DecisionInput {
                            role: binding.role.clone(),
                        },
                        label: format!(
                            "input {} ({} · {})",
                            binding.role, binding.feature_key, binding.output
                        ),
                    });
                }
            }
            candidates.push(InferenceInputCandidate {
                kind: InputKind::DecisionSet,
                id: entry.decision_set_id.clone(),
                status: entry.status.as_str().into(),
                eligible: reason.is_none(),
                reason,
                target: manifest.map(|m| m.identity.feature_set.target.clone()),
                row_count: manifest.map(|m| m.row_count),
                created_at_utc: manifest.map(|m| m.created_at_utc.clone()),
                columns,
            });
        }
        Ok(candidates)
    }

    /// Resolves a consumable input through its own store's downstream boundary.
    fn resolve_input(&self, input: &InputSetRef) -> Result<ResolvedInput, ModelError> {
        match input {
            InputSetRef::FeatureSet { feature_set_id } => {
                if !validate_prefixed_id(feature_set_id, "fs_") {
                    return Err(ModelError::InvalidRequest(
                        "feature set ID is not a registered fs_<sha256> identity".into(),
                    ));
                }
                let (entry, path) = self
                    .strategies
                    .features()
                    .consumable_feature_set(feature_set_id)?;
                let manifest =
                    entry
                        .manifest
                        .as_ref()
                        .ok_or_else(|| ModelError::InputIntegrity {
                            input_id: feature_set_id.clone(),
                            reason: "verified manifest is unavailable".into(),
                        })?;
                let table = InputTable::for_feature_set(manifest).map_err(|reason| {
                    ModelError::InputIntegrity {
                        input_id: feature_set_id.clone(),
                        reason,
                    }
                })?;
                Ok(ResolvedInput {
                    identity: InputSetIdentity {
                        kind: InputKind::FeatureSet,
                        id: feature_set_id.clone(),
                        identity_sha256: manifest.identity_sha256.clone(),
                        manifest_sha256: entry.manifest_sha256.clone(),
                        output_sha256: manifest.output_sha256.clone(),
                        output_fingerprint: manifest.output_fingerprint.clone(),
                        row_count: manifest.row_count,
                        target: manifest.identity.target.clone(),
                    },
                    table,
                    path,
                    feature_set_id: feature_set_id.clone(),
                    datasets: manifest.lineage.clone(),
                    feature_outputs: manifest
                        .identity
                        .features
                        .iter()
                        .map(|feature| (feature.key.clone(), feature.outputs.clone()))
                        .collect(),
                })
            }
            InputSetRef::DecisionSet { decision_set_id } => {
                if !validate_prefixed_id(decision_set_id, "dc_") {
                    return Err(ModelError::InvalidRequest(
                        "decision set ID is not a registered dc_<sha256> identity".into(),
                    ));
                }
                let path = self.strategies.output_path(decision_set_id)?;
                let entry = self
                    .strategies
                    .inspect_decision_set(decision_set_id)?
                    .filter(|entry| entry.status == DecisionSetStatus::Ready)
                    .ok_or_else(|| ModelError::InputNotUsable {
                        input_id: decision_set_id.clone(),
                        status: "NOT_READY".into(),
                    })?;
                let manifest =
                    entry
                        .manifest
                        .as_ref()
                        .ok_or_else(|| ModelError::InputIntegrity {
                            input_id: decision_set_id.clone(),
                            reason: "verified manifest is unavailable".into(),
                        })?;
                let table = InputTable::for_decision_set(manifest).map_err(|reason| {
                    ModelError::InputIntegrity {
                        input_id: decision_set_id.clone(),
                        reason,
                    }
                })?;
                Ok(ResolvedInput {
                    identity: decision_input_identity(&entry, manifest),
                    table,
                    path,
                    feature_set_id: manifest.identity.feature_set.feature_set_id.clone(),
                    datasets: manifest.lineage.datasets.clone(),
                    feature_outputs: BTreeMap::new(),
                })
            }
        }
    }

    /// Validates the request, re-verifies the model and input, and runs inference into a private
    /// staging directory. Nothing is registered until [`ModelLabStore::commit_inference`].
    pub fn prepare_inference(
        &self,
        request: &InferenceRequest,
        cancel: &AtomicBool,
        progress: &InferenceProgress,
    ) -> Result<PreparedInference, ModelError> {
        if parse_authority(&request.authority)? != Authority::AdvisoryOnly {
            return Err(ModelError::AuthorityRefused(request.authority.clone()));
        }
        let quotas = &request.quotas;
        if quotas.max_rows == 0 || quotas.max_output_bytes == 0 {
            return Err(ModelError::InvalidRequest(
                "positive row and output-byte quotas are required".into(),
            ));
        }
        if !(MIN_REQUEST_TIMEOUT_MS..=MAX_REQUEST_TIMEOUT_MS).contains(&quotas.request_timeout_ms) {
            return Err(ModelError::InvalidRequest(format!(
                "request timeout must be between {MIN_REQUEST_TIMEOUT_MS} and {MAX_REQUEST_TIMEOUT_MS} ms"
            )));
        }
        progress.set_phase(ProgressPhase::VerifyingModel, 0);
        let (model, content) = self.usable_model(&request.model_id)?;
        progress.set_phase(ProgressPhase::VerifyingInput, 0);
        let input = self.resolve_input(&request.input)?;
        let plan = plan_bindings(request, &model, &input)?;
        if input.identity.row_count > quotas.max_rows {
            return Err(ModelError::ResourceQuota(format!(
                "input has {} rows; the inference-row quota is {}",
                input.identity.row_count, quotas.max_rows
            )));
        }
        let stage = self.staging_directory.join(Uuid::now_v7().to_string());
        fs::create_dir(&stage)?;
        let result = self.prepare_in_stage(
            request, &model, content, &input, &plan, &stage, cancel, progress,
        );
        if result.is_err() {
            let _ = fs::remove_dir_all(&stage);
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_in_stage(
        &self,
        request: &InferenceRequest,
        model: &ModelManifest,
        content: Option<Vec<u8>>,
        input: &ResolvedInput,
        plan: &Plan,
        stage: &Path,
        cancel: &AtomicBool,
        progress: &InferenceProgress,
    ) -> Result<PreparedInference, ModelError> {
        let input_copy = stage.join(INPUT_COPY_FILE);
        let (copied_sha256, _) = copy_hashed(&input.path, &input_copy, u64::MAX, cancel)?;
        if copied_sha256 != input.identity.output_sha256 {
            return Err(ModelError::InputIntegrity {
                input_id: input.identity.id.clone(),
                reason: "input bytes changed after verification".into(),
            });
        }
        let identity = &model.identity;
        let output_path = stage.join(INFERENCE_OUTPUT_FILE);
        let target = &input.identity.target;
        let materialized = match (&identity.runtime, content) {
            (RuntimeConfig::OnnxNative {}, Some(bytes)) => {
                let native = NativeOnnxModel::load(&bytes, &self.limits.onnx)?;
                let mut rows = OnnxRows {
                    model: &native,
                    outputs: &identity.outputs,
                };
                inference::materialize(
                    plan,
                    &identity.outputs,
                    &mut rows,
                    &input_copy,
                    &output_path,
                    &target.symbol,
                    &target.timeframe,
                    request.quotas.max_rows,
                    request.quotas.max_output_bytes,
                    cancel,
                    progress,
                    &input.identity.id,
                )?
            }
            (
                RuntimeConfig::OllamaLoopback {
                    seed,
                    max_output_tokens,
                    context_tokens,
                    instruction,
                },
                None,
            ) => {
                let mut rows = OllamaRows {
                    endpoint: self.ollama_endpoint()?,
                    model_name: &identity.model_name,
                    settings: GenerationSettings {
                        seed: *seed,
                        max_output_tokens: *max_output_tokens,
                        context_tokens: *context_tokens,
                        instruction: instruction.clone(),
                    },
                    fields: &identity.input_fields,
                    outputs: &identity.outputs,
                    timeout: Duration::from_millis(request.quotas.request_timeout_ms),
                };
                inference::materialize(
                    plan,
                    &identity.outputs,
                    &mut rows,
                    &input_copy,
                    &output_path,
                    &target.symbol,
                    &target.timeframe,
                    request.quotas.max_rows,
                    request.quotas.max_output_bytes,
                    cancel,
                    progress,
                    &input.identity.id,
                )?
            }
            _ => {
                return Err(ModelError::InvalidPrepared(
                    "model runtime and stored content disagree".into(),
                ))
            }
        };
        fs::remove_file(&input_copy)?;
        // The input is re-verified after the read as well as before it.
        let after = self.resolve_input(&request.input)?;
        if after.identity != input.identity {
            return Err(ModelError::InputIntegrity {
                input_id: input.identity.id.clone(),
                reason: "input changed or lost READY verification during inference".into(),
            });
        }
        let inference_identity = InferenceIdentity {
            schema_version: INFERENCE_SCHEMA_VERSION,
            registry_id: MODEL_REGISTRY_ID.into(),
            registry_version: MODEL_REGISTRY_VERSION.into(),
            semantics: InferenceSemantics::current(),
            model: ModelReference {
                model_id: model.model_id.clone(),
                identity_sha256: model.identity_sha256.clone(),
                adapter: identity.adapter,
                model_name: identity.model_name.clone(),
                content_digest: identity.content_digest.clone(),
            },
            input: input.identity.clone(),
            bindings: plan_normalized(plan, request),
            authority: Authority::AdvisoryOnly,
        };
        let identity_sha256 = inference_identity.sha256()?;
        let manifest = InferenceRecordManifest {
            schema_version: INFERENCE_SCHEMA_VERSION,
            record_id: format!("ir_{identity_sha256}"),
            identity_sha256,
            identity: inference_identity,
            created_at_utc: timestamp(),
            output_file: INFERENCE_OUTPUT_FILE.into(),
            output_sha256: materialized.output_sha256,
            output_fingerprint: materialized.output_fingerprint,
            row_count: materialized.row_count,
            columns: materialized.columns,
            outputs: identity.outputs.clone(),
            summary: materialized.summary,
            run: materialized.run,
            lineage: InferenceLineage {
                model_id: model.model_id.clone(),
                model_name: identity.model_name.clone(),
                adapter: identity.adapter,
                content_digest: identity.content_digest.clone(),
                input_kind: input.identity.kind,
                input_id: input.identity.id.clone(),
                feature_set_id: input.feature_set_id.clone(),
                target: target.clone(),
                datasets: input.datasets.clone(),
            },
            authority: Authority::AdvisoryOnly,
            status: InferenceRecordStatus::Ready,
            research_notice: INFERENCE_NOTICE.into(),
        };
        let manifest_json = manifest.canonical_json()?;
        let manifest_sha256 = hex(Sha256::digest(manifest_json.as_bytes()));
        write_new(&stage.join(MANIFEST_FILE), manifest_json.as_bytes())?;
        let existing = {
            let connection = self.connection_guard()?;
            get_model_lab_record(
                &connection,
                ModelLabRecordKind::Inference,
                &manifest.record_id,
            )?
        }
        .map(|existing| ExistingRecordNotice {
            record_id: existing.record_id.clone(),
            output_matches: existing.content_sha256 == manifest.output_sha256,
        });
        Ok(PreparedInference {
            preview: InferencePreview {
                record_id: manifest.record_id.clone(),
                manifest_sha256: manifest_sha256.clone(),
                manifest: manifest.clone(),
                sample_rows: materialized.preview_rows,
                existing,
            },
            manifest,
            manifest_json,
            manifest_sha256,
            staging_directory: stage.to_path_buf(),
        })
    }

    /// Commits a confirmed preview. A new identity is promoted atomically through the journal.
    /// An existing identity is never overwritten: the replay is recorded as REPRODUCED when its
    /// output digest matches and NON_DETERMINISTIC when it does not.
    pub fn commit_inference(
        &self,
        prepared: PreparedInference,
    ) -> Result<InferenceCommitResult, ModelError> {
        let _operation = self.operation_guard()?;
        let record_id = prepared.manifest.record_id.clone();
        let result = self.commit_locked(&prepared);
        let pending = self.has_pending_promotion(&record_id).unwrap_or(true);
        if let Err(error) = &result {
            if !pending {
                let status = if matches!(error, ModelError::Cancelled) {
                    "CANCELLED"
                } else {
                    "FAILED"
                };
                let _ = self.save_attempt(
                    "INFERENCE",
                    status,
                    Some(&record_id),
                    Some(error.code()),
                    Some(&error.to_string()),
                );
            }
        }
        if !pending && prepared.staging_directory.exists() {
            let _ = fs::remove_dir_all(&prepared.staging_directory);
        }
        result
    }

    fn commit_locked(
        &self,
        prepared: &PreparedInference,
    ) -> Result<InferenceCommitResult, ModelError> {
        let stage = verify_prepared(prepared, &self.staging_directory)?;
        let manifest = &prepared.manifest;
        let source = &manifest.identity.input;
        let input_ref = match source.kind {
            InputKind::FeatureSet => InputSetRef::FeatureSet {
                feature_set_id: source.id.clone(),
            },
            InputKind::DecisionSet => InputSetRef::DecisionSet {
                decision_set_id: source.id.clone(),
            },
        };
        if !self
            .resolve_input(&input_ref)
            .is_ok_and(|input| input.identity == *source)
        {
            return Err(ModelError::InputIntegrity {
                input_id: source.id.clone(),
                reason: "input changed or lost READY verification after preview".into(),
            });
        }
        let model_ok = {
            let metadata = {
                let connection = self.connection_guard()?;
                get_model_lab_record(
                    &connection,
                    ModelLabRecordKind::Model,
                    &manifest.identity.model.model_id,
                )?
            };
            match metadata {
                Some(metadata) => {
                    let entry = self.verify_model(&metadata, None)?;
                    entry.status == ModelStatus::Ready
                        && metadata.identity_sha256 == manifest.identity.model.identity_sha256
                }
                None => false,
            }
        };
        if !model_ok {
            return Err(ModelError::ModelNotUsable {
                model_id: manifest.identity.model.model_id.clone(),
                status: "QUARANTINED".into(),
                reason: Some(
                    "model registration changed or failed verification after preview".into(),
                ),
            });
        }
        let existing = {
            let connection = self.connection_guard()?;
            get_model_lab_record(
                &connection,
                ModelLabRecordKind::Inference,
                &manifest.record_id,
            )?
        };
        if let Some(existing) = existing {
            let entry = self.verify_record(
                &existing,
                &mut LineageCheck::default(),
                &AtomicBool::new(false),
            )?;
            if entry.status == InferenceRecordStatus::Quarantined {
                return Err(ModelError::ExistingRecordCorrupt(
                    manifest.record_id.clone(),
                ));
            }
            let reproduced = existing.content_sha256 == manifest.output_sha256;
            let now = timestamp();
            {
                let connection = self.connection_guard()?;
                save_inference_replay(
                    &connection,
                    &InferenceReplayRecord {
                        replay_id: Uuid::now_v7().to_string(),
                        record_id: manifest.record_id.clone(),
                        replayed_at_utc: now,
                        output_sha256: manifest.output_sha256.clone(),
                        outcome: if reproduced {
                            "REPRODUCED".into()
                        } else {
                            "NON_DETERMINISTIC".into()
                        },
                    },
                )?;
            }
            let (status, code, message) = if reproduced {
                (
                    "REUSED",
                    "REPLAY_REPRODUCED",
                    "Replay reproduced the registered output digest; the record was reused",
                )
            } else {
                (
                    "NON_DETERMINISTIC",
                    "REPLAY_NON_DETERMINISTIC",
                    "Replay produced a different output digest; the registered record was kept unchanged",
                )
            };
            self.save_attempt(
                "INFERENCE",
                status,
                Some(&manifest.record_id),
                Some(code),
                Some(message),
            )?;
            let _ = fs::remove_dir_all(&stage);
            let determinism = self.determinism(&manifest.record_id)?.0;
            return Ok(InferenceCommitResult {
                record_id: manifest.record_id.clone(),
                disposition: if reproduced {
                    InferenceDisposition::ReplayReproduced
                } else {
                    InferenceDisposition::ReplayNonDeterministic
                },
                determinism,
            });
        }
        let destination = self.records_directory.join(&manifest.record_id);
        if path_exists(&destination)? {
            return Err(ModelError::OrphanDirectoryExists);
        }
        let staging_name = stage
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| ModelError::InvalidPrepared("staging name is invalid".into()))?
            .to_owned();
        let now = timestamp();
        let pending = PendingModelLabPromotion {
            staging_name,
            record: record_metadata(prepared),
            attempt: ModelLabAttemptRecord {
                attempt_id: Uuid::now_v7().to_string(),
                started_at_utc: now.clone(),
                completed_at_utc: now,
                operation: "INFERENCE".into(),
                status: "MATERIALIZED".into(),
                subject_id: Some(manifest.record_id.clone()),
                diagnostic_code: None,
                diagnostic_message: None,
            },
        };
        {
            let connection = self.connection_guard()?;
            save_pending_model_lab_promotion(&connection, &pending)?;
        }
        if let Err(error) = promote_with_bounded_retry(&stage, &destination) {
            if !path_exists(&destination)? {
                let connection = self.connection_guard()?;
                clear_pending_model_lab_promotion(&connection, &manifest.record_id)?;
            }
            return Err(error);
        }
        let mut connection = self.connection_guard()?;
        complete_pending_model_lab_promotion(&mut connection, &manifest.record_id)?;
        Ok(InferenceCommitResult {
            record_id: manifest.record_id.clone(),
            disposition: InferenceDisposition::Materialized,
            determinism: Determinism::NotReplayed,
        })
    }

    /// Removes a preview that the Owner did not confirm and records the cancellation.
    pub fn discard_inference(&self, prepared: PreparedInference) -> Result<(), ModelError> {
        let _operation = self.operation_guard()?;
        verify_prepared(&prepared, &self.staging_directory)?;
        fs::remove_dir_all(&prepared.staging_directory)?;
        self.save_attempt(
            "INFERENCE",
            "CANCELLED",
            Some(&prepared.manifest.record_id),
            Some("OWNER_CANCELLED_PREVIEW"),
            Some("Inference preview was cancelled before explicit confirmation"),
        )
    }

    pub fn record_failure(
        &self,
        operation: &str,
        status: &str,
        diagnostic_code: &str,
        diagnostic_message: &str,
    ) -> Result<(), ModelError> {
        if !matches!(status, "FAILED" | "CANCELLED")
            || !matches!(operation, "MODEL_REGISTRATION" | "INFERENCE")
        {
            return Err(ModelError::InvalidRequest(
                "only failed or cancelled attempts can be recorded as failures".into(),
            ));
        }
        let _operation = self.operation_guard()?;
        self.save_attempt(
            operation,
            status,
            None,
            Some(diagnostic_code),
            Some(diagnostic_message),
        )
    }

    pub fn list_attempts(&self) -> Result<Vec<ModelLabAttemptRecord>, ModelError> {
        let connection = self.connection_guard()?;
        Ok(list_model_lab_attempts(&connection, ATTEMPT_HISTORY_LIMIT)?)
    }

    /// Lists every inference record after re-verifying its manifest, output digest, the input it
    /// consumed, and its model registration. Each upstream identity is checked once.
    pub fn list_inference_records(
        &self,
        cancel: &AtomicBool,
    ) -> Result<Vec<InferenceRecordCatalogEntry>, ModelError> {
        let mut check = LineageCheck::default();
        let mut entries = Vec::new();
        let mut cursor = String::new();
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(ModelError::Cancelled);
            }
            let page = {
                let connection = self.connection_guard()?;
                list_model_lab_records_after(
                    &connection,
                    ModelLabRecordKind::Inference,
                    &cursor,
                    CATALOG_PAGE_SIZE,
                )?
            };
            if page.is_empty() {
                return Ok(entries);
            }
            for metadata in page {
                cursor.clone_from(&metadata.record_id);
                entries.push(self.verify_record(&metadata, &mut check, cancel)?);
            }
        }
    }

    pub fn inspect_inference_record(
        &self,
        record_id: &str,
    ) -> Result<Option<InferenceRecordCatalogEntry>, ModelError> {
        if !validate_prefixed_id(record_id, "ir_") {
            return Err(ModelError::InvalidRecordId);
        }
        let metadata = {
            let connection = self.connection_guard()?;
            get_model_lab_record(&connection, ModelLabRecordKind::Inference, record_id)?
        };
        metadata
            .map(|metadata| {
                self.verify_record(
                    &metadata,
                    &mut LineageCheck::default(),
                    &AtomicBool::new(false),
                )
            })
            .transpose()
    }

    /// Downstream-use boundary: a READY record's manifest and output path. Callers must still
    /// compare the file digest with the manifest around every read.
    pub fn usable_record(
        &self,
        record_id: &str,
    ) -> Result<(InferenceRecordManifest, PathBuf), ModelError> {
        match self.inspect_inference_record(record_id)? {
            Some(entry) if entry.status == InferenceRecordStatus::Ready => {
                let manifest = entry.manifest.ok_or(ModelError::RecordNotUsable {
                    record_id: record_id.into(),
                    status: "QUARANTINED".into(),
                })?;
                Ok((
                    manifest,
                    self.records_directory
                        .join(record_id)
                        .join(INFERENCE_OUTPUT_FILE),
                ))
            }
            Some(entry) => Err(ModelError::RecordNotUsable {
                record_id: record_id.into(),
                status: entry.status.as_str().into(),
            }),
            None => Err(ModelError::RecordNotUsable {
                record_id: record_id.into(),
                status: "NOT_REGISTERED".into(),
            }),
        }
    }

    fn determinism(
        &self,
        record_id: &str,
    ) -> Result<(Determinism, Vec<InferenceReplayRecord>), ModelError> {
        let replays = {
            let connection = self.connection_guard()?;
            list_inference_replays(&connection, record_id)?
        };
        let determinism = if replays.is_empty() {
            Determinism::NotReplayed
        } else if replays.iter().all(|replay| replay.outcome == "REPRODUCED") {
            Determinism::Reproduced
        } else {
            Determinism::NonDeterministic
        };
        Ok((determinism, replays))
    }

    fn verify_record(
        &self,
        metadata: &ModelLabRecordMetadata,
        check: &mut LineageCheck,
        cancel: &AtomicBool,
    ) -> Result<InferenceRecordCatalogEntry, ModelError> {
        let manifest =
            serde_json::from_str::<InferenceRecordManifest>(&metadata.manifest_json).ok();
        let directory = self.records_directory.join(&metadata.record_id);
        let id_valid = validate_prefixed_id(&metadata.record_id, "ir_");
        let identity_valid = manifest.as_ref().is_some_and(|manifest| {
            manifest.identity_is_valid()
                && manifest.record_id == metadata.record_id
                && manifest.identity_sha256 == metadata.identity_sha256
                && manifest.output_sha256 == metadata.content_sha256
                && manifest.identity.model.model_id == metadata.parent_id
                && Some(manifest.identity.input.id.as_str()) == metadata.input_id.as_deref()
                && manifest
                    .canonical_json()
                    .is_ok_and(|json| json == metadata.manifest_json)
        });
        let manifest_file_valid = id_valid
            && directory_is_regular(&directory)
            && regular_file_bytes(&directory.join(MANIFEST_FILE))?
                .is_some_and(|bytes| bytes == metadata.manifest_json.as_bytes());
        let output_valid = id_valid
            && directory_is_regular(&directory)
            && is_regular_file(&directory.join(INFERENCE_OUTPUT_FILE))?
            && sha256_file(&directory.join(INFERENCE_OUTPUT_FILE), cancel)?
                == metadata.content_sha256;
        let lineage_problem = match &manifest {
            Some(manifest) => self.record_lineage_problem(manifest, check)?,
            None => Some("inference manifest is unreadable, so lineage cannot be checked".into()),
        };
        let integrity = InferenceRecordIntegrity {
            manifest_valid: metadata.manifest_integrity_valid
                && identity_valid
                && manifest_file_valid,
            output_valid,
            lineage_valid: lineage_problem.is_none(),
        };
        let valid = integrity.manifest_valid && integrity.output_valid && integrity.lineage_valid;
        let diagnostic = if !integrity.manifest_valid || !integrity.output_valid {
            Some(format!(
                "inference record {} manifest or output digest does not match the immutable registry",
                metadata.record_id
            ))
        } else {
            lineage_problem
        };
        let (determinism, replays) = self.determinism(&metadata.record_id)?;
        Ok(InferenceRecordCatalogEntry {
            record_id: metadata.record_id.clone(),
            model_id: metadata.parent_id.clone(),
            input_id: metadata.input_id.clone().unwrap_or_default(),
            status: if valid && manifest.is_some() {
                InferenceRecordStatus::Ready
            } else {
                InferenceRecordStatus::Quarantined
            },
            determinism,
            replays,
            manifest,
            integrity,
            diagnostic,
        })
    }

    /// Why a record's upstream is untrustworthy, or `None` when its input is READY and unchanged
    /// and its model registration still verifies. The live Ollama runtime is not consulted: a
    /// record keeps the digest it was computed with.
    fn record_lineage_problem(
        &self,
        manifest: &InferenceRecordManifest,
        check: &mut LineageCheck,
    ) -> Result<Option<String>, ModelError> {
        let source = &manifest.identity.input;
        let input_problem = match source.kind {
            InputKind::FeatureSet => {
                if !check.feature_sets.contains_key(&source.id) {
                    let entry = self.strategies.features().inspect_feature_set(&source.id)?;
                    check.feature_sets.insert(source.id.clone(), entry);
                }
                match &check.feature_sets[&source.id] {
                    None => Some(format!("feature set {} is no longer registered", source.id)),
                    Some(entry) if entry.status != FeatureSetStatus::Ready => Some(format!(
                        "feature set {} is {}, not READY{}",
                        source.id,
                        entry.status.as_str(),
                        entry
                            .diagnostic
                            .as_deref()
                            .map(|reason| format!(": {reason}"))
                            .unwrap_or_default()
                    )),
                    Some(entry) => entry
                        .manifest
                        .as_ref()
                        .filter(|m| {
                            entry.manifest_sha256 == source.manifest_sha256
                                && m.identity_sha256 == source.identity_sha256
                                && m.output_sha256 == source.output_sha256
                                && m.output_fingerprint == source.output_fingerprint
                                && m.row_count == source.row_count
                        })
                        .is_none()
                        .then(|| {
                            format!(
                                "feature set {} no longer matches the recorded manifest or output digest",
                                source.id
                            )
                        }),
                }
            }
            InputKind::DecisionSet => {
                if !check.decision_sets.contains_key(&source.id) {
                    let entry = self.strategies.inspect_decision_set(&source.id)?;
                    check.decision_sets.insert(source.id.clone(), entry);
                }
                match &check.decision_sets[&source.id] {
                    None => Some(format!("decision set {} is no longer registered", source.id)),
                    Some(entry) if entry.status != DecisionSetStatus::Ready => Some(format!(
                        "decision set {} is {}, not READY{}",
                        source.id,
                        entry.status.as_str(),
                        entry
                            .diagnostic
                            .as_deref()
                            .map(|reason| format!(": {reason}"))
                            .unwrap_or_default()
                    )),
                    Some(entry) => entry
                        .manifest
                        .as_ref()
                        .filter(|m| decision_input_identity(entry, m) == *source)
                        .is_none()
                        .then(|| {
                            format!(
                                "decision set {} no longer matches the recorded manifest or output digest",
                                source.id
                            )
                        }),
                }
            }
        };
        if input_problem.is_some() {
            return Ok(input_problem);
        }
        let model_id = &manifest.identity.model.model_id;
        if !check.models.contains_key(model_id) {
            let metadata = {
                let connection = self.connection_guard()?;
                get_model_lab_record(&connection, ModelLabRecordKind::Model, model_id)?
            };
            let entry = metadata
                .map(|metadata| self.verify_model(&metadata, None))
                .transpose()?;
            check.models.insert(model_id.clone(), entry);
        }
        Ok(match &check.models[model_id] {
            None => Some(format!("model {model_id} is no longer registered")),
            Some(entry) if entry.status != ModelStatus::Ready => Some(
                entry
                    .diagnostic
                    .clone()
                    .unwrap_or_else(|| format!("model {model_id} failed verification")),
            ),
            Some(entry)
                if entry.manifest.as_ref().is_none_or(|m| {
                    m.identity_sha256 != manifest.identity.model.identity_sha256
                }) =>
            {
                Some(format!(
                    "model {model_id} no longer matches the recorded identity"
                ))
            }
            Some(_) => None,
        })
    }

    fn recover_pending_promotions(&self) -> Result<(), ModelError> {
        let pending = {
            let connection = self.connection_guard()?;
            list_pending_model_lab_promotions(&connection)?
        };
        for item in pending {
            let id = item.record.record_id.clone();
            let blocked = |reason: &str| ModelError::RecoveryBlocked {
                record_id: id.clone(),
                reason: reason.into(),
            };
            if Uuid::parse_str(&item.staging_name)
                .ok()
                .is_none_or(|uuid| uuid.to_string() != item.staging_name)
            {
                return Err(blocked("journal contains an invalid staging identity"));
            }
            let stage = self.staging_directory.join(&item.staging_name);
            let destination = match item.record.kind {
                ModelLabRecordKind::Model => self.models_directory.join(&id),
                ModelLabRecordKind::Inference => self.records_directory.join(&id),
            };
            match (path_exists(&stage)?, path_exists(&destination)?) {
                (true, true) => {
                    return Err(blocked(
                        "both staging and promoted directories exist; evidence was preserved",
                    ))
                }
                (false, true) => {
                    if !snapshot_matches(&destination, &item.record)? {
                        return Err(blocked(
                            "promoted record failed journal integrity checks; evidence was preserved",
                        ));
                    }
                }
                (true, false) => {
                    if !snapshot_matches(&stage, &item.record)? {
                        return Err(blocked(
                            "staged record failed journal integrity checks; evidence was preserved",
                        ));
                    }
                    promote_with_bounded_retry(&stage, &destination)?;
                    if !snapshot_matches(&destination, &item.record)? {
                        return Err(blocked(
                            "promoted record failed post-move integrity checks; evidence was preserved",
                        ));
                    }
                }
                (false, false) => return Err(blocked(
                    "neither journaled staging nor promoted record exists; journal was preserved",
                )),
            }
            let mut connection = self.connection_guard()?;
            complete_pending_model_lab_promotion(&mut connection, &id)?;
        }
        Ok(())
    }

    fn has_pending_promotion(&self, record_id: &str) -> Result<bool, ModelError> {
        let connection = self.connection_guard()?;
        Ok(list_pending_model_lab_promotions(&connection)?
            .iter()
            .any(|item| item.record.record_id == record_id))
    }

    fn save_attempt(
        &self,
        operation: &str,
        status: &str,
        subject_id: Option<&str>,
        diagnostic_code: Option<&str>,
        diagnostic_message: Option<&str>,
    ) -> Result<(), ModelError> {
        let now = timestamp();
        let connection = self.connection_guard()?;
        save_model_lab_attempt(
            &connection,
            &ModelLabAttemptRecord {
                attempt_id: Uuid::now_v7().to_string(),
                started_at_utc: now.clone(),
                completed_at_utc: now,
                operation: operation.into(),
                status: status.into(),
                subject_id: subject_id.map(str::to_owned),
                diagnostic_code: diagnostic_code.map(str::to_owned),
                diagnostic_message: diagnostic_message.map(str::to_owned),
            },
        )?;
        Ok(())
    }

    fn connection_guard(&self) -> Result<MutexGuard<'_, rusqlite::Connection>, ModelError> {
        self.connection.lock().map_err(|_| ModelError::Poisoned)
    }

    fn operation_guard(&self) -> Result<MutexGuard<'_, ()>, ModelError> {
        self.operation_lock.lock().map_err(|_| ModelError::Poisoned)
    }

    pub(crate) fn staging_root(&self) -> &Path {
        &self.staging_directory
    }

    #[doc(hidden)]
    pub fn staging_directory_for_tests(&self) -> &Path {
        &self.staging_directory
    }

    #[doc(hidden)]
    pub fn models_directory_for_tests(&self) -> &Path {
        &self.models_directory
    }

    #[doc(hidden)]
    pub fn records_directory_for_tests(&self) -> &Path {
        &self.records_directory
    }
}

fn decision_input_identity(
    entry: &DecisionSetCatalogEntry,
    manifest: &DecisionSetManifest,
) -> InputSetIdentity {
    InputSetIdentity {
        kind: InputKind::DecisionSet,
        id: entry.decision_set_id.clone(),
        identity_sha256: manifest.identity_sha256.clone(),
        manifest_sha256: entry.manifest_sha256.clone(),
        output_sha256: manifest.output_sha256.clone(),
        output_fingerprint: manifest.output_fingerprint.clone(),
        row_count: manifest.row_count,
        target: manifest.identity.feature_set.target.clone(),
    }
}

/// Resolves each declared model input field to the input columns that carry it. Every field must
/// be bound exactly once, to a column of the kind its input set provides.
fn plan_bindings(
    request: &InferenceRequest,
    model: &ModelManifest,
    input: &ResolvedInput,
) -> Result<Plan, ModelError> {
    let fields = &model.identity.input_fields;
    let declared = fields.iter().collect::<BTreeSet<_>>();
    if let Some(unknown) = request.bindings.keys().find(|key| !declared.contains(key)) {
        return Err(ModelError::InvalidRequest(format!(
            "{unknown} is not an input field of model {}",
            model.model_id
        )));
    }
    let invalid = |field: &str, reason: String| {
        ModelError::InvalidRequest(format!("input field {field} {reason}"))
    };
    let mut bound = Vec::with_capacity(fields.len());
    for field in fields {
        let binding = request
            .bindings
            .get(field)
            .ok_or_else(|| invalid(field, "is required but not bound".into()))?;
        let column = |name: &str, kind: crate::input::ColumnType| {
            input
                .table
                .position(name)
                .filter(|position| input.table.columns[*position].1 == kind)
                .ok_or_else(|| {
                    invalid(
                        field,
                        format!("names column {name}, which the input does not provide"),
                    )
                })
        };
        let value = match (input.identity.kind, binding) {
            (
                InputKind::FeatureSet,
                InputColumn::Feature {
                    feature_key,
                    output,
                },
            ) => {
                let outputs = input.feature_outputs.get(feature_key).ok_or_else(|| {
                    invalid(
                        field,
                        format!(
                            "names feature {feature_key:?}, which the feature set does not contain"
                        ),
                    )
                })?;
                if !outputs.contains(output) {
                    return Err(invalid(
                        field,
                        format!(
                            "names output {output:?}; feature {feature_key} outputs {}",
                            outputs.join(", ")
                        ),
                    ));
                }
                BoundValue::Feature {
                    value: column(
                        &format!("{feature_key}__{output}"),
                        crate::input::ColumnType::Float,
                    )?,
                    status: column(
                        &format!("{feature_key}__status"),
                        crate::input::ColumnType::Text,
                    )?,
                }
            }
            (InputKind::DecisionSet, InputColumn::DecisionInput { role }) => {
                BoundValue::DecisionInput {
                    value: column(&format!("input__{role}"), crate::input::ColumnType::Float)?,
                }
            }
            (InputKind::DecisionSet, InputColumn::Decision {}) => BoundValue::Decision {
                column: column("decision", crate::input::ColumnType::Text)?,
            },
            (kind, _) => {
                return Err(invalid(
                    field,
                    format!("uses a binding that a {kind:?} input does not provide"),
                ))
            }
        };
        bound.push(value);
    }
    Ok(Plan {
        fields: fields.clone(),
        bound,
        table: input.table.clone(),
    })
}

fn plan_normalized(plan: &Plan, request: &InferenceRequest) -> Vec<NormalizedInputBinding> {
    plan.fields
        .iter()
        .zip(&plan.bound)
        .map(|(field, bound)| {
            let name = |index: usize| plan.table.columns[index].0.clone();
            let (source, column, status_column) = match bound {
                BoundValue::Feature { value, status } => {
                    ("FEATURE", name(*value), Some(name(*status)))
                }
                BoundValue::DecisionInput { value } => ("DECISION_INPUT", name(*value), None),
                BoundValue::Decision { column } => ("DECISION", name(*column), None),
            };
            debug_assert!(request.bindings.contains_key(field));
            NormalizedInputBinding {
                field: field.clone(),
                source: source.into(),
                column,
                status_column,
            }
        })
        .collect()
}

fn record_metadata(prepared: &PreparedInference) -> ModelLabRecordMetadata {
    let manifest = &prepared.manifest;
    ModelLabRecordMetadata {
        kind: ModelLabRecordKind::Inference,
        record_id: manifest.record_id.clone(),
        identity_sha256: manifest.identity_sha256.clone(),
        parent_id: manifest.identity.model.model_id.clone(),
        input_id: Some(manifest.identity.input.id.clone()),
        manifest_json: prepared.manifest_json.clone(),
        manifest_sha256: prepared.manifest_sha256.clone(),
        content_sha256: manifest.output_sha256.clone(),
        created_at_utc: manifest.created_at_utc.clone(),
        status: "READY".into(),
        manifest_integrity_valid: true,
    }
}

/// Checks that a prepared record is internally consistent and that its staged files still match.
/// Returns the canonical staging path.
fn verify_prepared(
    prepared: &PreparedInference,
    staging_root: &Path,
) -> Result<PathBuf, ModelError> {
    let manifest = &prepared.manifest;
    if !manifest.identity_is_valid()
        || prepared.manifest_json != manifest.canonical_json()?
        || hex(Sha256::digest(prepared.manifest_json.as_bytes())) != prepared.manifest_sha256
        || prepared.preview.record_id != manifest.record_id
        || prepared.preview.manifest_sha256 != prepared.manifest_sha256
        || prepared.preview.manifest != *manifest
    {
        return Err(ModelError::InvalidPrepared(
            "manifest, preview, or immutable identity is inconsistent".into(),
        ));
    }
    let canonical_root = fs::canonicalize(staging_root)?;
    let stage = fs::canonicalize(&prepared.staging_directory)?;
    let stage_name_valid = stage
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| {
            Uuid::parse_str(name)
                .ok()
                .map(|uuid| uuid.to_string() == name)
        })
        .unwrap_or(false);
    if stage.parent() != Some(canonical_root.as_path())
        || !directory_is_regular(&stage)
        || !stage_name_valid
        || path_exists(&stage.join(INPUT_COPY_FILE))?
    {
        return Err(ModelError::InvalidPrepared(
            "staging path is outside the private model lab staging directory".into(),
        ));
    }
    let manifest_matches = regular_file_bytes(&stage.join(MANIFEST_FILE))?
        .is_some_and(|bytes| bytes == prepared.manifest_json.as_bytes());
    let output_matches = is_regular_file(&stage.join(INFERENCE_OUTPUT_FILE))?
        && sha256_file(&stage.join(INFERENCE_OUTPUT_FILE), &AtomicBool::new(false))?
            == manifest.output_sha256;
    if !manifest_matches || !output_matches {
        return Err(ModelError::InvalidPrepared(
            "staged manifest or inference output digest does not match".into(),
        ));
    }
    Ok(stage)
}

/// Whether a journaled directory holds exactly the record the journal describes.
fn snapshot_matches(directory: &Path, record: &ModelLabRecordMetadata) -> Result<bool, ModelError> {
    if !directory_is_regular(directory)
        || !record.manifest_integrity_valid
        || !is_sha256(&record.content_sha256)
    {
        return Ok(false);
    }
    let Some(bytes) = regular_file_bytes(&directory.join(MANIFEST_FILE))? else {
        return Ok(false);
    };
    if bytes != record.manifest_json.as_bytes() {
        return Ok(false);
    }
    let never = AtomicBool::new(false);
    match record.kind {
        ModelLabRecordKind::Model => {
            let Ok(manifest) = serde_json::from_slice::<ModelManifest>(&bytes) else {
                return Ok(false);
            };
            if !manifest.identity_is_valid()
                || manifest.model_id != record.record_id
                || manifest.identity.content_digest != record.content_sha256
            {
                return Ok(false);
            }
            match manifest.identity.adapter {
                AdapterKind::OllamaLoopback => Ok(true),
                AdapterKind::OnnxNative => {
                    let path = directory.join(ONNX_CONTENT_FILE);
                    Ok(is_regular_file(&path)?
                        && sha256_file(&path, &never)? == record.content_sha256)
                }
            }
        }
        ModelLabRecordKind::Inference => {
            let Ok(manifest) = serde_json::from_slice::<InferenceRecordManifest>(&bytes) else {
                return Ok(false);
            };
            if !manifest.identity_is_valid()
                || manifest.record_id != record.record_id
                || manifest.output_sha256 != record.content_sha256
            {
                return Ok(false);
            }
            let path = directory.join(INFERENCE_OUTPUT_FILE);
            Ok(is_regular_file(&path)? && sha256_file(&path, &never)? == record.content_sha256)
        }
    }
}

fn ensure_directory(root: &Path, name: &str) -> Result<PathBuf, ModelError> {
    let path = root.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(ModelError::UnsafeStorePath),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&path)?,
        Err(error) => return Err(error.into()),
    }
    let canonical = fs::canonicalize(&path)?;
    if canonical.parent() != Some(root) {
        return Err(ModelError::UnsafeStorePath);
    }
    Ok(canonical)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), ModelError> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn promote_with_bounded_retry(source: &Path, destination: &Path) -> Result<(), ModelError> {
    let mut last_error = None;
    for attempt in 0..3u64 {
        if path_exists(destination)? {
            return Err(ModelError::OrphanDirectoryExists);
        }
        match fs::rename(source, destination) {
            Ok(()) => return Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
                ) && attempt < 2 =>
            {
                last_error = Some(error);
                thread::sleep(Duration::from_millis(25 * (attempt + 1)));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err(last_error
        .unwrap_or_else(|| std::io::Error::other("bounded promotion retry exhausted"))
        .into())
}

fn path_exists(path: &Path) -> Result<bool, ModelError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn directory_is_regular(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
}

fn is_regular_file(path: &Path) -> Result<bool, ModelError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn regular_file_bytes(path: &Path) -> Result<Option<Vec<u8>>, ModelError> {
    if !is_regular_file(path)? {
        return Ok(None);
    }
    Ok(Some(fs::read(path)?))
}

pub(crate) fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}
