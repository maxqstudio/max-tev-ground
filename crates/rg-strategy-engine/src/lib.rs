//! RG-R03 Deterministic Strategy Engine: typed, versioned research rules that turn one verified,
//! immutable RG-R02 feature set into one BUY, SELL, or SKIP decision per target closed bar.
//!
//! Decisions are research records. Nothing here places, sizes, or simulates orders, connects to a
//! broker or MT5, or computes positions, PnL, or backtest results.

mod engine;
pub mod registry;
pub mod spec;
pub mod store;

pub use engine::{
    DecisionProgress, PreviewDecision, PreviewInput, ProgressPhase, ProgressSnapshot, PREVIEW_ROWS,
};
pub use registry::{strategy_definitions, StrategyDefinition};
pub use spec::{
    Decision, DecisionQuotas, DecisionSetManifest, DecisionSetRequest, DecisionSetStatus,
    FeatureBinding, ReasonCode,
};

use rg_feature_studio::FeatureError;
use rg_persistence::PersistenceError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StrategyError {
    #[error("unknown strategy: {0}")]
    UnknownStrategy(String),
    #[error("strategy {strategy_id} version {requested} is unsupported; registry version is {supported}")]
    UnsupportedStrategyVersion {
        strategy_id: String,
        requested: u32,
        supported: u32,
    },
    #[error("strategy {strategy_id} parameter {parameter} {reason}")]
    InvalidParameter {
        strategy_id: String,
        parameter: String,
        reason: String,
    },
    #[error("strategy binding {role} {reason}")]
    InvalidBinding { role: String, reason: String },
    #[error("invalid decision request: {0}")]
    InvalidRequest(String),
    #[error("feature set {0} is not registered")]
    FeatureSetMissing(String),
    #[error("feature set {feature_set_id} cannot be used: status {status}")]
    FeatureSetNotUsable {
        feature_set_id: String,
        status: String,
    },
    #[error("feature set {feature_set_id} failed verification: {reason}")]
    FeatureSetIntegrity {
        feature_set_id: String,
        reason: String,
    },
    #[error("resource quota exceeded: {0}")]
    ResourceQuota(String),
    #[error("strategy evaluation cancelled")]
    Cancelled,
    #[error("prepared decision set failed verification: {0}")]
    InvalidPrepared(String),
    #[error("decision set {0} already exists but its stored content is not trustworthy")]
    ExistingDecisionSetCorrupt(String),
    #[error("replaying decision set {0} produced different output; the stored set was preserved")]
    NonDeterministicReplay(String),
    #[error("an unregistered decision set directory already occupies this identity")]
    OrphanDirectoryExists,
    #[error("decision set promotion recovery is blocked for {decision_set_id}: {reason}")]
    RecoveryBlocked {
        decision_set_id: String,
        reason: String,
    },
    #[error("decision store path is not a private regular directory or file")]
    UnsafeStorePath,
    #[error("decision store is unavailable because its lock was poisoned")]
    Poisoned,
    #[error("decision set ID is invalid")]
    InvalidDecisionSetId,
    #[error("decision set {decision_set_id} cannot be used downstream: status {status}")]
    DecisionSetNotUsable {
        decision_set_id: String,
        status: String,
    },
    #[error(transparent)]
    Feature(FeatureError),
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

impl From<FeatureError> for StrategyError {
    fn from(error: FeatureError) -> Self {
        match error {
            FeatureError::FeatureSetNotUsable {
                feature_set_id,
                status,
            } => Self::FeatureSetNotUsable {
                feature_set_id,
                status,
            },
            FeatureError::InvalidFeatureSetId => Self::InvalidRequest(
                "feature set ID is not a registered fs_<sha256> identity".into(),
            ),
            FeatureError::Cancelled => Self::Cancelled,
            other => Self::Feature(other),
        }
    }
}

impl StrategyError {
    /// Stable diagnostic code recorded with failed attempts and shown in the UI.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownStrategy(_) => "UNKNOWN_STRATEGY",
            Self::UnsupportedStrategyVersion { .. } => "UNSUPPORTED_STRATEGY_VERSION",
            Self::InvalidParameter { .. } => "INVALID_PARAMETER",
            Self::InvalidBinding { .. } => "INVALID_BINDING",
            Self::InvalidRequest(_) => "INVALID_REQUEST",
            Self::FeatureSetMissing(_) => "FEATURE_SET_MISSING",
            Self::FeatureSetNotUsable { .. } => "FEATURE_SET_NOT_USABLE",
            Self::FeatureSetIntegrity { .. } => "FEATURE_SET_INTEGRITY",
            Self::ResourceQuota(_) => "RESOURCE_QUOTA",
            Self::Cancelled => "CANCELLED",
            Self::InvalidPrepared(_) => "INVALID_PREPARED_DECISION_SET",
            Self::ExistingDecisionSetCorrupt(_) => "EXISTING_DECISION_SET_CORRUPT",
            Self::NonDeterministicReplay(_) => "NON_DETERMINISTIC_REPLAY",
            Self::OrphanDirectoryExists => "ORPHAN_DIRECTORY_EXISTS",
            Self::RecoveryBlocked { .. } => "PROMOTION_RECOVERY_BLOCKED",
            Self::UnsafeStorePath => "UNSAFE_STORE_PATH",
            Self::Poisoned => "STORE_UNAVAILABLE",
            Self::InvalidDecisionSetId => "INVALID_DECISION_SET_ID",
            Self::DecisionSetNotUsable { .. } => "DECISION_SET_NOT_USABLE",
            Self::Feature(_) => "FEATURE_STORE_FAILURE",
            Self::Persistence(_) => "PERSISTENCE_FAILURE",
            Self::Parquet(_) | Self::Arrow(_) => "PARQUET_FAILURE",
            Self::Io(_) => "FILESYSTEM_FAILURE",
            Self::Serialization(_) => "SERIALIZATION_FAILURE",
        }
    }
}
