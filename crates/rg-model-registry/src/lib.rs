//! RG-R04 Ollama and ONNX Model Registry: local research models registered by immutable identity,
//! and ADVISORY_ONLY inference records over verified RG-R02 feature sets and RG-R03 decision sets.
//!
//! Runtimes are reached only through an Owner-configured loopback Ollama endpoint or a native
//! interpreter over a user-selected ONNX file copied into application storage. Nothing here pulls
//! or downloads a model, calls a remote endpoint, changes a strategy decision, sizes a position,
//! places an order, or computes PnL, win rate, or any claim of edge.

pub mod compare;
pub mod http;
mod inference;
mod input;
pub mod ollama;
pub mod onnx;
pub mod spec;
pub mod store;

#[cfg(any(test, feature = "test-fixtures"))]
pub mod fixtures;

pub use compare::{ComparisonReport, ComparisonRequest};
pub use http::{LoopbackEndpoint, RuntimeError, DEFAULT_OLLAMA_ENDPOINT};
pub use inference::{InferenceProgress, PreviewRow, ProgressPhase, ProgressSnapshot, PREVIEW_ROWS};
pub use onnx::OnnxError;
pub use spec::{
    AdapterKind, Authority, InferenceQuotas, InferenceRecordManifest, InferenceRecordStatus,
    InferenceRequest, InputColumn, InputSetRef, ModelCandidate, ModelManifest,
    ModelRegistrationRequest, ModelStatus, OutputField, OutputKind, RuntimeConfig,
};

use rg_feature_studio::FeatureError;
use rg_persistence::PersistenceError;
use rg_strategy_engine::StrategyError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("invalid model lab request: {0}")]
    InvalidRequest(String),
    #[error("authority {0} is not executable in RG-R04; only ADVISORY_ONLY outputs are")]
    AuthorityRefused(String),
    #[error("model candidate {0} is not available; discover or select it again")]
    CandidateMissing(String),
    #[error("model candidate {candidate_id} changed since it was reviewed: {reason}")]
    CandidateChanged {
        candidate_id: String,
        reason: String,
    },
    #[error("model {0} is not registered")]
    ModelMissing(String),
    #[error("model {model_id} cannot be used: status {status}{}", .reason.as_deref().map(|reason| format!(" ({reason})")).unwrap_or_default())]
    ModelNotUsable {
        model_id: String,
        status: String,
        reason: Option<String>,
    },
    #[error(transparent)]
    Onnx(#[from] OnnxError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error("input {0} is not registered")]
    InputMissing(String),
    #[error("input {input_id} cannot be used: status {status}")]
    InputNotUsable { input_id: String, status: String },
    #[error("input {input_id} failed verification: {reason}")]
    InputIntegrity { input_id: String, reason: String },
    #[error("resource quota exceeded: {0}")]
    ResourceQuota(String),
    #[error("model lab operation cancelled")]
    Cancelled,
    #[error("prepared record failed verification: {0}")]
    InvalidPrepared(String),
    #[error("{0} already exists but its stored content is not trustworthy")]
    ExistingRecordCorrupt(String),
    #[error("an unregistered directory already occupies this identity")]
    OrphanDirectoryExists,
    #[error("model lab promotion recovery is blocked for {record_id}: {reason}")]
    RecoveryBlocked { record_id: String, reason: String },
    #[error("model lab store path is not a private regular directory or file")]
    UnsafeStorePath,
    #[error("model lab store is unavailable because its lock was poisoned")]
    Poisoned,
    #[error("model or inference record ID is invalid")]
    InvalidRecordId,
    #[error("inference record {record_id} cannot be used: status {status}")]
    RecordNotUsable { record_id: String, status: String },
    #[error("comparison refused: {0}")]
    ComparisonRefused(String),
    #[error(transparent)]
    Feature(FeatureError),
    #[error(transparent)]
    Strategy(StrategyError),
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    #[error("Arrow/Parquet error: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),
    #[error("Arrow error: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

impl From<FeatureError> for ModelError {
    fn from(error: FeatureError) -> Self {
        match error {
            FeatureError::FeatureSetNotUsable {
                feature_set_id,
                status,
            } if status == "NOT_REGISTERED" => Self::InputMissing(feature_set_id),
            FeatureError::FeatureSetNotUsable {
                feature_set_id,
                status,
            } => Self::InputNotUsable {
                input_id: feature_set_id,
                status,
            },
            FeatureError::Cancelled => Self::Cancelled,
            other => Self::Feature(other),
        }
    }
}

impl From<StrategyError> for ModelError {
    fn from(error: StrategyError) -> Self {
        match error {
            StrategyError::DecisionSetNotUsable {
                decision_set_id,
                status,
            } if status == "NOT_REGISTERED" => Self::InputMissing(decision_set_id),
            StrategyError::DecisionSetNotUsable {
                decision_set_id,
                status,
            } => Self::InputNotUsable {
                input_id: decision_set_id,
                status,
            },
            StrategyError::Cancelled => Self::Cancelled,
            other => Self::Strategy(other),
        }
    }
}

impl ModelError {
    /// Stable diagnostic code recorded with failed attempts and shown in the UI.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "INVALID_REQUEST",
            Self::AuthorityRefused(_) => "AUTHORITY_REFUSED",
            Self::CandidateMissing(_) => "CANDIDATE_MISSING",
            Self::CandidateChanged { .. } => "CANDIDATE_CHANGED",
            Self::ModelMissing(_) => "MODEL_MISSING",
            Self::ModelNotUsable { .. } => "MODEL_NOT_USABLE",
            Self::Onnx(OnnxError::Limit(_)) => "ONNX_LIMIT",
            Self::Onnx(OnnxError::Unsupported(_)) => "ONNX_UNSUPPORTED",
            Self::Onnx(_) => "ONNX_INVALID",
            Self::Runtime(error) => error.code(),
            Self::InputMissing(_) => "INPUT_MISSING",
            Self::InputNotUsable { .. } => "INPUT_NOT_USABLE",
            Self::InputIntegrity { .. } => "INPUT_INTEGRITY",
            Self::ResourceQuota(_) => "RESOURCE_QUOTA",
            Self::Cancelled => "CANCELLED",
            Self::InvalidPrepared(_) => "INVALID_PREPARED_RECORD",
            Self::ExistingRecordCorrupt(_) => "EXISTING_RECORD_CORRUPT",
            Self::OrphanDirectoryExists => "ORPHAN_DIRECTORY_EXISTS",
            Self::RecoveryBlocked { .. } => "PROMOTION_RECOVERY_BLOCKED",
            Self::UnsafeStorePath => "UNSAFE_STORE_PATH",
            Self::Poisoned => "STORE_UNAVAILABLE",
            Self::InvalidRecordId => "INVALID_RECORD_ID",
            Self::RecordNotUsable { .. } => "RECORD_NOT_USABLE",
            Self::ComparisonRefused(_) => "COMPARISON_REFUSED",
            Self::Feature(_) => "FEATURE_STORE_FAILURE",
            Self::Strategy(_) => "DECISION_STORE_FAILURE",
            Self::Persistence(_) => "PERSISTENCE_FAILURE",
            Self::Parquet(_) | Self::Arrow(_) => "PARQUET_FAILURE",
            Self::Io(_) => "FILESYSTEM_FAILURE",
            Self::Serialization(_) => "SERIALIZATION_FAILURE",
        }
    }
}
