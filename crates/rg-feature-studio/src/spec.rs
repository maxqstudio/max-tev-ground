//! Feature-set requests, normalized identities, temporal semantics, and manifests.

use std::collections::{BTreeMap, BTreeSet};

use rg_data_ground::Timeframe;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    registry::{resolve_indicator, IndicatorConfig, ParameterValue},
    FeatureError,
};

pub const FEATURE_ENGINE_ID: &str = "rg-native-features";
pub const FEATURE_ENGINE_VERSION: &str = "1.0.0";
pub const FEATURE_SET_SCHEMA_VERSION: u16 = 1;
pub const MAX_FEATURES: usize = 32;
pub const MAX_INPUT_DATASETS: usize = 8;
pub const MAX_STALENESS_SECONDS: u64 = 31 * 86_400;
pub const MAX_AVAILABILITY_LAG_SECONDS: u64 = 7 * 86_400;
pub const RESEARCH_NOTICE: &str = "Derived research features computed from registered datasets. They are not trading signals, backtest results, or evidence of edge.";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(deny_unknown_fields)]
pub struct SeriesSelector {
    pub dataset_id: String,
    pub symbol: String,
    pub timeframe: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum FeatureSource {
    /// Computed on the target series; the row's own closed bar is the newest observation.
    Target,
    /// Computed on another series and joined as of the target decision time.
    Reference {
        series: SeriesSelector,
        /// Rows whose latest visible reference observation is older than this are STALE.
        max_staleness_seconds: u64,
        /// Extra delay after a reference bar's `available_at_utc` before it may be observed.
        availability_lag_seconds: u64,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FeatureSpec {
    pub key: String,
    pub indicator_id: String,
    pub indicator_version: u32,
    #[serde(default)]
    pub parameters: BTreeMap<String, ParameterValue>,
    pub source: FeatureSource,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FeatureQuotas {
    pub max_target_rows: u64,
    pub max_input_rows: u64,
    pub max_output_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FeatureSetRequest {
    pub target: SeriesSelector,
    pub features: Vec<FeatureSpec>,
    pub quotas: FeatureQuotas,
}

/// The explicit, versioned timing contract every feature set is bound to.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TemporalSemantics {
    pub version: u16,
    pub decision_boundary: String,
    pub current_bar: String,
    pub window_basis: String,
    pub warmup: String,
    pub reference_join: String,
    pub reference_staleness: String,
    pub gaps: String,
}

impl TemporalSemantics {
    pub fn current() -> Self {
        Self {
            version: 1,
            decision_boundary: "Each row's decision time is its target bar's available_at_utc, which RG-R01 sets to the canonical bar close.".into(),
            current_bar: "The target bar that closes at the decision time is included; no bar with a later availability is visible.".into(),
            window_basis: "Windows count observed closed bars of one series; closed-session gaps are neither filled nor counted.".into(),
            warmup: "Rows before an indicator is fully seeded are null with status WARMUP; nothing is back-filled from later bars.".into(),
            reference_join: "A reference value is visible only when its bar's available_at_utc plus the declared lag is at or before the decision time; the latest such bar is used, never a nearer future or unfinished bar.".into(),
            reference_staleness: "A reference older than its declared maximum staleness, or not yet observed, is null with status STALE or NO_OBSERVATION; values are never forward-filled beyond the limit.".into(),
            gaps: "No resampling, synthesis, centered windows, or forward labels. Higher timeframes must be imported series with their own canonical bar contracts.".into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NormalizedFeature {
    pub key: String,
    pub indicator_id: String,
    pub indicator_version: u32,
    pub parameters: BTreeMap<String, ParameterValue>,
    pub source: FeatureSource,
    pub outputs: Vec<String>,
    pub warmup_bars: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct InputDatasetIdentity {
    pub dataset_id: String,
    pub fingerprint: String,
    pub canonical_bars_sha256: String,
    pub canonical_bars_fingerprint: String,
    pub bar_count: u64,
}

/// Everything that determines a feature set's content. Its SHA-256 is the feature-set identity.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FeatureSetIdentity {
    pub schema_version: u16,
    pub engine_id: String,
    pub engine_version: String,
    pub temporal_semantics: TemporalSemantics,
    pub target: SeriesSelector,
    pub features: Vec<NormalizedFeature>,
    pub inputs: Vec<InputDatasetIdentity>,
}

impl FeatureSetIdentity {
    pub fn sha256(&self) -> Result<String, FeatureError> {
        Ok(hex(Sha256::digest(serde_json::to_vec(self)?)))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RowStatus {
    Ready,
    Warmup,
    Stale,
    NoObservation,
}

impl RowStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "READY",
            Self::Warmup => "WARMUP",
            Self::Stale => "STALE",
            Self::NoObservation => "NO_OBSERVATION",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ColumnKind {
    TimestampUtc,
    NullableTimestampUtc,
    NullableFloat64,
    Status,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OutputColumn {
    pub name: String,
    pub kind: ColumnKind,
    pub feature_key: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FeatureSummary {
    pub key: String,
    pub ready_rows: u64,
    pub warmup_rows: u64,
    pub stale_rows: u64,
    pub no_observation_rows: u64,
    pub first_ready_decision_utc: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InputLineage {
    pub dataset_id: String,
    pub fingerprint: String,
    pub source_id: String,
    pub broker: String,
    pub server: String,
    pub source_file_name: String,
    pub dataset_created_at_utc: String,
    pub series_used: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FeatureSetStatus {
    Ready,
    Blocked,
    Quarantined,
}

impl FeatureSetStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ready => "READY",
            Self::Blocked => "BLOCKED",
            Self::Quarantined => "QUARANTINED",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FeatureSetManifest {
    pub schema_version: u16,
    pub feature_set_id: String,
    pub identity_sha256: String,
    pub identity: FeatureSetIdentity,
    pub created_at_utc: String,
    pub output_file: String,
    pub output_sha256: String,
    pub output_fingerprint: String,
    pub row_count: u64,
    pub columns: Vec<OutputColumn>,
    pub feature_summaries: Vec<FeatureSummary>,
    pub lineage: Vec<InputLineage>,
    pub status: FeatureSetStatus,
    pub research_notice: String,
}

impl FeatureSetManifest {
    pub fn canonical_json(&self) -> Result<String, FeatureError> {
        Ok(serde_json::to_string(self)?)
    }

    /// The stored identity hashes to the recorded identity and ID, and the manifest is shaped
    /// like one this engine writes.
    pub fn identity_is_valid(&self) -> bool {
        self.schema_version == FEATURE_SET_SCHEMA_VERSION
            && self.output_file == "features.parquet"
            && self
                .identity
                .sha256()
                .is_ok_and(|digest| digest == self.identity_sha256)
            && self.feature_set_id == format!("fs_{}", self.identity_sha256)
            && is_sha256(&self.output_sha256)
            && is_sha256(&self.output_fingerprint)
            && self.status != FeatureSetStatus::Quarantined
    }
}

/// A request that passed structural and registry validation.
#[derive(Clone, Debug)]
pub struct ValidatedRequest {
    pub target: SeriesSelector,
    pub features: Vec<(NormalizedFeature, IndicatorConfig)>,
    pub dataset_ids: BTreeSet<String>,
    pub quotas: FeatureQuotas,
}

pub fn validate_request(request: &FeatureSetRequest) -> Result<ValidatedRequest, FeatureError> {
    let invalid = FeatureError::InvalidRequest;
    validate_selector(&request.target)?;
    if request.features.is_empty() || request.features.len() > MAX_FEATURES {
        return Err(invalid(format!(
            "a feature set needs between 1 and {MAX_FEATURES} features"
        )));
    }
    let quotas = &request.quotas;
    if quotas.max_target_rows == 0 || quotas.max_input_rows == 0 || quotas.max_output_bytes == 0 {
        return Err(invalid(
            "positive target-row, input-row, and output-byte quotas are required".into(),
        ));
    }
    let mut keys = BTreeSet::new();
    let mut dataset_ids = BTreeSet::from([request.target.dataset_id.clone()]);
    let mut features = Vec::with_capacity(request.features.len());
    for spec in &request.features {
        validate_key(&spec.key)?;
        if !keys.insert(spec.key.as_str()) {
            return Err(invalid(format!("feature key {} is used twice", spec.key)));
        }
        if let FeatureSource::Reference {
            series,
            max_staleness_seconds,
            availability_lag_seconds,
        } = &spec.source
        {
            validate_selector(series)?;
            if *series == request.target {
                return Err(invalid(format!(
                    "feature {} references the target series; use a TARGET source",
                    spec.key
                )));
            }
            if !(1..=MAX_STALENESS_SECONDS).contains(max_staleness_seconds) {
                return Err(invalid(format!(
                    "feature {} maximum staleness must be between 1 and {MAX_STALENESS_SECONDS} seconds",
                    spec.key
                )));
            }
            if *availability_lag_seconds > MAX_AVAILABILITY_LAG_SECONDS {
                return Err(invalid(format!(
                    "feature {} availability lag must be at most {MAX_AVAILABILITY_LAG_SECONDS} seconds",
                    spec.key
                )));
            }
            dataset_ids.insert(series.dataset_id.clone());
        }
        let resolved =
            resolve_indicator(&spec.indicator_id, spec.indicator_version, &spec.parameters)?;
        features.push((
            NormalizedFeature {
                key: spec.key.clone(),
                indicator_id: resolved.definition.id.to_owned(),
                indicator_version: resolved.definition.version,
                parameters: resolved.parameters,
                source: spec.source.clone(),
                outputs: resolved
                    .definition
                    .outputs
                    .iter()
                    .map(|output| (*output).to_owned())
                    .collect(),
                warmup_bars: resolved.config.warmup_bars(),
            },
            resolved.config,
        ));
    }
    if dataset_ids.len() > MAX_INPUT_DATASETS {
        return Err(invalid(format!(
            "a feature set can read at most {MAX_INPUT_DATASETS} datasets"
        )));
    }
    Ok(ValidatedRequest {
        target: request.target.clone(),
        features,
        dataset_ids,
        quotas: quotas.clone(),
    })
}

fn validate_selector(selector: &SeriesSelector) -> Result<(), FeatureError> {
    if !is_dataset_id(&selector.dataset_id) {
        return Err(FeatureError::InvalidRequest(format!(
            "{} is not a registered dataset identity (ds_ followed by a lowercase SHA-256)",
            selector.dataset_id
        )));
    }
    let symbol = &selector.symbol;
    if symbol.is_empty()
        || symbol.len() > 64
        || symbol
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(FeatureError::InvalidRequest(format!(
            "invalid market symbol: {symbol:?}"
        )));
    }
    let canonical = Timeframe::parse(&selector.timeframe)
        .map(Timeframe::as_str)
        .ok()
        .filter(|value| *value == selector.timeframe);
    if canonical.is_none() {
        return Err(FeatureError::InvalidRequest(format!(
            "timeframe must be a canonical RG-R01 timeframe such as H1, found {:?}",
            selector.timeframe
        )));
    }
    Ok(())
}

fn validate_key(key: &str) -> Result<(), FeatureError> {
    let mut characters = key.chars();
    let valid = key.len() <= 48
        && characters
            .next()
            .is_some_and(|first| first.is_ascii_lowercase())
        && characters.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !key.contains("__")
        && !key.ends_with('_');
    if !valid {
        return Err(FeatureError::InvalidRequest(format!(
            "feature key {key:?} must be 1-48 lowercase letters, digits, or single underscores and start with a letter"
        )));
    }
    Ok(())
}

pub(crate) fn is_dataset_id(value: &str) -> bool {
    value.len() == 67
        && value.starts_with("ds_")
        && value[3..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn dataset(seed: char) -> String {
        format!("ds_{}", seed.to_string().repeat(64))
    }

    fn request() -> FeatureSetRequest {
        FeatureSetRequest {
            target: SeriesSelector {
                dataset_id: dataset('a'),
                symbol: "EURUSD.m".into(),
                timeframe: "H1".into(),
            },
            features: vec![
                FeatureSpec {
                    key: "sma_20".into(),
                    indicator_id: "SMA".into(),
                    indicator_version: 1,
                    parameters: BTreeMap::new(),
                    source: FeatureSource::Target,
                },
                FeatureSpec {
                    key: "gbp_rsi".into(),
                    indicator_id: "RSI_WILDER".into(),
                    indicator_version: 1,
                    parameters: BTreeMap::new(),
                    source: FeatureSource::Reference {
                        series: SeriesSelector {
                            dataset_id: dataset('b'),
                            symbol: "GBPUSD.m".into(),
                            timeframe: "H4".into(),
                        },
                        max_staleness_seconds: 8 * 3600,
                        availability_lag_seconds: 0,
                    },
                },
            ],
            quotas: FeatureQuotas {
                max_target_rows: 1000,
                max_input_rows: 1000,
                max_output_bytes: 1_000_000,
            },
        }
    }

    #[test]
    fn gate_data_policy_requests_name_registered_dataset_identities_not_paths() {
        for dataset_id in [
            r"C:\Users\fixture\AppData\Roaming\MetaQuotes\Terminal\Common\Files\EURUSD.csv",
            "../datasets/ds_fixture",
            &format!("ds_{}", "A".repeat(64)),
            "ds_short",
        ] {
            let mut changed = request();
            changed.target.dataset_id = dataset_id.into();
            assert!(
                matches!(
                    validate_request(&changed),
                    Err(FeatureError::InvalidRequest(_))
                ),
                "{dataset_id}"
            );
        }
    }

    #[test]
    fn gate_feature_materialization_identity_changes_with_any_content_input() {
        let validated = validate_request(&request()).unwrap();
        let identity = |features: Vec<NormalizedFeature>, inputs: Vec<InputDatasetIdentity>| {
            FeatureSetIdentity {
                schema_version: FEATURE_SET_SCHEMA_VERSION,
                engine_id: FEATURE_ENGINE_ID.into(),
                engine_version: FEATURE_ENGINE_VERSION.into(),
                temporal_semantics: TemporalSemantics::current(),
                target: validated.target.clone(),
                features,
                inputs,
            }
            .sha256()
            .unwrap()
        };
        let features: Vec<_> = validated.features.iter().map(|(f, _)| f.clone()).collect();
        let inputs = vec![InputDatasetIdentity {
            dataset_id: dataset('a'),
            fingerprint: "a".repeat(64),
            canonical_bars_sha256: "c".repeat(64),
            canonical_bars_fingerprint: "d".repeat(64),
            bar_count: 10,
        }];
        let base = identity(features.clone(), inputs.clone());
        assert_eq!(base, identity(features.clone(), inputs.clone()));

        let mut changed_param = features.clone();
        changed_param[0]
            .parameters
            .insert("period".into(), ParameterValue::Integer(21));
        assert_ne!(base, identity(changed_param, inputs.clone()));
        let mut changed_staleness = features.clone();
        if let FeatureSource::Reference {
            max_staleness_seconds,
            ..
        } = &mut changed_staleness[1].source
        {
            *max_staleness_seconds += 1;
        }
        assert_ne!(base, identity(changed_staleness, inputs.clone()));
        let mut changed_input = inputs.clone();
        changed_input[0].canonical_bars_sha256 = "e".repeat(64);
        assert_ne!(base, identity(features, changed_input));
    }

    #[test]
    fn gate_multi_symbol_timeframe_join_requires_explicit_bounded_reference_contracts() {
        let mut stale = request();
        if let FeatureSource::Reference {
            max_staleness_seconds,
            ..
        } = &mut stale.features[1].source
        {
            *max_staleness_seconds = 0;
        }
        assert!(validate_request(&stale).is_err());
        let mut lagged = request();
        if let FeatureSource::Reference {
            availability_lag_seconds,
            ..
        } = &mut lagged.features[1].source
        {
            *availability_lag_seconds = MAX_AVAILABILITY_LAG_SECONDS + 1;
        }
        assert!(validate_request(&lagged).is_err());
        let mut self_reference = request();
        self_reference.features[1].source = FeatureSource::Reference {
            series: self_reference.target.clone(),
            max_staleness_seconds: 3600,
            availability_lag_seconds: 0,
        };
        assert!(validate_request(&self_reference).is_err());
        let missing_field = serde_json::json!({
            "kind": "REFERENCE",
            "series": {"dataset_id": dataset('b'), "symbol": "GBPUSD.m", "timeframe": "H4"},
            "max_staleness_seconds": 3600
        });
        assert!(serde_json::from_value::<FeatureSource>(missing_field).is_err());
    }

    #[test]
    fn gate_input_authority_request_rejects_non_canonical_series_and_keys() {
        for timeframe in ["h1", "60", "H2", ""] {
            let mut changed = request();
            changed.target.timeframe = timeframe.into();
            assert!(validate_request(&changed).is_err(), "{timeframe}");
        }
        for symbol in ["", " EURUSD", "EUR USD", &"X".repeat(65)] {
            let mut changed = request();
            changed.target.symbol = symbol.into();
            assert!(validate_request(&changed).is_err(), "{symbol}");
        }
        for key in [
            "",
            "Sma",
            "1sma",
            "sma__20",
            "sma_",
            "sma-20",
            &"a".repeat(49),
        ] {
            let mut changed = request();
            changed.features[0].key = key.into();
            assert!(validate_request(&changed).is_err(), "{key}");
        }
        let mut duplicate = request();
        duplicate.features[1].key = "sma_20".into();
        assert!(validate_request(&duplicate).is_err());
        let mut unbounded = request();
        unbounded.quotas.max_output_bytes = 0;
        assert!(validate_request(&unbounded).is_err());
        let mut many = request();
        many.features = (0..=MAX_FEATURES)
            .map(|index| FeatureSpec {
                key: format!("f{index}"),
                ..request().features[0].clone()
            })
            .collect();
        assert!(validate_request(&many).is_err());
        let unknown = serde_json::json!({
            "target": {"dataset_id": dataset('a'), "symbol": "EURUSD.m", "timeframe": "H1"},
            "features": [],
            "quotas": {"max_target_rows": 1, "max_input_rows": 1, "max_output_bytes": 1},
            "path": "C:/data.csv"
        });
        assert!(serde_json::from_value::<FeatureSetRequest>(unknown).is_err());
    }

    #[test]
    fn suffixed_symbols_are_distinct_series_identities() {
        let mut plain = request();
        plain.target.symbol = "EURUSD".into();
        let suffixed = request();
        assert_ne!(
            validate_request(&plain).unwrap().target,
            validate_request(&suffixed).unwrap().target
        );
    }
}
