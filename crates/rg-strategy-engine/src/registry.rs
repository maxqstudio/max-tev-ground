//! Native, versioned strategy definitions, typed parameter validation, and the pure decision
//! rules they evaluate.
//!
//! A definition names the feature inputs it needs by role and the source each role must come
//! from. It never names a symbol, broker suffix, or timeframe, so one definition applies to any
//! target series a verified feature set describes.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::StrategyError;

/// Largest absolute parameter value. Thresholds beyond it are rejected as unbounded.
pub const MAX_ABS_PARAMETER: f64 = 1.0e9;

/// The feature-set source a binding role must come from.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BindingSource {
    /// A feature computed on the target series itself.
    Target,
    /// A feature of another series, joined as of the target decision time by RG-R02.
    Reference,
}

impl BindingSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Target => "TARGET",
            Self::Reference => "REFERENCE",
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct BindingSpec {
    pub role: &'static str,
    pub description: &'static str,
    pub source: BindingSource,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct StrategyParameterSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub min: f64,
    pub max: f64,
    pub default: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StrategyKind {
    ThresholdBand,
    TrendContext,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct StrategyDefinition {
    pub id: &'static str,
    pub version: u32,
    pub name: &'static str,
    pub description: &'static str,
    pub decision_rule: &'static str,
    pub equality_semantics: &'static str,
    pub warmup_rule: &'static str,
    pub bindings: &'static [BindingSpec],
    pub parameters: &'static [StrategyParameterSpec],
    #[serde(skip)]
    kind: StrategyKind,
}

static DEFINITIONS: [StrategyDefinition; 2] = [
    StrategyDefinition {
        id: "THRESHOLD_BAND",
        version: 1,
        name: "Threshold band",
        description: "Compares one target-series feature with an upper and a lower threshold.",
        decision_rule: "BUY when signal > upper; SELL when signal < lower; otherwise SKIP with NO_SIGNAL.",
        equality_semantics: "Comparisons are strict: a signal equal to a threshold is NO_SIGNAL. lower must be strictly below upper.",
        warmup_rule: "No extra warmup; a row is SKIP while its bound feature is WARMUP.",
        bindings: &[BindingSpec {
            role: "signal",
            description: "Target-series feature compared with the thresholds",
            source: BindingSource::Target,
        }],
        parameters: &[
            StrategyParameterSpec {
                name: "upper",
                description: "Signal strictly above this emits BUY",
                min: -MAX_ABS_PARAMETER,
                max: MAX_ABS_PARAMETER,
                default: 70.0,
            },
            StrategyParameterSpec {
                name: "lower",
                description: "Signal strictly below this emits SELL",
                min: -MAX_ABS_PARAMETER,
                max: MAX_ABS_PARAMETER,
                default: 30.0,
            },
        ],
        kind: StrategyKind::ThresholdBand,
    },
    StrategyDefinition {
        id: "TREND_CONTEXT",
        version: 1,
        name: "Trend with context filter",
        description: "Compares a fast and a slow target-series feature, gated by a reference-market context feature.",
        decision_rule: "SKIP with CONTEXT_FILTER when context is outside [context_min, context_max]; SKIP with NUMERIC_OVERFLOW when fast - slow is not finite; else BUY when fast - slow > margin, SELL when slow - fast > margin, otherwise SKIP with NO_SIGNAL.",
        equality_semantics: "Trend comparisons are strict: a difference equal to margin is NO_SIGNAL. The context band is inclusive. context_min must not exceed context_max.",
        warmup_rule: "No extra warmup; a row is SKIP while any bound feature is WARMUP, STALE, or not yet observed.",
        bindings: &[
            BindingSpec {
                role: "fast",
                description: "Faster target-series feature, for example a short moving average",
                source: BindingSource::Target,
            },
            BindingSpec {
                role: "slow",
                description: "Slower target-series feature, for example a long moving average",
                source: BindingSource::Target,
            },
            BindingSpec {
                role: "context",
                description: "Reference-market feature that may only filter target decisions",
                source: BindingSource::Reference,
            },
        ],
        parameters: &[
            StrategyParameterSpec {
                name: "margin",
                description: "Minimum fast/slow separation before a direction is emitted",
                min: 0.0,
                max: MAX_ABS_PARAMETER,
                default: 0.0,
            },
            StrategyParameterSpec {
                name: "context_min",
                description: "Lowest context value that allows a decision (inclusive)",
                min: -MAX_ABS_PARAMETER,
                max: MAX_ABS_PARAMETER,
                default: 0.0,
            },
            StrategyParameterSpec {
                name: "context_max",
                description: "Highest context value that allows a decision (inclusive)",
                min: -MAX_ABS_PARAMETER,
                max: MAX_ABS_PARAMETER,
                default: 100.0,
            },
        ],
        kind: StrategyKind::TrendContext,
    },
];

pub fn strategy_definitions() -> &'static [StrategyDefinition] {
    &DEFINITIONS
}

/// Fully resolved, executable configuration of one strategy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StrategyConfig {
    ThresholdBand {
        upper: f64,
        lower: f64,
    },
    TrendContext {
        margin: f64,
        context_min: f64,
        context_max: f64,
    },
}

/// What a rule concluded from finite, available inputs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RuleOutcome {
    pub buy: bool,
    pub sell: bool,
    /// A context input was outside its allowed range; no direction may be emitted.
    pub context_blocked: bool,
    /// A value derived from finite inputs was not finite; no direction may be emitted.
    pub numeric_overflow: bool,
}

impl StrategyConfig {
    /// Evaluates the rule on inputs ordered as the definition's bindings. Callers guarantee that
    /// every input is present and finite.
    pub fn evaluate(&self, inputs: &[f64]) -> RuleOutcome {
        match *self {
            Self::ThresholdBand { upper, lower } => {
                let signal = inputs[0];
                RuleOutcome {
                    buy: signal > upper,
                    sell: signal < lower,
                    ..RuleOutcome::default()
                }
            }
            Self::TrendContext {
                margin,
                context_min,
                context_max,
            } => {
                let (fast, slow, context) = (inputs[0], inputs[1], inputs[2]);
                if !(context_min..=context_max).contains(&context) {
                    return RuleOutcome {
                        context_blocked: true,
                        ..RuleOutcome::default()
                    };
                }
                // Finite inputs can still overflow when subtracted; that is not a direction.
                // IEEE-754 subtraction is sign-symmetric, so slow - fast is exactly -(fast - slow).
                let difference = fast - slow;
                if !difference.is_finite() {
                    return RuleOutcome {
                        numeric_overflow: true,
                        ..RuleOutcome::default()
                    };
                }
                RuleOutcome {
                    buy: difference > margin,
                    sell: -difference > margin,
                    ..RuleOutcome::default()
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedStrategy {
    pub definition: &'static StrategyDefinition,
    pub parameters: BTreeMap<String, f64>,
    pub config: StrategyConfig,
}

/// Validates a strategy reference and its parameters, filling defaults so that equivalent requests
/// normalize to the same identity.
pub fn resolve_strategy(
    strategy_id: &str,
    version: u32,
    supplied: &BTreeMap<String, f64>,
) -> Result<ResolvedStrategy, StrategyError> {
    let definition = DEFINITIONS
        .iter()
        .find(|definition| definition.id == strategy_id)
        .ok_or_else(|| StrategyError::UnknownStrategy(strategy_id.into()))?;
    if version != definition.version {
        return Err(StrategyError::UnsupportedStrategyVersion {
            strategy_id: strategy_id.into(),
            requested: version,
            supported: definition.version,
        });
    }
    let invalid = |parameter: &str, reason: String| StrategyError::InvalidParameter {
        strategy_id: strategy_id.into(),
        parameter: parameter.into(),
        reason,
    };
    if let Some(unknown) = supplied
        .keys()
        .find(|name| !definition.parameters.iter().any(|spec| spec.name == *name))
    {
        return Err(invalid(unknown, "is not declared by this strategy".into()));
    }
    let mut parameters = BTreeMap::new();
    for spec in definition.parameters {
        let value = supplied.get(spec.name).copied().unwrap_or(spec.default);
        if !value.is_finite() || value < spec.min || value > spec.max {
            return Err(invalid(
                spec.name,
                format!(
                    "must be a finite number between {} and {}",
                    spec.min, spec.max
                ),
            ));
        }
        // -0.0 and 0.0 compare equal but serialize differently; normalize so identities agree.
        parameters.insert(spec.name.to_owned(), if value == 0.0 { 0.0 } else { value });
    }
    let value = |name: &str| parameters[name];
    let config = match definition.kind {
        StrategyKind::ThresholdBand => {
            let (upper, lower) = (value("upper"), value("lower"));
            if lower >= upper {
                return Err(invalid("lower", "must be strictly below upper".into()));
            }
            StrategyConfig::ThresholdBand { upper, lower }
        }
        StrategyKind::TrendContext => {
            let (context_min, context_max) = (value("context_min"), value("context_max"));
            if context_min > context_max {
                return Err(invalid("context_min", "must not exceed context_max".into()));
            }
            StrategyConfig::TrendContext {
                margin: value("margin"),
                context_min,
                context_max,
            }
        }
    };
    Ok(ResolvedStrategy {
        definition,
        parameters,
        config,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(values: &[(&str, f64)]) -> BTreeMap<String, f64> {
        values
            .iter()
            .map(|(name, value)| ((*name).to_owned(), *value))
            .collect()
    }

    #[test]
    fn gate_typed_strategy_registry_definitions_are_versioned_typed_and_market_neutral() {
        let definitions = strategy_definitions();
        assert_eq!(definitions.len(), 2);
        let mut ids = std::collections::BTreeSet::new();
        for definition in definitions {
            assert!(ids.insert(definition.id), "duplicate {}", definition.id);
            assert_eq!(definition.version, 1);
            assert!(!definition.bindings.is_empty());
            assert!(!definition.equality_semantics.is_empty());
            for spec in definition.parameters {
                assert!(
                    spec.min <= spec.default && spec.default <= spec.max,
                    "{}",
                    spec.name
                );
                assert!(spec.max <= MAX_ABS_PARAMETER && spec.min >= -MAX_ABS_PARAMETER);
            }
            // Definitions describe roles and sources, never a market, suffix, or timeframe.
            let text = serde_json::to_string(definition).unwrap();
            for market in [
                "EURUSD", "GBPUSD", "XAUUSD", ".m\"", "\"H1\"", "\"H4\"", "\"M15\"",
            ] {
                assert!(!text.contains(market), "{} names {market}", definition.id);
            }
        }
        let trend = &definitions[1];
        assert_eq!(
            trend
                .bindings
                .iter()
                .map(|binding| (binding.role, binding.source))
                .collect::<Vec<_>>(),
            [
                ("fast", BindingSource::Target),
                ("slow", BindingSource::Target),
                ("context", BindingSource::Reference)
            ]
        );
    }

    #[test]
    fn gate_typed_strategy_registry_rejects_unknown_versions_and_out_of_range_parameters() {
        assert!(matches!(
            resolve_strategy("BREAKOUT_MAGIC", 1, &BTreeMap::new()),
            Err(StrategyError::UnknownStrategy(_))
        ));
        assert!(matches!(
            resolve_strategy("THRESHOLD_BAND", 2, &BTreeMap::new()),
            Err(StrategyError::UnsupportedStrategyVersion { supported: 1, .. })
        ));
        for supplied in [
            params(&[("upper", 2.0e9)]),
            params(&[("lower", -2.0e9)]),
            params(&[("upper", f64::INFINITY)]),
            params(&[("upper", f64::NAN)]),
            params(&[("upper", 30.0), ("lower", 30.0)]),
            params(&[("upper", 10.0), ("lower", 20.0)]),
            params(&[("period", 14.0)]),
        ] {
            assert!(
                matches!(
                    resolve_strategy("THRESHOLD_BAND", 1, &supplied),
                    Err(StrategyError::InvalidParameter { .. })
                ),
                "{supplied:?}"
            );
        }
        for supplied in [
            params(&[("margin", -0.1)]),
            params(&[("context_min", 60.0), ("context_max", 40.0)]),
        ] {
            assert!(
                resolve_strategy("TREND_CONTEXT", 1, &supplied).is_err(),
                "{supplied:?}"
            );
        }
        let equal_band = params(&[("context_min", 50.0), ("context_max", 50.0)]);
        assert!(resolve_strategy("TREND_CONTEXT", 1, &equal_band).is_ok());
    }

    #[test]
    fn gate_typed_strategy_registry_defaults_normalize_equivalent_requests() {
        let implicit = resolve_strategy("THRESHOLD_BAND", 1, &BTreeMap::new()).unwrap();
        let explicit = resolve_strategy(
            "THRESHOLD_BAND",
            1,
            &params(&[("upper", 70.0), ("lower", 30.0)]),
        )
        .unwrap();
        assert_eq!(implicit.parameters, explicit.parameters);
        assert_eq!(implicit.config, explicit.config);
        let negative_zero =
            resolve_strategy("TREND_CONTEXT", 1, &params(&[("margin", -0.0)])).unwrap();
        assert_eq!(
            serde_json::to_string(&negative_zero.parameters).unwrap(),
            serde_json::to_string(
                &resolve_strategy("TREND_CONTEXT", 1, &BTreeMap::new())
                    .unwrap()
                    .parameters
            )
            .unwrap()
        );
    }

    #[test]
    fn gate_decision_semantics_rules_match_hand_calculated_golden_cases() {
        let band = StrategyConfig::ThresholdBand {
            upper: 70.0,
            lower: 30.0,
        };
        let outcome = |buy, sell, context_blocked| RuleOutcome {
            buy,
            sell,
            context_blocked,
            numeric_overflow: false,
        };
        // (signal, expected): strictly above 70 is BUY, strictly below 30 is SELL, the band
        // including both edges is NO_SIGNAL.
        for (signal, expected) in [
            (70.000001, outcome(true, false, false)),
            (70.0, outcome(false, false, false)),
            (50.0, outcome(false, false, false)),
            (30.0, outcome(false, false, false)),
            (29.999999, outcome(false, true, false)),
            (-1.0e9, outcome(false, true, false)),
        ] {
            assert_eq!(band.evaluate(&[signal]), expected, "{signal}");
        }
        let trend = StrategyConfig::TrendContext {
            margin: 0.5,
            context_min: 40.0,
            context_max: 60.0,
        };
        // (fast, slow, context, expected), worked by hand: fast - slow vs margin 0.5, context in
        // [40, 60] inclusive.
        for (fast, slow, context, expected) in [
            (2.0, 1.0, 50.0, outcome(true, false, false)),
            (1.5, 1.0, 50.0, outcome(false, false, false)),
            (1.0, 2.0, 50.0, outcome(false, true, false)),
            (1.0, 1.5, 50.0, outcome(false, false, false)),
            (2.0, 1.0, 40.0, outcome(true, false, false)),
            (2.0, 1.0, 60.0, outcome(true, false, false)),
            (2.0, 1.0, 39.9, outcome(false, false, true)),
            (1.0, 2.0, 60.1, outcome(false, false, true)),
        ] {
            assert_eq!(
                trend.evaluate(&[fast, slow, context]),
                expected,
                "{fast} {slow} {context}"
            );
        }
        // Finite inputs whose difference overflows give no direction in either order.
        for (fast, slow) in [(f64::MAX, -f64::MAX), (-f64::MAX, f64::MAX)] {
            assert_eq!(
                trend.evaluate(&[fast, slow, 50.0]),
                RuleOutcome {
                    numeric_overflow: true,
                    ..RuleOutcome::default()
                }
            );
        }
    }
}
