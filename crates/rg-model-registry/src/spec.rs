//! Model identities, typed runtime parameters, declared authority, inference requests, and the
//! immutable manifests of registered models and inference records.

use std::collections::{BTreeMap, BTreeSet};

use rg_feature_studio::{spec::InputLineage, SeriesSelector};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ollama::OllamaListing, onnx::OnnxModelInfo, ModelError};

pub const MODEL_REGISTRY_ID: &str = "rg-model-registry";
pub const MODEL_REGISTRY_VERSION: &str = "1.0.0";
pub const MODEL_SCHEMA_VERSION: u16 = 1;
pub const INFERENCE_SCHEMA_VERSION: u16 = 1;
pub const ONNX_CONTENT_FILE: &str = "model.onnx";
pub const INFERENCE_OUTPUT_FILE: &str = "inference.parquet";
pub const MODEL_NOTICE: &str = "A registered local research model. Registration records its identity only: it does not establish that the model is accurate, profitable, or suitable for trading.";
pub const INFERENCE_NOTICE: &str = "ADVISORY_ONLY research outputs of a registered local model over a registered input set. They never create, alter, veto, or override a strategy decision, size a position, or send anything to a broker or MT5, and they are not evidence of edge, PnL, or profitability.";

pub const MAX_INPUT_FIELDS: usize = 64;
pub const MAX_OUTPUT_FIELDS: usize = 16;
pub const MAX_CATEGORY_LABELS: usize = 16;
pub const MAX_INSTRUCTION_CHARS: usize = 2000;
pub const MAX_REQUEST_TIMEOUT_MS: u64 = 120_000;
pub const MIN_REQUEST_TIMEOUT_MS: u64 = 100;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AdapterKind {
    OllamaLoopback,
    OnnxNative,
}

impl AdapterKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OllamaLoopback => "OLLAMA_LOOPBACK",
            Self::OnnxNative => "ONNX_NATIVE",
        }
    }

    pub fn version(self) -> &'static str {
        match self {
            Self::OllamaLoopback => "ollama-generate-json/1",
            Self::OnnxNative => "rg-native-onnx/1",
        }
    }
}

/// What a model output is allowed to do. RG-R04 executes only [`Authority::AdvisoryOnly`].
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Authority {
    AdvisoryOnly,
}

/// Authorities that later phases may define. RG-R04 refuses them explicitly.
pub const LOCKED_AUTHORITIES: &[&str] = &[
    "DECISION_FILTER",
    "DECISION_VETO",
    "DECISION_OVERRIDE",
    "POSITION_SIZING",
    "ORDER_GENERATION",
];

pub fn parse_authority(value: &str) -> Result<Authority, ModelError> {
    match value {
        "ADVISORY_ONLY" => Ok(Authority::AdvisoryOnly),
        locked if LOCKED_AUTHORITIES.contains(&locked) => {
            Err(ModelError::AuthorityRefused(locked.into()))
        }
        other => Err(ModelError::InvalidRequest(format!(
            "authority {other:?} is unknown; RG-R04 executes only ADVISORY_ONLY outputs"
        ))),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum OutputKind {
    Float64 { minimum: f64, maximum: f64 },
    Category { labels: Vec<String> },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OutputField {
    pub name: String,
    pub kind: OutputKind,
}

/// Typed, bounded runtime parameters. Decoding settings that are not listed are fixed by the
/// adapter (see [`Decoding`]) and cannot be supplied.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(
    tag = "adapter",
    rename_all = "SCREAMING_SNAKE_CASE",
    deny_unknown_fields
)]
pub enum RuntimeConfig {
    OllamaLoopback {
        seed: u32,
        max_output_tokens: u32,
        context_tokens: u32,
        instruction: String,
    },
    OnnxNative {},
}

impl RuntimeConfig {
    pub fn adapter(&self) -> AdapterKind {
        match self {
            Self::OllamaLoopback { .. } => AdapterKind::OllamaLoopback,
            Self::OnnxNative {} => AdapterKind::OnnxNative,
        }
    }
}

pub const OLLAMA_OUTPUT_TOKENS: (u32, u32) = (16, 1024);
pub const OLLAMA_CONTEXT_TOKENS: (u32, u32) = (512, 32_768);

/// Settings each adapter fixes for every inference. They are part of the model identity.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(
    tag = "adapter",
    rename_all = "SCREAMING_SNAKE_CASE",
    deny_unknown_fields
)]
pub enum Decoding {
    OllamaLoopback {
        temperature: f64,
        top_k: u32,
        top_p: f64,
        stream: bool,
        structured_output: String,
        prompt_template_version: u16,
        prompt_contents: String,
    },
    OnnxNative {
        element_type: String,
        execution: String,
        batch_size: u16,
    },
}

impl Decoding {
    pub fn for_adapter(adapter: AdapterKind) -> Self {
        match adapter {
            AdapterKind::OllamaLoopback => Self::OllamaLoopback {
                temperature: 0.0,
                top_k: 1,
                top_p: 1.0,
                stream: false,
                structured_output: "JSON_SCHEMA".into(),
                prompt_template_version: crate::ollama::PROMPT_TEMPLATE_VERSION,
                prompt_contents: "The Owner's instruction and the bound input field names and values of one row only. No timestamp, symbol, timeframe, row position, or other row is included, so the model cannot be told which date it is evaluating.".into(),
            },
            AdapterKind::OnnxNative => Self::OnnxNative {
                element_type: "FLOAT32".into(),
                execution: "SEQUENTIAL_SINGLE_THREAD".into(),
                batch_size: 1,
            },
        }
    }
}

/// A discovered, unregistered model offered for explicit registration.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ModelCandidate {
    pub candidate_id: String,
    pub adapter: AdapterKind,
    pub model_name: String,
    pub content_digest: String,
    pub eligible: bool,
    pub reason: Option<String>,
    pub registered_model_ids: Vec<String>,
    pub ollama: Option<OllamaListing>,
    pub onnx: Option<OnnxModelInfo>,
    pub content_bytes: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelRegistrationRequest {
    pub candidate_id: String,
    pub input_fields: Vec<String>,
    pub outputs: Vec<OutputField>,
    pub runtime: RuntimeConfig,
    pub authority: String,
}

/// Everything that determines what a registered model computes. Its SHA-256 is the model ID.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelIdentity {
    pub schema_version: u16,
    pub registry_id: String,
    pub registry_version: String,
    pub adapter: AdapterKind,
    pub adapter_version: String,
    pub model_name: String,
    pub content_digest: String,
    pub input_fields: Vec<String>,
    pub outputs: Vec<OutputField>,
    pub runtime: RuntimeConfig,
    pub decoding: Decoding,
    pub onnx: Option<OnnxModelInfo>,
    pub authority: Authority,
}

impl ModelIdentity {
    pub fn sha256(&self) -> Result<String, ModelError> {
        Ok(hex(Sha256::digest(serde_json::to_vec(self)?)))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum ModelSource {
    OllamaListing {
        endpoint: String,
        listing: OllamaListing,
    },
    OnnxFile {
        source_file_name: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ModelStatus {
    Ready,
    Quarantined,
    RuntimeUnavailable,
}

impl ModelStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ready => "READY",
            Self::Quarantined => "QUARANTINED",
            Self::RuntimeUnavailable => "RUNTIME_UNAVAILABLE",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelManifest {
    pub schema_version: u16,
    pub model_id: String,
    pub identity_sha256: String,
    pub identity: ModelIdentity,
    pub registered_at_utc: String,
    pub content_file: Option<String>,
    pub content_bytes: Option<u64>,
    pub source: ModelSource,
    pub status: ModelStatus,
    pub research_notice: String,
}

impl ModelManifest {
    pub fn canonical_json(&self) -> Result<String, ModelError> {
        Ok(serde_json::to_string(self)?)
    }

    /// The stored identity hashes to the recorded ID and the manifest is shaped like one this
    /// registry writes.
    pub fn identity_is_valid(&self) -> bool {
        let identity = &self.identity;
        let shape_valid = match identity.adapter {
            AdapterKind::OllamaLoopback => {
                self.content_file.is_none()
                    && self.content_bytes.is_none()
                    && identity.onnx.is_none()
                    && matches!(self.source, ModelSource::OllamaListing { .. })
            }
            AdapterKind::OnnxNative => {
                self.content_file.as_deref() == Some(ONNX_CONTENT_FILE)
                    && self.content_bytes.is_some()
                    && identity.onnx.is_some()
                    && matches!(self.source, ModelSource::OnnxFile { .. })
            }
        };
        self.schema_version == MODEL_SCHEMA_VERSION
            && identity.schema_version == MODEL_SCHEMA_VERSION
            && identity.registry_id == MODEL_REGISTRY_ID
            && identity.adapter_version == identity.adapter.version()
            && identity.runtime.adapter() == identity.adapter
            && identity.decoding == Decoding::for_adapter(identity.adapter)
            && identity.authority == Authority::AdvisoryOnly
            && is_sha256(&identity.content_digest)
            && shape_valid
            && self
                .identity
                .sha256()
                .is_ok_and(|digest| digest == self.identity_sha256)
            && self.model_id == format!("md_{}", self.identity_sha256)
            && self.status == ModelStatus::Ready
    }
}

/// Validates field names, output schemas, runtime parameters, and authority; returns them in
/// normalized form. Adapter-specific checks against the candidate happen in the store.
pub fn validate_registration(
    request: &ModelRegistrationRequest,
) -> Result<(Vec<String>, Vec<OutputField>, RuntimeConfig, Authority), ModelError> {
    let authority = parse_authority(&request.authority)?;
    if request.input_fields.is_empty() || request.input_fields.len() > MAX_INPUT_FIELDS {
        return Err(ModelError::InvalidRequest(format!(
            "declare between 1 and {MAX_INPUT_FIELDS} input fields"
        )));
    }
    if request.outputs.is_empty() || request.outputs.len() > MAX_OUTPUT_FIELDS {
        return Err(ModelError::InvalidRequest(format!(
            "declare between 1 and {MAX_OUTPUT_FIELDS} output fields"
        )));
    }
    let mut names = BTreeSet::new();
    for name in &request.input_fields {
        validate_field_name(name)?;
        if !names.insert(name.clone()) {
            return Err(ModelError::InvalidRequest(format!(
                "input field {name} is declared twice"
            )));
        }
    }
    let mut output_names = BTreeSet::new();
    for output in &request.outputs {
        validate_field_name(&output.name)?;
        if !output_names.insert(output.name.clone()) {
            return Err(ModelError::InvalidRequest(format!(
                "output field {} is declared twice",
                output.name
            )));
        }
        match &output.kind {
            OutputKind::Float64 { minimum, maximum } => {
                if !minimum.is_finite() || !maximum.is_finite() || minimum >= maximum {
                    return Err(ModelError::InvalidRequest(format!(
                        "output {} needs finite bounds with minimum below maximum",
                        output.name
                    )));
                }
            }
            OutputKind::Category { labels } => {
                if labels.len() < 2 || labels.len() > MAX_CATEGORY_LABELS {
                    return Err(ModelError::InvalidRequest(format!(
                        "output {} needs between 2 and {MAX_CATEGORY_LABELS} labels",
                        output.name
                    )));
                }
                let mut seen = BTreeSet::new();
                for label in labels {
                    let valid = !label.is_empty()
                        && label.len() <= 32
                        && label.starts_with(|c: char| c.is_ascii_uppercase())
                        && label
                            .chars()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
                    if !valid || !seen.insert(label) {
                        return Err(ModelError::InvalidRequest(format!(
                            "output {} label {label:?} must be unique UPPER_SNAKE_CASE of at most 32 characters",
                            output.name
                        )));
                    }
                }
            }
        }
    }
    if let Some(shared) = names.intersection(&output_names).next() {
        return Err(ModelError::InvalidRequest(format!(
            "{shared} is both an input and an output field"
        )));
    }
    let runtime = match &request.runtime {
        RuntimeConfig::OllamaLoopback {
            seed,
            max_output_tokens,
            context_tokens,
            instruction,
        } => {
            let bounded = |name: &str, value: u32, (min, max): (u32, u32)| {
                if value < min || value > max {
                    Err(ModelError::InvalidRequest(format!(
                        "{name} must be between {min} and {max}"
                    )))
                } else {
                    Ok(())
                }
            };
            bounded(
                "max_output_tokens",
                *max_output_tokens,
                OLLAMA_OUTPUT_TOKENS,
            )?;
            bounded("context_tokens", *context_tokens, OLLAMA_CONTEXT_TOKENS)?;
            let instruction = instruction.trim();
            if instruction.is_empty()
                || instruction.chars().count() > MAX_INSTRUCTION_CHARS
                || instruction
                    .chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\t')
            {
                return Err(ModelError::InvalidRequest(format!(
                    "instruction must be 1 to {MAX_INSTRUCTION_CHARS} printable characters"
                )));
            }
            RuntimeConfig::OllamaLoopback {
                seed: *seed,
                max_output_tokens: *max_output_tokens,
                context_tokens: *context_tokens,
                instruction: instruction.into(),
            }
        }
        RuntimeConfig::OnnxNative {} => RuntimeConfig::OnnxNative {},
    };
    Ok((
        request.input_fields.clone(),
        request.outputs.clone(),
        runtime,
        authority,
    ))
}

pub fn validate_field_name(name: &str) -> Result<(), ModelError> {
    let valid = !name.is_empty()
        && name.len() <= 48
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if valid {
        Ok(())
    } else {
        Err(ModelError::InvalidRequest(format!(
            "field name {name:?} must be lower_snake_case of at most 48 characters"
        )))
    }
}

/// The registered input an inference reads.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum InputSetRef {
    FeatureSet { feature_set_id: String },
    DecisionSet { decision_set_id: String },
}

impl InputSetRef {
    pub fn id(&self) -> &str {
        match self {
            Self::FeatureSet { feature_set_id } => feature_set_id,
            Self::DecisionSet { decision_set_id } => decision_set_id,
        }
    }
}

/// Where one model input field reads its value in each row.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(
    tag = "source",
    rename_all = "SCREAMING_SNAKE_CASE",
    deny_unknown_fields
)]
pub enum InputColumn {
    /// One output of one feature in a feature set; its row status must be READY.
    Feature { feature_key: String, output: String },
    /// A bound-role input value recorded in a decision set (`input__<role>`).
    DecisionInput { role: String },
    /// The decision itself, encoded BUY = 1, SELL = -1, SKIP = 0.
    Decision {},
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InferenceQuotas {
    pub max_rows: u64,
    pub max_output_bytes: u64,
    pub request_timeout_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InferenceRequest {
    pub model_id: String,
    pub input: InputSetRef,
    pub bindings: BTreeMap<String, InputColumn>,
    pub authority: String,
    pub quotas: InferenceQuotas,
}

/// The explicit, versioned inference contract every record is bound to.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InferenceSemantics {
    pub version: u16,
    pub rows: String,
    pub causality: String,
    pub skip: String,
    pub invalid_output: String,
    pub authority: String,
    pub determinism: String,
}

impl InferenceSemantics {
    pub fn current() -> Self {
        Self {
            version: 1,
            rows: "Exactly one output row per row of the consumed feature or decision set, in its ascending decision-time order.".into(),
            causality: "A row's model input is built only from that row's own bound values, which upstream phases computed from bars available at its decision time. No other row, future value, label, timestamp, or symbol is given to the model.".into(),
            skip: "A row is SKIPPED, without calling the model, when a bound feature is WARMUP, STALE, or NO_OBSERVATION, or a bound value is missing or not finite.".into(),
            invalid_output: "A model answer that is not exactly the declared fields, has a non-finite or out-of-bounds number, or an undeclared label is recorded as INVALID_OUTPUT with no value. An unavailable, failing, or timed-out runtime stops the run; it is never recorded as model output.".into(),
            authority: "ADVISORY_ONLY: outputs are research records. They never create, alter, veto, or override a strategy decision, size a position, or emit an order.".into(),
            determinism: "Replaying a record recomputes its output digest. A match is recorded as REPRODUCED; a mismatch is recorded as NON_DETERMINISTIC and the stored record is never overwritten.".into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NormalizedInputBinding {
    pub field: String,
    pub source: String,
    pub column: String,
    pub status_column: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelReference {
    pub model_id: String,
    pub identity_sha256: String,
    pub adapter: AdapterKind,
    pub model_name: String,
    pub content_digest: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum InputKind {
    FeatureSet,
    DecisionSet,
}

/// The exact registered content an inference record was computed from.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InputSetIdentity {
    pub kind: InputKind,
    pub id: String,
    pub identity_sha256: String,
    pub manifest_sha256: String,
    pub output_sha256: String,
    pub output_fingerprint: String,
    pub row_count: u64,
    pub target: SeriesSelector,
}

/// Everything that determines an inference record's content. Its SHA-256 is the record ID.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InferenceIdentity {
    pub schema_version: u16,
    pub registry_id: String,
    pub registry_version: String,
    pub semantics: InferenceSemantics,
    pub model: ModelReference,
    pub input: InputSetIdentity,
    pub bindings: Vec<NormalizedInputBinding>,
    pub authority: Authority,
}

impl InferenceIdentity {
    pub fn sha256(&self) -> Result<String, ModelError> {
        Ok(hex(Sha256::digest(serde_json::to_vec(self)?)))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RowStatus {
    Ok,
    Skipped,
    InvalidOutput,
}

impl RowStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Skipped => "SKIPPED",
            Self::InvalidOutput => "INVALID_OUTPUT",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RowReason {
    Warmup,
    Stale,
    NoObservation,
    NonFiniteInput,
    SchemaMismatch,
    NonFiniteOutput,
    OutOfRange,
}

impl RowReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Warmup => "WARMUP",
            Self::Stale => "STALE",
            Self::NoObservation => "NO_OBSERVATION",
            Self::NonFiniteInput => "NON_FINITE_INPUT",
            Self::SchemaMismatch => "SCHEMA_MISMATCH",
            Self::NonFiniteOutput => "NON_FINITE_OUTPUT",
            Self::OutOfRange => "OUT_OF_RANGE",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum InferenceColumnKind {
    Text,
    NullableText,
    TimestampUtc,
    NullableFloat64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InferenceColumn {
    pub name: String,
    pub kind: InferenceColumnKind,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InferenceSummary {
    pub ok_rows: u64,
    pub skipped_rows: u64,
    pub invalid_output_rows: u64,
    pub reasons: BTreeMap<String, u64>,
    pub first_decision_utc: Option<String>,
    pub last_decision_utc: Option<String>,
}

/// Descriptive timings of the run that produced the record. Not part of the identity or the
/// output digest, because runtimes do not reproduce timings.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunStatistics {
    pub invoked_rows: u64,
    pub total_latency_us: u64,
    pub max_latency_us: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InferenceLineage {
    pub model_id: String,
    pub model_name: String,
    pub adapter: AdapterKind,
    pub content_digest: String,
    pub input_kind: InputKind,
    pub input_id: String,
    pub feature_set_id: String,
    pub target: SeriesSelector,
    pub datasets: Vec<InputLineage>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum InferenceRecordStatus {
    Ready,
    Quarantined,
}

impl InferenceRecordStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ready => "READY",
            Self::Quarantined => "QUARANTINED",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct InferenceRecordManifest {
    pub schema_version: u16,
    pub record_id: String,
    pub identity_sha256: String,
    pub identity: InferenceIdentity,
    pub created_at_utc: String,
    pub output_file: String,
    pub output_sha256: String,
    pub output_fingerprint: String,
    pub row_count: u64,
    pub columns: Vec<InferenceColumn>,
    pub outputs: Vec<OutputField>,
    pub summary: InferenceSummary,
    pub run: RunStatistics,
    pub lineage: InferenceLineage,
    pub authority: Authority,
    pub status: InferenceRecordStatus,
    pub research_notice: String,
}

impl InferenceRecordManifest {
    pub fn canonical_json(&self) -> Result<String, ModelError> {
        Ok(serde_json::to_string(self)?)
    }

    pub fn identity_is_valid(&self) -> bool {
        self.schema_version == INFERENCE_SCHEMA_VERSION
            && self.identity.schema_version == INFERENCE_SCHEMA_VERSION
            && self.identity.registry_id == MODEL_REGISTRY_ID
            && self.identity.authority == Authority::AdvisoryOnly
            && self.authority == Authority::AdvisoryOnly
            && self.output_file == INFERENCE_OUTPUT_FILE
            && self
                .identity
                .sha256()
                .is_ok_and(|digest| digest == self.identity_sha256)
            && self.record_id == format!("ir_{}", self.identity_sha256)
            && is_sha256(&self.output_sha256)
            && is_sha256(&self.output_fingerprint)
            && self.status == InferenceRecordStatus::Ready
            && self.row_count == self.identity.input.row_count
            && self.summary.ok_rows + self.summary.skipped_rows + self.summary.invalid_output_rows
                == self.row_count
            && self.lineage.model_id == self.identity.model.model_id
            && self.lineage.input_id == self.identity.input.id
    }
}

pub(crate) fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn hex(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn validate_prefixed_id(value: &str, prefix: &str) -> bool {
    value.len() == 67 && value.starts_with(prefix) && is_sha256(&value[3..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(authority: &str) -> ModelRegistrationRequest {
        ModelRegistrationRequest {
            candidate_id: "onnx:00000000-0000-7000-8000-000000000000".into(),
            input_fields: vec!["fast".into(), "slow".into()],
            outputs: vec![OutputField {
                name: "score".into(),
                kind: OutputKind::Float64 {
                    minimum: -1.0,
                    maximum: 1.0,
                },
            }],
            runtime: RuntimeConfig::OnnxNative {},
            authority: authority.into(),
        }
    }

    #[test]
    fn gate_model_authority_boundary_only_advisory_authority_is_executable() {
        assert!(validate_registration(&request("ADVISORY_ONLY")).is_ok());
        for locked in LOCKED_AUTHORITIES {
            assert!(
                matches!(
                    validate_registration(&request(locked)),
                    Err(ModelError::AuthorityRefused(name)) if name == *locked
                ),
                "{locked} must be refused"
            );
        }
        assert!(matches!(
            validate_registration(&request("LIVE_TRADING")),
            Err(ModelError::InvalidRequest(_))
        ));
    }

    #[test]
    fn gate_typed_runtime_adapters_registration_rejects_unknown_fields_and_out_of_bounds_parameters(
    ) {
        let unknown = serde_json::json!({
            "candidate_id": "x",
            "input_fields": ["a"],
            "outputs": [{"name": "y", "kind": {"kind": "FLOAT64", "minimum": 0.0, "maximum": 1.0}}],
            "runtime": {"adapter": "OLLAMA_LOOPBACK", "seed": 1, "max_output_tokens": 64,
                        "context_tokens": 2048, "instruction": "x", "temperature": 0.9},
            "authority": "ADVISORY_ONLY"
        });
        assert!(serde_json::from_value::<ModelRegistrationRequest>(unknown).is_err());
        let extra_top = serde_json::json!({
            "candidate_id": "x", "input_fields": ["a"], "outputs": [],
            "runtime": {"adapter": "ONNX_NATIVE"}, "authority": "ADVISORY_ONLY", "threads": 8
        });
        assert!(serde_json::from_value::<ModelRegistrationRequest>(extra_top).is_err());

        let mut bad = request("ADVISORY_ONLY");
        bad.runtime = RuntimeConfig::OllamaLoopback {
            seed: 7,
            max_output_tokens: 4096,
            context_tokens: 2048,
            instruction: "Classify.".into(),
        };
        assert!(validate_registration(&bad).is_err());
        bad.runtime = RuntimeConfig::OllamaLoopback {
            seed: 7,
            max_output_tokens: 64,
            context_tokens: 2048,
            instruction: " ".into(),
        };
        assert!(validate_registration(&bad).is_err());
        let mut bad = request("ADVISORY_ONLY");
        bad.outputs[0].kind = OutputKind::Float64 {
            minimum: 1.0,
            maximum: f64::INFINITY,
        };
        assert!(validate_registration(&bad).is_err());
        let mut bad = request("ADVISORY_ONLY");
        bad.outputs[0].kind = OutputKind::Category {
            labels: vec!["UP".into(), "UP".into()],
        };
        assert!(validate_registration(&bad).is_err());
        let mut bad = request("ADVISORY_ONLY");
        bad.input_fields = vec!["Fast".into()];
        assert!(validate_registration(&bad).is_err());
        let mut bad = request("ADVISORY_ONLY");
        bad.input_fields = vec!["score".into()];
        assert!(validate_registration(&bad).is_err());
    }
}
