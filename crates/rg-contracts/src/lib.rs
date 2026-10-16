use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

pub const CONTRACT_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ContractError {
    #[error("invalid or empty field: {0}")]
    InvalidField(&'static str),
    #[error("unsupported contract schema version: {0}")]
    UnsupportedSchemaVersion(u16),
    #[error("invalid UTC timestamp: {0}")]
    InvalidTimestamp(String),
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':' | b'/'))
}

macro_rules! token_id {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, ContractError> {
                let value = value.into();
                if !valid_token(&value) {
                    return Err(ContractError::InvalidField(stringify!($name)));
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

token_id!(DatasetId);
token_id!(FeatureEngineId);
token_id!(FeatureId);
token_id!(StrategyId);
token_id!(ModelAdapterId);
token_id!(PolicyId);
token_id!(ExecutionSimulatorId);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExperimentId(String);

impl ExperimentId {
    pub fn new() -> Self {
        Self(Uuid::now_v7().to_string())
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, ContractError> {
        let value = value.into();
        let parsed =
            Uuid::parse_str(&value).map_err(|_| ContractError::InvalidField("experiment_id"))?;
        if parsed.get_version_num() != 7 {
            return Err(ContractError::InvalidField("experiment_id"));
        }
        Ok(Self(parsed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ExperimentId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ExperimentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Sha256Digest(String);

impl Sha256Digest {
    pub fn parse(value: impl Into<String>) -> Result<Self, ContractError> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ContractError::InvalidField("sha256_digest"));
        }
        Ok(Self(value))
    }

    pub fn of_bytes(value: &[u8]) -> Self {
        use std::fmt::Write;
        let digest = Sha256::digest(value);
        let mut encoded = String::with_capacity(64);
        for byte in digest {
            write!(&mut encoded, "{byte:02x}").expect("writing into String cannot fail");
        }
        Self(encoded)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetIdentity {
    pub dataset_id: DatasetId,
    pub fingerprint: Sha256Digest,
    pub source_id: String,
    pub symbol: String,
    pub timeframe: String,
    pub timestamps: CandleTimestampContract,
    pub missing_data: MissingDataPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandleTimestampContract {
    pub timezone: String,
    pub open_at_field: String,
    pub close_at_field: String,
    pub decision_available_at_field: String,
    pub decision_rule: DecisionAvailability,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DecisionAvailability {
    AtCandleClose,
    AtNextBarOpen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MissingDataPolicy {
    RejectDataset,
    PreserveGaps,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureEngineIdentity {
    pub engine_id: FeatureEngineId,
    pub version: String,
    pub feature_set: Vec<FeatureConfiguration>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureConfiguration {
    pub feature_id: FeatureId,
    pub version: String,
    pub timeframe: String,
    pub parameters: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategyIdentity {
    pub strategy_id: StrategyId,
    pub version: String,
    pub parameters: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelIdentity {
    pub name: String,
    pub digest: Sha256Digest,
    pub adapter_id: ModelAdapterId,
    pub runtime_configuration: serde_json::Map<String, serde_json::Value>,
    pub authority: ModelAuthority,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ModelAuthority {
    AdvisoryOnly,
    VetoOnly,
    RiskAdjustment,
    IndependentDecision,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyIdentity {
    pub policy_id: PolicyId,
    pub version: String,
    pub rule_digest: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiskConfiguration {
    /// Basis points of equity at risk per trade. 10_000 is 100%.
    pub risk_per_trade_bps: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSimulatorConfiguration {
    pub simulator_id: ExecutionSimulatorId,
    pub version: String,
    pub spread_model_id: String,
    pub commission_bps: u32,
    pub slippage_ticks: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemporalSplitConfiguration {
    pub discovery_start_utc: String,
    pub discovery_end_utc: String,
    pub tuning_end_utc: String,
    pub untouched_oos_start_utc: String,
    pub untouched_oos_end_utc: String,
    pub oos_exposure: OosExposure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OosExposure {
    Unexposed,
    Exposed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentManifest {
    pub schema_version: u16,
    pub experiment_id: ExperimentId,
    pub dataset: DatasetIdentity,
    pub feature_engine: FeatureEngineIdentity,
    pub strategy: StrategyIdentity,
    pub model: Option<ModelIdentity>,
    pub policy: Option<PolicyIdentity>,
    pub risk: RiskConfiguration,
    pub execution: ExecutionSimulatorConfiguration,
    pub temporal_split: TemporalSplitConfiguration,
    pub research_code_version: String,
    pub random_seed: Option<u64>,
}

impl ExperimentManifest {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != CONTRACT_SCHEMA_VERSION {
            return Err(ContractError::UnsupportedSchemaVersion(self.schema_version));
        }
        ExperimentId::parse(self.experiment_id.as_str())?;
        DatasetId::parse(self.dataset.dataset_id.as_str())?;
        FeatureEngineId::parse(self.feature_engine.engine_id.as_str())?;
        StrategyId::parse(self.strategy.strategy_id.as_str())?;
        ExecutionSimulatorId::parse(self.execution.simulator_id.as_str())?;
        Sha256Digest::parse(self.dataset.fingerprint.as_str())?;
        for (field, value) in [
            ("dataset.source_id", self.dataset.source_id.as_str()),
            ("dataset.symbol", self.dataset.symbol.as_str()),
            ("dataset.timeframe", self.dataset.timeframe.as_str()),
            (
                "dataset.timestamps.timezone",
                self.dataset.timestamps.timezone.as_str(),
            ),
            (
                "dataset.timestamps.open_at_field",
                self.dataset.timestamps.open_at_field.as_str(),
            ),
            (
                "dataset.timestamps.close_at_field",
                self.dataset.timestamps.close_at_field.as_str(),
            ),
            (
                "dataset.timestamps.decision_available_at_field",
                self.dataset.timestamps.decision_available_at_field.as_str(),
            ),
            (
                "feature_engine.version",
                self.feature_engine.version.as_str(),
            ),
            ("strategy.version", self.strategy.version.as_str()),
            ("research_code_version", self.research_code_version.as_str()),
            ("execution.version", self.execution.version.as_str()),
            (
                "execution.spread_model_id",
                self.execution.spread_model_id.as_str(),
            ),
        ] {
            require_nonempty(field, value)?;
        }
        if self.dataset.timestamps.timezone != "UTC" {
            return Err(ContractError::InvalidField("dataset.timestamps.timezone"));
        }
        for feature in &self.feature_engine.feature_set {
            FeatureId::parse(feature.feature_id.as_str())?;
            require_nonempty("feature.version", &feature.version)?;
            require_nonempty("feature.timeframe", &feature.timeframe)?;
        }
        if let Some(model) = &self.model {
            ModelAdapterId::parse(model.adapter_id.as_str())?;
            require_nonempty("model.name", &model.name)?;
            Sha256Digest::parse(model.digest.as_str())?;
        }
        if let Some(policy) = &self.policy {
            PolicyId::parse(policy.policy_id.as_str())?;
            require_nonempty("policy.version", &policy.version)?;
            Sha256Digest::parse(policy.rule_digest.as_str())?;
        }
        if self.risk.risk_per_trade_bps == 0 || self.risk.risk_per_trade_bps > 10_000 {
            return Err(ContractError::InvalidField("risk.risk_per_trade_bps"));
        }
        if self.execution.commission_bps > 10_000 {
            return Err(ContractError::InvalidField("execution.commission_bps"));
        }
        self.temporal_split.validate()
    }

    pub fn canonical_json(&self) -> Result<String, ContractError> {
        self.validate()?;
        serde_json::to_string(self)
            .map_err(|_| ContractError::InvalidField("manifest_serialization"))
    }

    pub fn fingerprint(&self) -> Result<Sha256Digest, ContractError> {
        Ok(Sha256Digest::of_bytes(self.canonical_json()?.as_bytes()))
    }
}

impl TemporalSplitConfiguration {
    pub fn validate(&self) -> Result<(), ContractError> {
        let timestamps = [
            &self.discovery_start_utc,
            &self.discovery_end_utc,
            &self.tuning_end_utc,
            &self.untouched_oos_start_utc,
            &self.untouched_oos_end_utc,
        ];
        for timestamp in timestamps {
            if !is_utc_timestamp(timestamp) {
                return Err(ContractError::InvalidTimestamp(timestamp.clone()));
            }
        }
        if !(self.discovery_start_utc < self.discovery_end_utc
            && self.discovery_end_utc <= self.tuning_end_utc
            && self.tuning_end_utc <= self.untouched_oos_start_utc
            && self.untouched_oos_start_utc < self.untouched_oos_end_utc)
        {
            return Err(ContractError::InvalidField("temporal_split.order"));
        }
        Ok(())
    }
}

fn require_nonempty(field: &'static str, value: &str) -> Result<(), ContractError> {
    if value.trim().is_empty() {
        return Err(ContractError::InvalidField(field));
    }
    Ok(())
}

fn is_utc_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
        || ![0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18]
            .iter()
            .all(|index| bytes[*index].is_ascii_digit())
    {
        return false;
    }
    let number = |start, end| {
        value[start..end]
            .parse::<u32>()
            .expect("timestamp digits checked")
    };
    let year = number(0, 4);
    let month = number(5, 7);
    let day = number(8, 10);
    let hour = number(11, 13);
    let minute = number(14, 16);
    let second = number(17, 19);
    if !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 59 {
        return false;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    (1..=days).contains(&day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest() -> ExperimentManifest {
        ExperimentManifest {
            schema_version: CONTRACT_SCHEMA_VERSION,
            experiment_id: ExperimentId::new(),
            dataset: DatasetIdentity {
                dataset_id: DatasetId::parse("eurusd-h1-v1").unwrap(),
                fingerprint: Sha256Digest::of_bytes(b"fixture dataset"),
                source_id: "fixture:contract-test".into(),
                symbol: "EURUSD.m".into(),
                timeframe: "H1".into(),
                timestamps: CandleTimestampContract {
                    timezone: "UTC".into(),
                    open_at_field: "open_time".into(),
                    close_at_field: "close_time".into(),
                    decision_available_at_field: "decision_available_at".into(),
                    decision_rule: DecisionAvailability::AtCandleClose,
                },
                missing_data: MissingDataPolicy::PreserveGaps,
            },
            feature_engine: FeatureEngineIdentity {
                engine_id: FeatureEngineId::parse("native-rust").unwrap(),
                version: "0.1.0".into(),
                feature_set: vec![FeatureConfiguration {
                    feature_id: FeatureId::parse("rsi").unwrap(),
                    version: "1".into(),
                    timeframe: "H1".into(),
                    parameters: [("period".into(), json!(14))].into_iter().collect(),
                }],
            },
            strategy: StrategyIdentity {
                strategy_id: StrategyId::parse("breakout").unwrap(),
                version: "1".into(),
                parameters: [("lookback".into(), json!(20))].into_iter().collect(),
            },
            model: None,
            policy: None,
            risk: RiskConfiguration {
                risk_per_trade_bps: 100,
            },
            execution: ExecutionSimulatorConfiguration {
                simulator_id: ExecutionSimulatorId::parse("bar-close-v1").unwrap(),
                version: "1".into(),
                spread_model_id: "fixed-spread-fixture".into(),
                commission_bps: 1,
                slippage_ticks: 0,
            },
            temporal_split: TemporalSplitConfiguration {
                discovery_start_utc: "2024-01-01T00:00:00Z".into(),
                discovery_end_utc: "2024-06-01T00:00:00Z".into(),
                tuning_end_utc: "2024-09-01T00:00:00Z".into(),
                untouched_oos_start_utc: "2024-10-01T00:00:00Z".into(),
                untouched_oos_end_utc: "2025-01-01T00:00:00Z".into(),
                oos_exposure: OosExposure::Unexposed,
            },
            research_code_version: "git:fixture".into(),
            random_seed: Some(7),
        }
    }

    #[test]
    fn manifest_round_trips_as_strict_versioned_json() {
        let value = manifest();
        let json = value.canonical_json().unwrap();
        let decoded: ExperimentManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, value);
        assert!(json.contains(r#""missing_data":"PRESERVE_GAPS""#));
    }

    #[test]
    fn model_authority_is_required_and_serialized_explicitly() {
        let model = ModelIdentity {
            name: "local-model:4b".into(),
            digest: Sha256Digest::of_bytes(b"weights"),
            adapter_id: ModelAdapterId::parse("ollama").unwrap(),
            runtime_configuration: Default::default(),
            authority: ModelAuthority::VetoOnly,
        };
        let mut value = serde_json::to_value(model).unwrap();
        assert_eq!(value["authority"], "VETO_ONLY");
        value.as_object_mut().unwrap().remove("authority");
        assert!(serde_json::from_value::<ModelIdentity>(value).is_err());
    }

    #[test]
    fn changed_configuration_changes_the_immutable_fingerprint() {
        let first = manifest();
        let mut changed = first.clone();
        changed
            .strategy
            .parameters
            .insert("lookback".into(), json!(21));
        assert_ne!(first.fingerprint().unwrap(), changed.fingerprint().unwrap());
    }

    #[test]
    fn invalid_risk_and_temporal_order_are_rejected() {
        let mut value = manifest();
        value.risk.risk_per_trade_bps = 10_001;
        assert!(value.validate().is_err());

        let mut value = manifest();
        value.temporal_split.untouched_oos_start_utc = "2024-08-01T00:00:00Z".into();
        assert_eq!(
            value.validate(),
            Err(ContractError::InvalidField("temporal_split.order"))
        );
    }

    #[test]
    fn invalid_calendar_dates_are_rejected() {
        let mut value = manifest();
        value.temporal_split.discovery_start_utc = "2023-02-29T00:00:00Z".into();
        assert!(matches!(
            value.validate(),
            Err(ContractError::InvalidTimestamp(_))
        ));
    }

    #[test]
    fn deserialized_typed_ids_are_validated_before_use() {
        let mut value = manifest();
        value.model = Some(ModelIdentity {
            name: "fixture-model".into(),
            digest: Sha256Digest::of_bytes(b"model"),
            adapter_id: ModelAdapterId::parse("ollama").unwrap(),
            runtime_configuration: Default::default(),
            authority: ModelAuthority::VetoOnly,
        });
        value.policy = Some(PolicyIdentity {
            policy_id: PolicyId::parse("threshold").unwrap(),
            version: "1".into(),
            rule_digest: Sha256Digest::of_bytes(b"policy"),
        });
        let paths = [
            "/dataset/dataset_id",
            "/feature_engine/engine_id",
            "/feature_engine/feature_set/0/feature_id",
            "/strategy/strategy_id",
            "/model/adapter_id",
            "/policy/policy_id",
            "/execution/simulator_id",
        ];

        for path in paths {
            let mut serialized = serde_json::to_value(&value).unwrap();
            *serialized.pointer_mut(path).unwrap() = serde_json::json!("invalid id");
            let decoded: ExperimentManifest = serde_json::from_value(serialized).unwrap();
            assert!(decoded.validate().is_err(), "accepted invalid ID at {path}");
        }
    }

    #[test]
    fn digest_is_sha256_hex() {
        assert_eq!(
            Sha256Digest::of_bytes(b"abc").as_str(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
