//! Native, versioned indicator definitions and typed parameter validation.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::FeatureError;

/// Largest window accepted by any indicator. Windows are recomputed exactly from their
/// observations, so the bound also caps per-row CPU cost.
pub const MAX_PERIOD: i64 = 500;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PriceField {
    Open,
    High,
    Low,
    Close,
    /// `(high + low + close) / 3`.
    Typical,
}

impl PriceField {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "OPEN" => Some(Self::Open),
            "HIGH" => Some(Self::High),
            "LOW" => Some(Self::Low),
            "CLOSE" => Some(Self::Close),
            "TYPICAL" => Some(Self::Typical),
            _ => None,
        }
    }
}

const PRICE_OPTIONS: &[&str] = &["OPEN", "HIGH", "LOW", "CLOSE", "TYPICAL"];

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ParameterKind {
    Integer {
        min: i64,
        max: i64,
        default: i64,
    },
    Number {
        min: f64,
        max: f64,
        default: f64,
    },
    Choice {
        options: &'static [&'static str],
        default: &'static str,
    },
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ParameterSpec {
    pub name: &'static str,
    pub description: &'static str,
    #[serde(flatten)]
    pub kind: ParameterKind,
}

/// Which bar fields an indicator reads. Volume is deliberately absent: RG-R01 manifests do not
/// declare whether volume is tick volume or exchange volume, so no RG-R02 indicator consumes it.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum InputRequirement {
    SelectedPrice,
    HighLowClose,
    HighLow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum IndicatorKind {
    Sma,
    Ema,
    RsiWilder,
    AtrWilder,
    StdDev,
    Bollinger,
    Roc,
    LogReturn,
    Donchian,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct IndicatorDefinition {
    pub id: &'static str,
    pub version: u32,
    pub name: &'static str,
    pub description: &'static str,
    pub formula: &'static str,
    pub warmup_rule: &'static str,
    pub inputs: InputRequirement,
    pub outputs: &'static [&'static str],
    pub parameters: &'static [ParameterSpec],
    #[serde(skip)]
    kind: IndicatorKind,
}

const fn period(min: i64, default: i64) -> ParameterSpec {
    ParameterSpec {
        name: "period",
        description: "Number of observed closed bars in the causal window",
        kind: ParameterKind::Integer {
            min,
            max: MAX_PERIOD,
            default,
        },
    }
}

const PRICE: ParameterSpec = ParameterSpec {
    name: "price",
    description: "Bar field used as the indicator input",
    kind: ParameterKind::Choice {
        options: PRICE_OPTIONS,
        default: "CLOSE",
    },
};

const BAND_WIDTH: ParameterSpec = ParameterSpec {
    name: "k",
    description: "Band half-width in population standard deviations",
    kind: ParameterKind::Number {
        min: 0.1,
        max: 10.0,
        default: 2.0,
    },
};

static DEFINITIONS: [IndicatorDefinition; 9] = [
    IndicatorDefinition {
        id: "SMA",
        version: 1,
        name: "Simple moving average",
        description: "Arithmetic mean of the selected price over the window.",
        formula: "sum(x[t-n+1..=t]) / n, summed oldest to newest",
        warmup_rule: "First n-1 rows are WARMUP",
        inputs: InputRequirement::SelectedPrice,
        outputs: &["value"],
        parameters: &[period(1, 20), PRICE],
        kind: IndicatorKind::Sma,
    },
    IndicatorDefinition {
        id: "EMA",
        version: 1,
        name: "Exponential moving average",
        description: "SMA-seeded exponential average with alpha = 2 / (n + 1).",
        formula: "ema[n-1] = SMA(n); ema[t] = alpha * x[t] + (1 - alpha) * ema[t-1]",
        warmup_rule: "First n-1 rows are WARMUP",
        inputs: InputRequirement::SelectedPrice,
        outputs: &["value"],
        parameters: &[period(2, 20), PRICE],
        kind: IndicatorKind::Ema,
    },
    IndicatorDefinition {
        id: "RSI_WILDER",
        version: 1,
        name: "Relative strength index (Wilder)",
        description: "Wilder-smoothed RSI of bar-to-bar price changes.",
        formula: "avg seeded by the mean of the first n changes, then avg = (avg * (n-1) + x) / n; RSI = 100 - 100 / (1 + gain/loss); loss = 0 gives 100, flat gives 50",
        warmup_rule: "First n rows are WARMUP (n changes need n+1 bars)",
        inputs: InputRequirement::SelectedPrice,
        outputs: &["value"],
        parameters: &[period(2, 14), PRICE],
        kind: IndicatorKind::RsiWilder,
    },
    IndicatorDefinition {
        id: "ATR_WILDER",
        version: 1,
        name: "Average true range (Wilder)",
        description: "Wilder-smoothed true range; the first bar's true range is high - low.",
        formula: "TR = max(h-l, |h-c[t-1]|, |l-c[t-1]|); atr seeded by mean(TR[0..n]), then atr = (atr * (n-1) + TR) / n",
        warmup_rule: "First n-1 rows are WARMUP",
        inputs: InputRequirement::HighLowClose,
        outputs: &["value"],
        parameters: &[period(2, 14)],
        kind: IndicatorKind::AtrWilder,
    },
    IndicatorDefinition {
        id: "STDDEV",
        version: 1,
        name: "Rolling standard deviation",
        description: "Population standard deviation (divide by n) of the selected price.",
        formula: "sqrt(sum((x - mean)^2) / n) over the window, two-pass",
        warmup_rule: "First n-1 rows are WARMUP",
        inputs: InputRequirement::SelectedPrice,
        outputs: &["value"],
        parameters: &[period(2, 20), PRICE],
        kind: IndicatorKind::StdDev,
    },
    IndicatorDefinition {
        id: "BOLLINGER",
        version: 1,
        name: "Bollinger bands",
        description: "SMA middle band with bands at k population standard deviations.",
        formula: "middle = SMA(n); upper/lower = middle +/- k * STDDEV(n)",
        warmup_rule: "First n-1 rows are WARMUP",
        inputs: InputRequirement::SelectedPrice,
        outputs: &["upper", "middle", "lower"],
        parameters: &[period(2, 20), BAND_WIDTH, PRICE],
        kind: IndicatorKind::Bollinger,
    },
    IndicatorDefinition {
        id: "ROC",
        version: 1,
        name: "Rate of change",
        description: "Percentage change from the observation n bars earlier.",
        formula: "100 * (x[t] - x[t-n]) / x[t-n]",
        warmup_rule: "First n rows are WARMUP",
        inputs: InputRequirement::SelectedPrice,
        outputs: &["value"],
        parameters: &[period(1, 10), PRICE],
        kind: IndicatorKind::Roc,
    },
    IndicatorDefinition {
        id: "LOG_RETURN",
        version: 1,
        name: "Log return",
        description: "Natural-log return from the observation n bars earlier.",
        formula: "ln(x[t] / x[t-n])",
        warmup_rule: "First n rows are WARMUP",
        inputs: InputRequirement::SelectedPrice,
        outputs: &["value"],
        parameters: &[period(1, 1), PRICE],
        kind: IndicatorKind::LogReturn,
    },
    IndicatorDefinition {
        id: "DONCHIAN",
        version: 1,
        name: "Donchian channel",
        description: "Highest high and lowest low over the window, including the current closed bar.",
        formula: "upper = max(high[t-n+1..=t]); lower = min(low[...]); middle = (upper + lower) / 2",
        warmup_rule: "First n-1 rows are WARMUP",
        inputs: InputRequirement::HighLow,
        outputs: &["upper", "middle", "lower"],
        parameters: &[period(2, 20)],
        kind: IndicatorKind::Donchian,
    },
];

pub fn indicator_definitions() -> &'static [IndicatorDefinition] {
    &DEFINITIONS
}

/// A parameter value as supplied by a request. Integers deserialize as `Integer`, other JSON
/// numbers as `Number`, and strings as `Choice`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum ParameterValue {
    Integer(i64),
    Number(f64),
    Choice(String),
}

/// Fully resolved, executable configuration of one indicator.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IndicatorConfig {
    Sma {
        period: usize,
        price: PriceField,
    },
    Ema {
        period: usize,
        price: PriceField,
    },
    RsiWilder {
        period: usize,
        price: PriceField,
    },
    AtrWilder {
        period: usize,
    },
    StdDev {
        period: usize,
        price: PriceField,
    },
    Bollinger {
        period: usize,
        k: f64,
        price: PriceField,
    },
    Roc {
        period: usize,
        price: PriceField,
    },
    LogReturn {
        period: usize,
        price: PriceField,
    },
    Donchian {
        period: usize,
    },
}

impl IndicatorConfig {
    /// Number of leading rows that are WARMUP before the first ready value.
    pub fn warmup_bars(&self) -> u64 {
        let leading = match *self {
            Self::Sma { period, .. }
            | Self::Ema { period, .. }
            | Self::AtrWilder { period }
            | Self::StdDev { period, .. }
            | Self::Bollinger { period, .. }
            | Self::Donchian { period } => period - 1,
            Self::RsiWilder { period, .. }
            | Self::Roc { period, .. }
            | Self::LogReturn { period, .. } => period,
        };
        leading as u64
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedIndicator {
    pub definition: &'static IndicatorDefinition,
    pub parameters: BTreeMap<String, ParameterValue>,
    pub config: IndicatorConfig,
}

/// Validates an indicator reference and its parameters against the registry, filling defaults so
/// that equivalent requests normalize to the same identity.
pub fn resolve_indicator(
    indicator_id: &str,
    version: u32,
    supplied: &BTreeMap<String, ParameterValue>,
) -> Result<ResolvedIndicator, FeatureError> {
    let definition = DEFINITIONS
        .iter()
        .find(|definition| definition.id == indicator_id)
        .ok_or_else(|| FeatureError::UnknownIndicator(indicator_id.to_owned()))?;
    if definition.version != version {
        return Err(FeatureError::UnsupportedIndicatorVersion {
            indicator_id: indicator_id.to_owned(),
            requested: version,
            supported: definition.version,
        });
    }
    if let Some(unknown) = supplied
        .keys()
        .find(|name| !definition.parameters.iter().any(|spec| spec.name == *name))
    {
        return Err(FeatureError::InvalidParameter {
            indicator_id: indicator_id.to_owned(),
            parameter: unknown.clone(),
            reason: "is not declared by this indicator".into(),
        });
    }
    let mut parameters = BTreeMap::new();
    for spec in definition.parameters {
        let invalid = |reason: String| FeatureError::InvalidParameter {
            indicator_id: indicator_id.to_owned(),
            parameter: spec.name.to_owned(),
            reason,
        };
        let value = match (&spec.kind, supplied.get(spec.name)) {
            (ParameterKind::Integer { default, .. }, None) => ParameterValue::Integer(*default),
            (ParameterKind::Integer { min, max, .. }, Some(ParameterValue::Integer(value))) => {
                if value < min || value > max {
                    return Err(invalid(format!("must be between {min} and {max}")));
                }
                ParameterValue::Integer(*value)
            }
            (ParameterKind::Integer { .. }, Some(_)) => {
                return Err(invalid("must be an integer".into()))
            }
            (ParameterKind::Number { default, .. }, None) => ParameterValue::Number(*default),
            (ParameterKind::Number { min, max, .. }, Some(supplied)) => {
                let value = match supplied {
                    ParameterValue::Number(value) => *value,
                    ParameterValue::Integer(value) => *value as f64,
                    ParameterValue::Choice(_) => return Err(invalid("must be a number".into())),
                };
                if !value.is_finite() || value < *min || value > *max {
                    return Err(invalid(format!(
                        "must be a finite number between {min} and {max}"
                    )));
                }
                ParameterValue::Number(value)
            }
            (ParameterKind::Choice { default, .. }, None) => {
                ParameterValue::Choice((*default).to_owned())
            }
            (ParameterKind::Choice { options, .. }, Some(ParameterValue::Choice(value))) => {
                if !options.contains(&value.as_str()) {
                    return Err(invalid(format!("must be one of {}", options.join(", "))));
                }
                ParameterValue::Choice(value.clone())
            }
            (ParameterKind::Choice { .. }, Some(_)) => {
                return Err(invalid("must be a text choice".into()))
            }
        };
        parameters.insert(spec.name.to_owned(), value);
    }
    let config = build_config(definition.kind, &parameters).ok_or_else(|| {
        FeatureError::InvalidParameter {
            indicator_id: indicator_id.to_owned(),
            parameter: "*".into(),
            reason: "normalized parameters could not be bound".into(),
        }
    })?;
    Ok(ResolvedIndicator {
        definition,
        parameters,
        config,
    })
}

fn build_config(
    kind: IndicatorKind,
    parameters: &BTreeMap<String, ParameterValue>,
) -> Option<IndicatorConfig> {
    let period = || match parameters.get("period") {
        Some(ParameterValue::Integer(value)) => usize::try_from(*value).ok(),
        _ => None,
    };
    let price = || match parameters.get("price") {
        Some(ParameterValue::Choice(value)) => PriceField::parse(value),
        _ => None,
    };
    Some(match kind {
        IndicatorKind::Sma => IndicatorConfig::Sma {
            period: period()?,
            price: price()?,
        },
        IndicatorKind::Ema => IndicatorConfig::Ema {
            period: period()?,
            price: price()?,
        },
        IndicatorKind::RsiWilder => IndicatorConfig::RsiWilder {
            period: period()?,
            price: price()?,
        },
        IndicatorKind::AtrWilder => IndicatorConfig::AtrWilder { period: period()? },
        IndicatorKind::StdDev => IndicatorConfig::StdDev {
            period: period()?,
            price: price()?,
        },
        IndicatorKind::Bollinger => IndicatorConfig::Bollinger {
            period: period()?,
            k: match parameters.get("k") {
                Some(ParameterValue::Number(value)) => *value,
                _ => return None,
            },
            price: price()?,
        },
        IndicatorKind::Roc => IndicatorConfig::Roc {
            period: period()?,
            price: price()?,
        },
        IndicatorKind::LogReturn => IndicatorConfig::LogReturn {
            period: period()?,
            price: price()?,
        },
        IndicatorKind::Donchian => IndicatorConfig::Donchian { period: period()? },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(values: &[(&str, ParameterValue)]) -> BTreeMap<String, ParameterValue> {
        values
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone()))
            .collect()
    }

    #[test]
    fn gate_indicator_registry_identities_are_unique_versioned_and_volume_free() {
        let definitions = indicator_definitions();
        let ids: std::collections::BTreeSet<_> = definitions.iter().map(|item| item.id).collect();
        assert_eq!(ids.len(), definitions.len());
        for definition in definitions {
            assert!(definition.version >= 1, "{}", definition.id);
            assert!(!definition.outputs.is_empty(), "{}", definition.id);
            assert!(!definition.formula.is_empty() && !definition.warmup_rule.is_empty());
            let names: std::collections::BTreeSet<_> =
                definition.parameters.iter().map(|spec| spec.name).collect();
            assert_eq!(
                names.len(),
                definition.parameters.len(),
                "{}",
                definition.id
            );
            assert!(!definition
                .description
                .to_ascii_lowercase()
                .contains("volume"));
        }
        let serialized = serde_json::to_value(definitions).unwrap();
        assert_eq!(serialized[0]["parameters"][0]["kind"], "INTEGER");
        assert_eq!(serialized[0]["parameters"][0]["max"], MAX_PERIOD);
        assert!(serialized[0].get("kind").is_none());
    }

    #[test]
    fn gate_indicator_registry_fills_defaults_into_a_stable_normalized_form() {
        let defaulted = resolve_indicator("SMA", 1, &BTreeMap::new()).unwrap();
        let explicit = resolve_indicator(
            "SMA",
            1,
            &params(&[
                ("period", ParameterValue::Integer(20)),
                ("price", ParameterValue::Choice("CLOSE".into())),
            ]),
        )
        .unwrap();
        assert_eq!(defaulted.parameters, explicit.parameters);
        assert_eq!(
            explicit.config,
            IndicatorConfig::Sma {
                period: 20,
                price: PriceField::Close
            }
        );
        let integer_k = resolve_indicator(
            "BOLLINGER",
            1,
            &params(&[("k", ParameterValue::Integer(2))]),
        )
        .unwrap();
        assert_eq!(integer_k.parameters["k"], ParameterValue::Number(2.0));
    }

    #[test]
    fn gate_indicator_registry_rejects_unknown_versions_parameters_and_out_of_range_values() {
        assert!(matches!(
            resolve_indicator("MACD", 1, &BTreeMap::new()),
            Err(FeatureError::UnknownIndicator(_))
        ));
        assert!(matches!(
            resolve_indicator("SMA", 2, &BTreeMap::new()),
            Err(FeatureError::UnsupportedIndicatorVersion { .. })
        ));
        for (id, values) in [
            ("SMA", params(&[("lookback", ParameterValue::Integer(3))])),
            ("SMA", params(&[("period", ParameterValue::Integer(0))])),
            (
                "SMA",
                params(&[("period", ParameterValue::Integer(MAX_PERIOD + 1))]),
            ),
            ("SMA", params(&[("period", ParameterValue::Number(5.5))])),
            (
                "SMA",
                params(&[("period", ParameterValue::Choice("5".into()))]),
            ),
            ("EMA", params(&[("period", ParameterValue::Integer(1))])),
            (
                "SMA",
                params(&[("price", ParameterValue::Choice("VOLUME".into()))]),
            ),
            ("SMA", params(&[("price", ParameterValue::Integer(1))])),
            ("BOLLINGER", params(&[("k", ParameterValue::Number(0.0))])),
            (
                "BOLLINGER",
                params(&[("k", ParameterValue::Number(f64::NAN))]),
            ),
            (
                "BOLLINGER",
                params(&[("k", ParameterValue::Number(f64::INFINITY))]),
            ),
            (
                "BOLLINGER",
                params(&[("k", ParameterValue::Choice("2".into()))]),
            ),
        ] {
            assert!(
                matches!(
                    resolve_indicator(id, 1, &values),
                    Err(FeatureError::InvalidParameter { .. })
                ),
                "{id} {values:?}"
            );
        }
    }

    #[test]
    fn gate_indicator_registry_declares_the_warmup_each_indicator_implements() {
        for definition in indicator_definitions() {
            let resolved = resolve_indicator(definition.id, 1, &BTreeMap::new()).unwrap();
            let expected = match definition.id {
                "RSI_WILDER" | "ROC" | "LOG_RETURN" => "First n rows are WARMUP",
                _ => "First n-1 rows are WARMUP",
            };
            assert!(
                definition.warmup_rule.starts_with(expected),
                "{}",
                definition.id
            );
            let period = match resolved.parameters["period"] {
                ParameterValue::Integer(value) => value as u64,
                _ => unreachable!(),
            };
            let warmup = if expected.contains("n-1") {
                period - 1
            } else {
                period
            };
            assert_eq!(resolved.config.warmup_bars(), warmup, "{}", definition.id);
        }
    }

    #[test]
    fn parameter_values_deserialize_by_json_type() {
        let values: BTreeMap<String, ParameterValue> =
            serde_json::from_str(r#"{"period":14,"k":2.5,"price":"HIGH"}"#).unwrap();
        assert_eq!(values["period"], ParameterValue::Integer(14));
        assert_eq!(values["k"], ParameterValue::Number(2.5));
        assert_eq!(values["price"], ParameterValue::Choice("HIGH".into()));
    }
}
