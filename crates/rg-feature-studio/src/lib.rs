//! RG-R02 Feature Studio: a deterministic, causal technical-feature engine over verified,
//! immutable Data Ground datasets.

mod engine;
pub mod indicators;
pub mod registry;
pub mod spec;
pub mod store;

pub use engine::{
    FeatureProgress, PreviewFeatureValue, PreviewRow, ProgressPhase, ProgressSnapshot, PREVIEW_ROWS,
};
pub use registry::{indicator_definitions, IndicatorDefinition, ParameterValue};
pub use spec::{
    FeatureQuotas, FeatureSetManifest, FeatureSetRequest, FeatureSetStatus, FeatureSource,
    FeatureSpec, RowStatus, SeriesSelector, TemporalSemantics,
};

use rg_data_ground::store::DatasetStoreError;
use rg_persistence::PersistenceError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FeatureError {
    #[error("unknown indicator: {0}")]
    UnknownIndicator(String),
    #[error("indicator {indicator_id} version {requested} is unsupported; registry version is {supported}")]
    UnsupportedIndicatorVersion {
        indicator_id: String,
        requested: u32,
        supported: u32,
    },
    #[error("indicator {indicator_id} parameter {parameter} {reason}")]
    InvalidParameter {
        indicator_id: String,
        parameter: String,
        reason: String,
    },
    #[error("invalid feature request: {0}")]
    InvalidRequest(String),
    #[error("input dataset {0} is not registered")]
    InputMissing(String),
    #[error("input dataset {dataset_id} is {status}; only READY verified datasets are accepted")]
    InputNotReady { dataset_id: String, status: String },
    #[error("input dataset {dataset_id} failed integrity verification: {reason}")]
    InputIntegrity { dataset_id: String, reason: String },
    #[error("series {symbol} {timeframe} is absent from dataset {dataset_id}")]
    SeriesNotFound {
        dataset_id: String,
        symbol: String,
        timeframe: String,
    },
    #[error("resource quota exceeded: {0}")]
    ResourceQuota(String),
    #[error("feature computation cancelled")]
    Cancelled,
    #[error("feature {feature_key} failed at {decision_time_utc}: {source}")]
    Indicator {
        feature_key: String,
        decision_time_utc: String,
        source: indicators::IndicatorError,
    },
    #[error("prepared feature set failed verification: {0}")]
    InvalidPrepared(String),
    #[error("feature set {0} already exists but its stored content is not trustworthy")]
    ExistingFeatureSetCorrupt(String),
    #[error("replaying feature set {0} produced different output; the stored set was preserved")]
    NonDeterministicReplay(String),
    #[error("an unregistered feature set directory already occupies this identity")]
    OrphanDirectoryExists,
    #[error("feature set promotion recovery is blocked for {feature_set_id}: {reason}")]
    RecoveryBlocked {
        feature_set_id: String,
        reason: String,
    },
    #[error("feature store path is not a private regular directory or file")]
    UnsafeStorePath,
    #[error("feature store is unavailable because its lock was poisoned")]
    Poisoned,
    #[error("feature set ID is invalid")]
    InvalidFeatureSetId,
    #[error("feature set {feature_set_id} cannot be used downstream: status {status}")]
    FeatureSetNotUsable {
        feature_set_id: String,
        status: String,
    },
    #[error(transparent)]
    DatasetStore(#[from] DatasetStoreError),
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

impl FeatureError {
    /// Stable diagnostic code recorded with failed attempts and shown in the UI.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownIndicator(_) => "UNKNOWN_INDICATOR",
            Self::UnsupportedIndicatorVersion { .. } => "UNSUPPORTED_INDICATOR_VERSION",
            Self::InvalidParameter { .. } => "INVALID_PARAMETER",
            Self::InvalidRequest(_) => "INVALID_REQUEST",
            Self::InputMissing(_) => "INPUT_MISSING",
            Self::InputNotReady { .. } => "INPUT_NOT_READY",
            Self::InputIntegrity { .. } => "INPUT_INTEGRITY",
            Self::SeriesNotFound { .. } => "SERIES_NOT_FOUND",
            Self::ResourceQuota(_) => "RESOURCE_QUOTA",
            Self::Cancelled => "CANCELLED",
            Self::Indicator { .. } => "NON_FINITE_FEATURE",
            Self::InvalidPrepared(_) => "INVALID_PREPARED_FEATURE_SET",
            Self::ExistingFeatureSetCorrupt(_) => "EXISTING_FEATURE_SET_CORRUPT",
            Self::NonDeterministicReplay(_) => "NON_DETERMINISTIC_REPLAY",
            Self::OrphanDirectoryExists => "ORPHAN_DIRECTORY_EXISTS",
            Self::RecoveryBlocked { .. } => "PROMOTION_RECOVERY_BLOCKED",
            Self::UnsafeStorePath => "UNSAFE_STORE_PATH",
            Self::Poisoned => "STORE_UNAVAILABLE",
            Self::InvalidFeatureSetId => "INVALID_FEATURE_SET_ID",
            Self::FeatureSetNotUsable { .. } => "FEATURE_SET_NOT_USABLE",
            Self::DatasetStore(_) => "DATASET_STORE_FAILURE",
            Self::Persistence(_) => "PERSISTENCE_FAILURE",
            Self::Parquet(_) | Self::Arrow(_) => "PARQUET_FAILURE",
            Self::Io(_) => "FILESYSTEM_FAILURE",
            Self::Serialization(_) => "SERIALIZATION_FAILURE",
        }
    }
}
