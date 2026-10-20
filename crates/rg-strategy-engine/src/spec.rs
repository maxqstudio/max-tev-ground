//! Decision-set requests, normalized identities, decision semantics, and manifests.

use std::collections::{BTreeMap, BTreeSet};

use rg_feature_studio::{
    spec::{ColumnKind, InputLineage},
    FeatureSetManifest, FeatureSource, SeriesSelector,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    registry::{resolve_strategy, BindingSource, RuleOutcome, StrategyConfig},
    StrategyError,
};

pub const STRATEGY_ENGINE_ID: &str = "rg-native-strategies";
pub const STRATEGY_ENGINE_VERSION: &str = "1.0.0";
pub const DECISION_SET_SCHEMA_VERSION: u16 = 1;
pub const DECISION_OUTPUT_FILE: &str = "decisions.parquet";
pub const RESEARCH_NOTICE: &str = "Research decisions from a deterministic rule over a registered feature set. They are not orders: nothing is sent to a broker or MT5, and no position, PnL, backtest, or evidence of edge is implied.";

/// Selects one output of one feature in the consumed feature set.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct FeatureBinding {
    pub feature_key: String,
    pub output: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DecisionQuotas {
    pub max_rows: u64,
    pub max_output_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DecisionSetRequest {
    pub feature_set_id: String,
    pub strategy_id: String,
    pub strategy_version: u32,
    #[serde(default)]
    pub parameters: BTreeMap<String, f64>,
    pub bindings: BTreeMap<String, FeatureBinding>,
    pub quotas: DecisionQuotas,
}

/// The explicit, versioned decision contract every decision set is bound to.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DecisionSemantics {
    pub version: u16,
    pub decision_rows: String,
    pub causality: String,
    pub context: String,
    pub skip_precedence: String,
    pub equality: String,
    pub conflict: String,
}

impl DecisionSemantics {
    pub fn current() -> Self {
        Self {
            version: 1,
            decision_rows: "Exactly one decision per row of the consumed feature set, that is, per closed bar of its one target symbol and timeframe, in the feature set's ascending decision-time order.".into(),
            causality: "A decision reads only the feature values of its own row, which RG-R02 computed from bars available at that row's decision time. No other row, future value, or label is read, and nothing is recomputed.".into(),
            context: "Reference-series features are context inputs only. They can filter a target decision but never create a decision row of their own.".into(),
            skip_precedence: "Bound inputs are checked in the definition's role order. The first role whose feature is WARMUP, STALE, or NO_OBSERVATION gives SKIP with that reason; a missing or non-finite READY value gives NON_FINITE_INPUT. Only then is the rule evaluated: CONTEXT_FILTER, then CONFLICT, then BUY, SELL, or NO_SIGNAL.".into(),
            equality: "Each definition states its comparison semantics; directional thresholds are strict, so equality yields NO_SIGNAL.".into(),
            conflict: "If BUY and SELL conditions both hold for a row, the decision is SKIP with CONFLICT. BUY and SELL are never emitted together.".into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NormalizedBinding {
    pub role: String,
    pub source: String,
    pub feature_key: String,
    pub output: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NormalizedStrategy {
    pub strategy_id: String,
    pub strategy_version: u32,
    pub parameters: BTreeMap<String, f64>,
    pub bindings: Vec<NormalizedBinding>,
}

/// The exact feature-set content a decision set was computed from.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FeatureSetInputIdentity {
    pub feature_set_id: String,
    pub identity_sha256: String,
    pub manifest_sha256: String,
    pub output_sha256: String,
    pub output_fingerprint: String,
    pub row_count: u64,
    pub target: SeriesSelector,
}

/// Everything that determines a decision set's content. Its SHA-256 is the decision-set identity.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DecisionSetIdentity {
    pub schema_version: u16,
    pub engine_id: String,
    pub engine_version: String,
    pub decision_semantics: DecisionSemantics,
    pub strategy: NormalizedStrategy,
    pub feature_set: FeatureSetInputIdentity,
}

impl DecisionSetIdentity {
    pub fn sha256(&self) -> Result<String, StrategyError> {
        Ok(hex(Sha256::digest(serde_json::to_vec(self)?)))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Decision {
    Buy,
    Sell,
    Skip,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "BUY",
            Self::Sell => "SELL",
            Self::Skip => "SKIP",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReasonCode {
    BuyRule,
    SellRule,
    Warmup,
    Stale,
    NoObservation,
    NonFiniteInput,
    ContextFilter,
    NoSignal,
    Conflict,
}

impl ReasonCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BuyRule => "BUY_RULE",
            Self::SellRule => "SELL_RULE",
            Self::Warmup => "WARMUP",
            Self::Stale => "STALE",
            Self::NoObservation => "NO_OBSERVATION",
            Self::NonFiniteInput => "NON_FINITE_INPUT",
            Self::ContextFilter => "CONTEXT_FILTER",
            Self::NoSignal => "NO_SIGNAL",
            Self::Conflict => "CONFLICT",
        }
    }
}

/// A bound feature value as it appears in one feature-set row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowInput<'a> {
    pub status: &'a str,
    pub value: Option<f64>,
}

/// One decision with its reason and, for input-driven SKIPs, the index of the role responsible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecisionOutcome {
    pub decision: Decision,
    pub reason: ReasonCode,
    pub role_index: Option<usize>,
}

/// Turns a rule outcome into exactly one decision. BUY and SELL together are a CONFLICT SKIP.
pub fn combine(outcome: RuleOutcome) -> (Decision, ReasonCode) {
    match outcome {
        RuleOutcome {
            context_blocked: true,
            ..
        } => (Decision::Skip, ReasonCode::ContextFilter),
        RuleOutcome {
            buy: true,
            sell: true,
            ..
        } => (Decision::Skip, ReasonCode::Conflict),
        RuleOutcome { buy: true, .. } => (Decision::Buy, ReasonCode::BuyRule),
        RuleOutcome { sell: true, .. } => (Decision::Sell, ReasonCode::SellRule),
        _ => (Decision::Skip, ReasonCode::NoSignal),
    }
}

/// Decides one row from inputs ordered as the definition's roles, applying the SKIP precedence of
/// [`DecisionSemantics`]. Returns `None` for a status this engine does not recognize.
pub fn decide(config: &StrategyConfig, inputs: &[RowInput<'_>]) -> Option<DecisionOutcome> {
    let skip = |reason, role_index| DecisionOutcome {
        decision: Decision::Skip,
        reason,
        role_index: Some(role_index),
    };
    let mut values = Vec::with_capacity(inputs.len());
    for (index, input) in inputs.iter().enumerate() {
        match input.status {
            "READY" => {}
            "WARMUP" => return Some(skip(ReasonCode::Warmup, index)),
            "STALE" => return Some(skip(ReasonCode::Stale, index)),
            "NO_OBSERVATION" => return Some(skip(ReasonCode::NoObservation, index)),
            _ => return None,
        }
        match input.value {
            Some(value) if value.is_finite() => values.push(value),
            _ => return Some(skip(ReasonCode::NonFiniteInput, index)),
        }
    }
    let (decision, reason) = combine(config.evaluate(&values));
    Some(DecisionOutcome {
        decision,
        reason,
        role_index: None,
    })
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DecisionColumnKind {
    Text,
    NullableText,
    TimestampUtc,
    NullableFloat64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DecisionColumn {
    pub name: String,
    pub kind: DecisionColumnKind,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DecisionSummary {
    pub buy_rows: u64,
    pub sell_rows: u64,
    pub skip_rows: u64,
    pub skip_reasons: BTreeMap<String, u64>,
    pub first_decision_utc: Option<String>,
    pub last_decision_utc: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DecisionLineage {
    pub feature_set_id: String,
    pub target: SeriesSelector,
    pub context_series: Vec<SeriesSelector>,
    pub datasets: Vec<InputLineage>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DecisionSetStatus {
    Ready,
    Quarantined,
}

impl DecisionSetStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ready => "READY",
            Self::Quarantined => "QUARANTINED",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DecisionSetManifest {
    pub schema_version: u16,
    pub decision_set_id: String,
    pub identity_sha256: String,
    pub identity: DecisionSetIdentity,
    pub created_at_utc: String,
    pub output_file: String,
    pub output_sha256: String,
    pub output_fingerprint: String,
    pub row_count: u64,
    pub columns: Vec<DecisionColumn>,
    pub summary: DecisionSummary,
    pub lineage: DecisionLineage,
    pub status: DecisionSetStatus,
    pub research_notice: String,
}

impl DecisionSetManifest {
    pub fn canonical_json(&self) -> Result<String, StrategyError> {
        Ok(serde_json::to_string(self)?)
    }

    /// The stored identity hashes to the recorded identity and ID, and the manifest is shaped
    /// like one this engine writes.
    pub fn identity_is_valid(&self) -> bool {
        self.schema_version == DECISION_SET_SCHEMA_VERSION
            && self.output_file == DECISION_OUTPUT_FILE
            && self
                .identity
                .sha256()
                .is_ok_and(|digest| digest == self.identity_sha256)
            && self.decision_set_id == format!("dc_{}", self.identity_sha256)
            && is_sha256(&self.output_sha256)
            && is_sha256(&self.output_fingerprint)
            && self.status == DecisionSetStatus::Ready
            && self.row_count == self.identity.feature_set.row_count
            && self.summary.buy_rows + self.summary.sell_rows + self.summary.skip_rows
                == self.row_count
    }
}

/// One bound role resolved to the feature-set columns that carry its value and status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundColumns {
    pub role: String,
    pub value_column: String,
    pub status_column: String,
}

/// A request that passed registry validation and binding validation against the feature set.
#[derive(Clone, Debug)]
pub struct ValidatedRequest {
    pub strategy: NormalizedStrategy,
    pub config: StrategyConfig,
    pub columns: Vec<BoundColumns>,
    pub context_series: Vec<SeriesSelector>,
    pub quotas: DecisionQuotas,
}

/// Validates the request against the strategy registry and the consumed feature set's manifest:
/// every declared role is bound exactly once to an existing feature output whose source kind
/// matches the role.
pub fn validate_request(
    request: &DecisionSetRequest,
    feature_set: &FeatureSetManifest,
) -> Result<ValidatedRequest, StrategyError> {
    if request.feature_set_id != feature_set.feature_set_id {
        return Err(StrategyError::InvalidRequest(
            "request names a different feature set than the one verified".into(),
        ));
    }
    if request.quotas.max_rows == 0 || request.quotas.max_output_bytes == 0 {
        return Err(StrategyError::InvalidRequest(
            "positive row and output-byte quotas are required".into(),
        ));
    }
    let resolved = resolve_strategy(
        &request.strategy_id,
        request.strategy_version,
        &request.parameters,
    )?;
    let definition = resolved.definition;
    if let Some(unknown) = request
        .bindings
        .keys()
        .find(|role| !definition.bindings.iter().any(|spec| spec.role == *role))
    {
        return Err(StrategyError::InvalidBinding {
            role: unknown.clone(),
            reason: format!("is not a role of strategy {}", definition.id),
        });
    }
    let mut bindings = Vec::with_capacity(definition.bindings.len());
    let mut columns = Vec::with_capacity(definition.bindings.len());
    let mut context_series = BTreeSet::new();
    for spec in definition.bindings {
        let invalid = |reason: String| StrategyError::InvalidBinding {
            role: spec.role.into(),
            reason,
        };
        let binding = request
            .bindings
            .get(spec.role)
            .ok_or_else(|| invalid("is required but not bound".into()))?;
        let feature = feature_set
            .identity
            .features
            .iter()
            .find(|feature| feature.key == binding.feature_key)
            .ok_or_else(|| {
                invalid(format!(
                    "names feature {:?}, which the feature set does not contain",
                    binding.feature_key
                ))
            })?;
        if !feature.outputs.contains(&binding.output) {
            return Err(invalid(format!(
                "names output {:?}; feature {} outputs {}",
                binding.output,
                feature.key,
                feature.outputs.join(", ")
            )));
        }
        let source = match &feature.source {
            FeatureSource::Target => BindingSource::Target,
            FeatureSource::Reference { series, .. } => {
                if spec.source == BindingSource::Reference {
                    context_series.insert(series.clone());
                }
                BindingSource::Reference
            }
        };
        if source != spec.source {
            return Err(invalid(format!(
                "requires a {} feature, but {} is a {} feature",
                spec.source.as_str(),
                feature.key,
                source.as_str()
            )));
        }
        let value_column = format!("{}__{}", feature.key, binding.output);
        let status_column = format!("{}__status", feature.key);
        let has_column = |name: &str, kind: ColumnKind| {
            feature_set
                .columns
                .iter()
                .any(|column| column.name == name && column.kind == kind)
        };
        if !has_column(&value_column, ColumnKind::NullableFloat64)
            || !has_column(&status_column, ColumnKind::Status)
        {
            return Err(invalid(
                "does not match the feature set's declared output columns".into(),
            ));
        }
        bindings.push(NormalizedBinding {
            role: spec.role.into(),
            source: spec.source.as_str().into(),
            feature_key: binding.feature_key.clone(),
            output: binding.output.clone(),
        });
        columns.push(BoundColumns {
            role: spec.role.into(),
            value_column,
            status_column,
        });
    }
    Ok(ValidatedRequest {
        strategy: NormalizedStrategy {
            strategy_id: definition.id.into(),
            strategy_version: definition.version,
            parameters: resolved.parameters,
            bindings,
        },
        config: resolved.config,
        columns,
        context_series: context_series.into_iter().collect(),
        quotas: request.quotas.clone(),
    })
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

    fn ready(value: f64) -> RowInput<'static> {
        RowInput {
            status: "READY",
            value: Some(value),
        }
    }

    #[test]
    fn gate_skip_and_conflict_handling_precedence_is_role_ordered_then_rule() {
        let trend = StrategyConfig::TrendContext {
            margin: 0.0,
            context_min: 0.0,
            context_max: 100.0,
        };
        let skip = |reason, role| DecisionOutcome {
            decision: Decision::Skip,
            reason,
            role_index: role,
        };
        let warmup = RowInput {
            status: "WARMUP",
            value: None,
        };
        let stale = RowInput {
            status: "STALE",
            value: None,
        };
        let unobserved = RowInput {
            status: "NO_OBSERVATION",
            value: None,
        };
        let missing = RowInput {
            status: "READY",
            value: None,
        };
        // The first unavailable role wins, in definition order fast, slow, context.
        for (inputs, expected) in [
            (
                [warmup, stale, unobserved],
                skip(ReasonCode::Warmup, Some(0)),
            ),
            (
                [ready(2.0), stale, unobserved],
                skip(ReasonCode::Stale, Some(1)),
            ),
            (
                [ready(2.0), ready(1.0), unobserved],
                skip(ReasonCode::NoObservation, Some(2)),
            ),
            (
                [ready(2.0), missing, ready(50.0)],
                skip(ReasonCode::NonFiniteInput, Some(1)),
            ),
            (
                [ready(f64::NAN), ready(1.0), ready(50.0)],
                skip(ReasonCode::NonFiniteInput, Some(0)),
            ),
            (
                [ready(f64::INFINITY), ready(1.0), ready(50.0)],
                skip(ReasonCode::NonFiniteInput, Some(0)),
            ),
            (
                [ready(2.0), ready(1.0), ready(150.0)],
                skip(ReasonCode::ContextFilter, None),
            ),
            (
                [ready(1.0), ready(1.0), ready(50.0)],
                skip(ReasonCode::NoSignal, None),
            ),
        ] {
            assert_eq!(decide(&trend, &inputs), Some(expected), "{inputs:?}");
        }
        assert_eq!(
            decide(&trend, &[ready(2.0), ready(1.0), ready(50.0)]),
            Some(DecisionOutcome {
                decision: Decision::Buy,
                reason: ReasonCode::BuyRule,
                role_index: None
            })
        );
        let unknown = RowInput {
            status: "FILLED",
            value: Some(1.0),
        };
        assert_eq!(decide(&trend, &[unknown, ready(1.0), ready(50.0)]), None);
    }

    #[test]
    fn gate_skip_and_conflict_handling_buy_and_sell_together_are_a_conflict_skip() {
        let both = RuleOutcome {
            buy: true,
            sell: true,
            context_blocked: false,
        };
        assert_eq!(combine(both), (Decision::Skip, ReasonCode::Conflict));
        // A context block outranks any direction.
        assert_eq!(
            combine(RuleOutcome {
                context_blocked: true,
                ..both
            }),
            (Decision::Skip, ReasonCode::ContextFilter)
        );
        for (buy, sell, expected) in [
            (true, false, (Decision::Buy, ReasonCode::BuyRule)),
            (false, true, (Decision::Sell, ReasonCode::SellRule)),
            (false, false, (Decision::Skip, ReasonCode::NoSignal)),
        ] {
            assert_eq!(
                combine(RuleOutcome {
                    buy,
                    sell,
                    context_blocked: false
                }),
                expected
            );
        }
    }

    #[test]
    fn gate_decision_semantics_contract_is_versioned_and_bound_to_identity() {
        let semantics = DecisionSemantics::current();
        assert_eq!(semantics.version, 1);
        assert!(semantics.conflict.contains("never emitted together"));
        assert!(semantics.context.contains("never create a decision row"));
        let unknown = serde_json::json!({
            "feature_set_id": format!("fs_{}", "a".repeat(64)),
            "strategy_id": "THRESHOLD_BAND",
            "strategy_version": 1,
            "bindings": {},
            "quotas": {"max_rows": 1, "max_output_bytes": 1},
            "order_size": 1.0
        });
        assert!(serde_json::from_value::<DecisionSetRequest>(unknown).is_err());
    }
}
