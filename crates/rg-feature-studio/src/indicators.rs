//! Causal, streaming indicator state machines.
//!
//! Each state consumes one closed bar at a time in chronological order and can only observe bars
//! it has already been given. Windows are recomputed from their stored observations, oldest to
//! newest, so results do not drift with series length and match a direct evaluation bit for bit.

use std::collections::VecDeque;

use thiserror::Error;

use crate::registry::{IndicatorConfig, PriceField};

/// The price fields of one closed bar. Volume is intentionally not part of the indicator input.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PriceBar {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

impl PriceBar {
    fn price(&self, field: PriceField) -> f64 {
        match field {
            PriceField::Open => self.open,
            PriceField::High => self.high,
            PriceField::Low => self.low,
            PriceField::Close => self.close,
            PriceField::Typical => (self.high + self.low + self.close) / 3.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum IndicatorError {
    #[error("indicator produced a non-finite value")]
    NonFinite,
    #[error("indicator denominator was zero or non-positive")]
    InvalidDenominator,
}

#[derive(Clone, Debug)]
struct Window {
    values: VecDeque<f64>,
    capacity: usize,
}

impl Window {
    fn new(capacity: usize) -> Self {
        Self {
            values: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, value: f64) {
        if self.values.len() == self.capacity {
            self.values.pop_front();
        }
        self.values.push_back(value);
    }

    fn is_full(&self) -> bool {
        self.values.len() == self.capacity
    }

    fn mean(&self) -> f64 {
        self.values.iter().fold(0.0, |sum, value| sum + value) / self.values.len() as f64
    }

    fn population_stddev(&self, mean: f64) -> f64 {
        let squares = self
            .values
            .iter()
            .fold(0.0, |sum, value| sum + (value - mean) * (value - mean));
        (squares / self.values.len() as f64).sqrt()
    }

    fn front(&self) -> f64 {
        self.values[0]
    }

    fn max(&self) -> f64 {
        self.values
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max)
    }

    fn min(&self) -> f64 {
        self.values.iter().copied().fold(f64::INFINITY, f64::min)
    }
}

/// Wilder smoothing seeded by the arithmetic mean of the first `period` inputs.
#[derive(Clone, Debug)]
struct WilderAverage {
    period: usize,
    count: usize,
    seed_sum: f64,
    value: f64,
}

impl WilderAverage {
    fn new(period: usize) -> Self {
        Self {
            period,
            count: 0,
            seed_sum: 0.0,
            value: 0.0,
        }
    }

    fn push(&mut self, input: f64) -> Option<f64> {
        self.count += 1;
        let n = self.period as f64;
        if self.count < self.period {
            self.seed_sum += input;
            None
        } else if self.count == self.period {
            self.seed_sum += input;
            self.value = self.seed_sum / n;
            Some(self.value)
        } else {
            self.value = (self.value * (n - 1.0) + input) / n;
            Some(self.value)
        }
    }
}

#[derive(Clone, Debug)]
enum State {
    Sma {
        price: PriceField,
        window: Window,
    },
    Ema {
        price: PriceField,
        period: usize,
        count: usize,
        seed_sum: f64,
        value: f64,
    },
    Rsi {
        price: PriceField,
        previous: Option<f64>,
        gains: WilderAverage,
        losses: WilderAverage,
    },
    Atr {
        previous_close: Option<f64>,
        average: WilderAverage,
    },
    StdDev {
        price: PriceField,
        window: Window,
    },
    Bollinger {
        price: PriceField,
        k: f64,
        window: Window,
    },
    Roc {
        price: PriceField,
        window: Window,
    },
    LogReturn {
        price: PriceField,
        window: Window,
    },
    Donchian {
        highs: Window,
        lows: Window,
    },
}

/// Mutable state of one configured indicator over one series.
#[derive(Clone, Debug)]
pub struct IndicatorState {
    state: State,
    outputs: usize,
}

impl IndicatorState {
    pub fn new(config: IndicatorConfig) -> Self {
        let (state, outputs) = match config {
            IndicatorConfig::Sma { period, price } => (
                State::Sma {
                    price,
                    window: Window::new(period),
                },
                1,
            ),
            IndicatorConfig::Ema { period, price } => (
                State::Ema {
                    price,
                    period,
                    count: 0,
                    seed_sum: 0.0,
                    value: 0.0,
                },
                1,
            ),
            IndicatorConfig::RsiWilder { period, price } => (
                State::Rsi {
                    price,
                    previous: None,
                    gains: WilderAverage::new(period),
                    losses: WilderAverage::new(period),
                },
                1,
            ),
            IndicatorConfig::AtrWilder { period } => (
                State::Atr {
                    previous_close: None,
                    average: WilderAverage::new(period),
                },
                1,
            ),
            IndicatorConfig::StdDev { period, price } => (
                State::StdDev {
                    price,
                    window: Window::new(period),
                },
                1,
            ),
            IndicatorConfig::Bollinger { period, k, price } => (
                State::Bollinger {
                    price,
                    k,
                    window: Window::new(period),
                },
                3,
            ),
            IndicatorConfig::Roc { period, price } => (
                State::Roc {
                    price,
                    window: Window::new(period + 1),
                },
                1,
            ),
            IndicatorConfig::LogReturn { period, price } => (
                State::LogReturn {
                    price,
                    window: Window::new(period + 1),
                },
                1,
            ),
            IndicatorConfig::Donchian { period } => (
                State::Donchian {
                    highs: Window::new(period),
                    lows: Window::new(period),
                },
                3,
            ),
        };
        Self { state, outputs }
    }

    pub fn output_count(&self) -> usize {
        self.outputs
    }

    /// Consumes the next closed bar. Returns `Ok(true)` and fills `out` when the indicator is
    /// ready, `Ok(false)` during warmup. Any non-finite result fails closed.
    pub fn update(&mut self, bar: &PriceBar, out: &mut [f64]) -> Result<bool, IndicatorError> {
        debug_assert_eq!(out.len(), self.outputs);
        let ready = match &mut self.state {
            State::Sma { price, window } => {
                window.push(bar.price(*price));
                if window.is_full() {
                    out[0] = window.mean();
                }
                window.is_full()
            }
            State::Ema {
                price,
                period,
                count,
                seed_sum,
                value,
            } => {
                let input = bar.price(*price);
                *count += 1;
                if *count < *period {
                    *seed_sum += input;
                    false
                } else {
                    if *count == *period {
                        *seed_sum += input;
                        *value = *seed_sum / *period as f64;
                    } else {
                        let alpha = 2.0 / (*period as f64 + 1.0);
                        *value = alpha * input + (1.0 - alpha) * *value;
                    }
                    out[0] = *value;
                    true
                }
            }
            State::Rsi {
                price,
                previous,
                gains,
                losses,
            } => {
                let input = bar.price(*price);
                match previous.replace(input) {
                    None => false,
                    Some(last) => {
                        let change = input - last;
                        let gain = gains.push(change.max(0.0));
                        let loss = losses.push((-change).max(0.0));
                        match (gain, loss) {
                            (Some(gain), Some(loss)) => {
                                out[0] = if loss == 0.0 {
                                    if gain == 0.0 {
                                        50.0
                                    } else {
                                        100.0
                                    }
                                } else {
                                    100.0 - 100.0 / (1.0 + gain / loss)
                                };
                                true
                            }
                            _ => false,
                        }
                    }
                }
            }
            State::Atr {
                previous_close,
                average,
            } => {
                let range = bar.high - bar.low;
                let true_range = match previous_close.replace(bar.close) {
                    None => range,
                    Some(close) => range
                        .max((bar.high - close).abs())
                        .max((bar.low - close).abs()),
                };
                match average.push(true_range) {
                    Some(value) => {
                        out[0] = value;
                        true
                    }
                    None => false,
                }
            }
            State::StdDev { price, window } => {
                window.push(bar.price(*price));
                if window.is_full() {
                    out[0] = window.population_stddev(window.mean());
                }
                window.is_full()
            }
            State::Bollinger { price, k, window } => {
                window.push(bar.price(*price));
                if window.is_full() {
                    let middle = window.mean();
                    let width = *k * window.population_stddev(middle);
                    out[0] = middle + width;
                    out[1] = middle;
                    out[2] = middle - width;
                }
                window.is_full()
            }
            State::Roc { price, window } => {
                window.push(bar.price(*price));
                if window.is_full() {
                    let base = window.front();
                    if base == 0.0 {
                        return Err(IndicatorError::InvalidDenominator);
                    }
                    out[0] = 100.0 * (bar.price(*price) - base) / base;
                }
                window.is_full()
            }
            State::LogReturn { price, window } => {
                let input = bar.price(*price);
                window.push(input);
                if window.is_full() {
                    let base = window.front();
                    if base <= 0.0 || input <= 0.0 {
                        return Err(IndicatorError::InvalidDenominator);
                    }
                    out[0] = (input / base).ln();
                }
                window.is_full()
            }
            State::Donchian { highs, lows } => {
                highs.push(bar.high);
                lows.push(bar.low);
                if highs.is_full() {
                    let upper = highs.max();
                    let lower = lows.min();
                    out[0] = upper;
                    out[1] = (upper + lower) / 2.0;
                    out[2] = lower;
                }
                highs.is_full()
            }
        };
        if ready && out.iter().any(|value| !value.is_finite()) {
            return Err(IndicatorError::NonFinite);
        }
        Ok(ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{indicator_definitions, resolve_indicator, ParameterValue};
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    fn close_bar(close: f64) -> PriceBar {
        PriceBar {
            open: close,
            high: close,
            low: close,
            close,
        }
    }

    fn run(config: IndicatorConfig, bars: &[PriceBar]) -> Vec<Option<Vec<f64>>> {
        let mut state = IndicatorState::new(config);
        let mut out = vec![0.0; state.output_count()];
        bars.iter()
            .map(|bar| state.update(bar, &mut out).unwrap().then(|| out.clone()))
            .collect()
    }

    fn closes(values: &[f64]) -> Vec<PriceBar> {
        values.iter().copied().map(close_bar).collect()
    }

    fn single(results: &[Option<Vec<f64>>]) -> Vec<Option<f64>> {
        results
            .iter()
            .map(|value| value.as_ref().map(|outputs| outputs[0]))
            .collect()
    }

    fn assert_close(actual: f64, expected: f64) {
        let tolerance = 1e-12 * expected.abs().max(1.0);
        assert!(
            (actual - expected).abs() <= tolerance,
            "actual {actual} expected {expected}"
        );
    }

    // Hand-calculated vectors. Every expected value below was derived on paper from the formula
    // in the registry, not from this implementation.

    #[test]
    fn gate_golden_vector_math_sma_and_ema_hand_calculated() {
        let bars = closes(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        let sma = single(&run(
            IndicatorConfig::Sma {
                period: 3,
                price: PriceField::Close,
            },
            &bars,
        ));
        assert_eq!(sma, vec![None, None, Some(2.0), Some(3.0), Some(4.0)]);
        // alpha = 2 / (3 + 1) = 0.5; seed = mean(1, 2, 3) = 2; 0.5*4 + 0.5*2 = 3; 0.5*5 + 0.5*3 = 4.
        let ema = single(&run(
            IndicatorConfig::Ema {
                period: 3,
                price: PriceField::Close,
            },
            &bars,
        ));
        assert_eq!(ema, vec![None, None, Some(2.0), Some(3.0), Some(4.0)]);
        // alpha = 2 / 5 = 0.4 with seed mean(10, 12, 11, 13) = 11.5; 0.4*9 + 0.6*11.5 = 10.5.
        let ema4 = single(&run(
            IndicatorConfig::Ema {
                period: 4,
                price: PriceField::Close,
            },
            &closes(&[10.0, 12.0, 11.0, 13.0, 9.0]),
        ));
        assert_eq!(ema4[3], Some(11.5));
        assert_close(ema4[4].unwrap(), 10.5);
    }

    #[test]
    fn gate_golden_vector_math_rsi_wilder_hand_calculated() {
        // Changes: +1, +1, -1, -1. Period 2 seeds gain=(1+1)/2=1, loss=0 -> 100.
        // Next: gain=(1*1+0)/2=0.5, loss=(0*1+1)/2=0.5 -> RS=1 -> 50.
        // Next: gain=(0.5+0)/2=0.25, loss=(0.5+1)/2=0.75 -> RS=1/3 -> 100-100/(4/3)=25.
        let rsi = single(&run(
            IndicatorConfig::RsiWilder {
                period: 2,
                price: PriceField::Close,
            },
            &closes(&[1.0, 2.0, 3.0, 2.0, 1.0]),
        ));
        assert_eq!(rsi[..2], [None, None]);
        assert_eq!(rsi[2], Some(100.0));
        assert_eq!(rsi[3], Some(50.0));
        assert_close(rsi[4].unwrap(), 25.0);
        let flat = single(&run(
            IndicatorConfig::RsiWilder {
                period: 3,
                price: PriceField::Close,
            },
            &closes(&[2.0; 6]),
        ));
        assert_eq!(
            flat,
            vec![None, None, None, Some(50.0), Some(50.0), Some(50.0)]
        );
        let falling = single(&run(
            IndicatorConfig::RsiWilder {
                period: 2,
                price: PriceField::Close,
            },
            &closes(&[5.0, 4.0, 3.0]),
        ));
        assert_eq!(falling[2], Some(0.0));
    }

    #[test]
    fn gate_golden_vector_math_atr_wilder_hand_calculated() {
        let bars = [
            PriceBar {
                open: 10.0,
                high: 11.0,
                low: 9.0,
                close: 10.0,
            }, // TR = 2 (first bar: h-l)
            PriceBar {
                open: 10.0,
                high: 12.0,
                low: 10.0,
                close: 11.0,
            }, // TR = max(2, 2, 0) = 2
            PriceBar {
                open: 11.0,
                high: 11.5,
                low: 8.0,
                close: 9.0,
            }, // TR = max(3.5, 0.5, 3) = 3.5
            PriceBar {
                open: 9.0,
                high: 13.0,
                low: 12.0,
                close: 12.5,
            }, // gap: TR = max(1, 4, 3) = 4
        ];
        let atr = single(&run(IndicatorConfig::AtrWilder { period: 2 }, &bars));
        // Seed mean(2, 2) = 2; then (2*1 + 3.5)/2 = 2.75; then (2.75 + 4)/2 = 3.375.
        assert_eq!(atr, vec![None, Some(2.0), Some(2.75), Some(3.375)]);
    }

    #[test]
    fn gate_golden_vector_math_stddev_bollinger_roc_log_return_donchian_hand_calculated() {
        // Population stddev of (2, 4, 4, 4, 5, 5, 7, 9) is exactly 2 with mean 5.
        let values = closes(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]);
        let std = single(&run(
            IndicatorConfig::StdDev {
                period: 8,
                price: PriceField::Close,
            },
            &values,
        ));
        assert_eq!(std[7], Some(2.0));
        let bands = run(
            IndicatorConfig::Bollinger {
                period: 8,
                k: 1.5,
                price: PriceField::Close,
            },
            &values,
        );
        assert_eq!(bands[7], Some(vec![8.0, 5.0, 2.0]));
        let roc = single(&run(
            IndicatorConfig::Roc {
                period: 2,
                price: PriceField::Close,
            },
            &closes(&[100.0, 50.0, 110.0, 75.0]),
        ));
        assert_eq!(roc, vec![None, None, Some(10.0), Some(50.0)]);
        let log_return = single(&run(
            IndicatorConfig::LogReturn {
                period: 1,
                price: PriceField::Close,
            },
            &closes(&[1.0, std::f64::consts::E, 1.0]),
        ));
        assert_eq!(log_return[0], None);
        assert_close(log_return[1].unwrap(), 1.0);
        assert_close(log_return[2].unwrap(), -1.0);
        let channel = run(
            IndicatorConfig::Donchian { period: 2 },
            &[
                PriceBar {
                    open: 1.0,
                    high: 3.0,
                    low: 1.0,
                    close: 2.0,
                },
                PriceBar {
                    open: 2.0,
                    high: 5.0,
                    low: 2.0,
                    close: 4.0,
                },
                PriceBar {
                    open: 4.0,
                    high: 4.0,
                    low: 0.5,
                    close: 1.0,
                },
            ],
        );
        assert_eq!(channel[0], None);
        assert_eq!(channel[1], Some(vec![5.0, 3.0, 1.0]));
        assert_eq!(channel[2], Some(vec![5.0, 2.75, 0.5]));
        let typical = single(&run(
            IndicatorConfig::Sma {
                period: 1,
                price: PriceField::Typical,
            },
            &[PriceBar {
                open: 9.0,
                high: 12.0,
                low: 6.0,
                close: 9.0,
            }],
        ));
        assert_eq!(typical, vec![Some(9.0)]);
    }

    #[test]
    fn gate_golden_vector_math_flat_series_has_defined_values() {
        let flat = closes(&[1.25; 30]);
        let last = |config| run(config, &flat).last().cloned().flatten().unwrap();
        assert_eq!(
            last(IndicatorConfig::StdDev {
                period: 10,
                price: PriceField::Close
            }),
            vec![0.0]
        );
        assert_eq!(
            last(IndicatorConfig::Bollinger {
                period: 10,
                k: 2.0,
                price: PriceField::Close
            }),
            vec![1.25, 1.25, 1.25]
        );
        assert_eq!(
            last(IndicatorConfig::Roc {
                period: 5,
                price: PriceField::Close
            }),
            vec![0.0]
        );
        assert_eq!(
            last(IndicatorConfig::LogReturn {
                period: 5,
                price: PriceField::Close
            }),
            vec![0.0]
        );
        assert_eq!(last(IndicatorConfig::AtrWilder { period: 5 }), vec![0.0]);
        assert_eq!(
            last(IndicatorConfig::RsiWilder {
                period: 5,
                price: PriceField::Close
            }),
            vec![50.0]
        );
        assert_eq!(
            last(IndicatorConfig::Ema {
                period: 5,
                price: PriceField::Close
            }),
            vec![1.25]
        );
    }

    #[test]
    fn gate_golden_vector_math_non_finite_results_fail_closed() {
        let mut state = IndicatorState::new(IndicatorConfig::StdDev {
            period: 2,
            price: PriceField::Close,
        });
        let mut out = [0.0];
        assert_eq!(state.update(&close_bar(1e300), &mut out), Ok(false));
        assert_eq!(
            state.update(&close_bar(-1e300), &mut out),
            Err(IndicatorError::NonFinite)
        );
        let mut roc = IndicatorState::new(IndicatorConfig::Roc {
            period: 1,
            price: PriceField::Close,
        });
        assert_eq!(roc.update(&close_bar(0.0), &mut out), Ok(false));
        assert_eq!(
            roc.update(&close_bar(1.0), &mut out),
            Err(IndicatorError::InvalidDenominator)
        );
        let mut log_return = IndicatorState::new(IndicatorConfig::LogReturn {
            period: 1,
            price: PriceField::Close,
        });
        assert_eq!(log_return.update(&close_bar(1.0), &mut out), Ok(false));
        assert_eq!(
            log_return.update(&close_bar(-1.0), &mut out),
            Err(IndicatorError::InvalidDenominator)
        );
        let mut overflow = IndicatorState::new(IndicatorConfig::Sma {
            period: 2,
            price: PriceField::Close,
        });
        overflow.update(&close_bar(f64::MAX), &mut out).unwrap();
        assert_eq!(
            overflow.update(&close_bar(f64::MAX), &mut out),
            Err(IndicatorError::NonFinite)
        );
    }

    #[test]
    fn gate_golden_vector_math_declared_warmup_matches_first_ready_row() {
        let bars: Vec<_> = (0..80)
            .map(|index| {
                let close = 100.0 + (index as f64 * 0.7).sin();
                PriceBar {
                    open: close - 0.1,
                    high: close + 0.3,
                    low: close - 0.4,
                    close,
                }
            })
            .collect();
        for definition in indicator_definitions() {
            for period in [2, 5, 17] {
                let resolved = resolve_indicator(
                    definition.id,
                    1,
                    &BTreeMap::from([("period".into(), ParameterValue::Integer(period))]),
                )
                .unwrap();
                let results = run(resolved.config, &bars);
                let first_ready = results.iter().position(Option::is_some).unwrap() as u64;
                assert_eq!(
                    first_ready,
                    resolved.config.warmup_bars(),
                    "{}",
                    definition.id
                );
                assert!(
                    results[first_ready as usize..].iter().all(Option::is_some),
                    "{} became not-ready after warmup",
                    definition.id
                );
                assert_eq!(
                    results[0].is_none(),
                    resolved.config.warmup_bars() > 0,
                    "{}",
                    definition.id
                );
            }
        }
    }

    // Independent reference implementation: whole-slice evaluation written from the formulas,
    // sharing no code with the streaming states above.
    fn reference_values(config: IndicatorConfig, bars: &[PriceBar]) -> Vec<Option<Vec<f64>>> {
        let pick = |bar: &PriceBar, field: PriceField| match field {
            PriceField::Open => bar.open,
            PriceField::High => bar.high,
            PriceField::Low => bar.low,
            PriceField::Close => bar.close,
            PriceField::Typical => (bar.high + bar.low + bar.close) / 3.0,
        };
        let mean = |values: &[f64]| {
            let mut total = 0.0;
            for value in values {
                total += value;
            }
            total / values.len() as f64
        };
        let pstd = |values: &[f64]| {
            let m = mean(values);
            let mut total = 0.0;
            for value in values {
                total += (value - m) * (value - m);
            }
            (total / values.len() as f64).sqrt()
        };
        let wilder = |inputs: &[f64], n: usize| -> Vec<Option<f64>> {
            let mut result = vec![None; inputs.len()];
            if inputs.len() < n {
                return result;
            }
            let mut average = mean(&inputs[..n]);
            result[n - 1] = Some(average);
            for index in n..inputs.len() {
                average = (average * (n as f64 - 1.0) + inputs[index]) / n as f64;
                result[index] = Some(average);
            }
            result
        };
        let length = bars.len();
        match config {
            IndicatorConfig::Sma { period, price } => (0..length)
                .map(|t| {
                    (t + 1 >= period).then(|| {
                        let window: Vec<f64> = bars[t + 1 - period..=t]
                            .iter()
                            .map(|b| pick(b, price))
                            .collect();
                        vec![mean(&window)]
                    })
                })
                .collect(),
            IndicatorConfig::Ema { period, price } => {
                let xs: Vec<f64> = bars.iter().map(|b| pick(b, price)).collect();
                let alpha = 2.0 / (period as f64 + 1.0);
                let mut result = vec![None; length];
                let mut value = 0.0;
                for t in 0..length {
                    if t + 1 == period {
                        value = mean(&xs[..period]);
                        result[t] = Some(vec![value]);
                    } else if t + 1 > period {
                        value = alpha * xs[t] + (1.0 - alpha) * value;
                        result[t] = Some(vec![value]);
                    }
                }
                result
            }
            IndicatorConfig::RsiWilder { period, price } => {
                let xs: Vec<f64> = bars.iter().map(|b| pick(b, price)).collect();
                let gains: Vec<f64> = xs.windows(2).map(|w| (w[1] - w[0]).max(0.0)).collect();
                let losses: Vec<f64> = xs.windows(2).map(|w| (w[0] - w[1]).max(0.0)).collect();
                let average_gain = wilder(&gains, period);
                let average_loss = wilder(&losses, period);
                std::iter::once(None)
                    .chain(
                        average_gain
                            .iter()
                            .zip(&average_loss)
                            .map(|(g, l)| match (g, l) {
                                (Some(g), Some(l)) => Some(vec![if *l == 0.0 {
                                    if *g == 0.0 {
                                        50.0
                                    } else {
                                        100.0
                                    }
                                } else {
                                    100.0 - 100.0 / (1.0 + g / l)
                                }]),
                                _ => None,
                            }),
                    )
                    .collect()
            }
            IndicatorConfig::AtrWilder { period } => {
                let ranges: Vec<f64> = (0..length)
                    .map(|t| {
                        let bar = &bars[t];
                        if t == 0 {
                            bar.high - bar.low
                        } else {
                            let close = bars[t - 1].close;
                            (bar.high - bar.low)
                                .max((bar.high - close).abs())
                                .max((bar.low - close).abs())
                        }
                    })
                    .collect();
                wilder(&ranges, period)
                    .into_iter()
                    .map(|value| value.map(|v| vec![v]))
                    .collect()
            }
            IndicatorConfig::StdDev { period, price } => (0..length)
                .map(|t| {
                    (t + 1 >= period).then(|| {
                        let window: Vec<f64> = bars[t + 1 - period..=t]
                            .iter()
                            .map(|b| pick(b, price))
                            .collect();
                        vec![pstd(&window)]
                    })
                })
                .collect(),
            IndicatorConfig::Bollinger { period, k, price } => (0..length)
                .map(|t| {
                    (t + 1 >= period).then(|| {
                        let window: Vec<f64> = bars[t + 1 - period..=t]
                            .iter()
                            .map(|b| pick(b, price))
                            .collect();
                        let middle = mean(&window);
                        let width = k * pstd(&window);
                        vec![middle + width, middle, middle - width]
                    })
                })
                .collect(),
            IndicatorConfig::Roc { period, price } => (0..length)
                .map(|t| {
                    (t >= period).then(|| {
                        let base = pick(&bars[t - period], price);
                        vec![100.0 * (pick(&bars[t], price) - base) / base]
                    })
                })
                .collect(),
            IndicatorConfig::LogReturn { period, price } => (0..length)
                .map(|t| {
                    (t >= period).then(|| {
                        vec![(pick(&bars[t], price) / pick(&bars[t - period], price)).ln()]
                    })
                })
                .collect(),
            IndicatorConfig::Donchian { period } => (0..length)
                .map(|t| {
                    (t + 1 >= period).then(|| {
                        let window = &bars[t + 1 - period..=t];
                        let upper = window.iter().map(|b| b.high).fold(f64::MIN, f64::max);
                        let lower = window.iter().map(|b| b.low).fold(f64::MAX, f64::min);
                        vec![upper, (upper + lower) / 2.0, lower]
                    })
                })
                .collect(),
        }
    }

    fn arbitrary_bars() -> impl Strategy<Value = Vec<PriceBar>> {
        prop::collection::vec(
            (1.0f64..2.0, 0.0f64..0.05, 0.0f64..0.05, 0.0f64..1.0),
            1..120,
        )
        .prop_map(|rows| {
            rows.into_iter()
                .map(|(base, up, down, position)| {
                    let high = base + up;
                    let low = base - down;
                    let close = low + (high - low) * position;
                    PriceBar {
                        open: base,
                        high,
                        low,
                        close,
                    }
                })
                .collect()
        })
    }

    fn arbitrary_config() -> impl Strategy<Value = IndicatorConfig> {
        let price = prop_oneof![
            Just(PriceField::Open),
            Just(PriceField::High),
            Just(PriceField::Low),
            Just(PriceField::Close),
            Just(PriceField::Typical)
        ];
        (2usize..30, 0.1f64..4.0, price, 0u8..9).prop_map(|(period, k, price, kind)| match kind {
            0 => IndicatorConfig::Sma { period, price },
            1 => IndicatorConfig::Ema { period, price },
            2 => IndicatorConfig::RsiWilder { period, price },
            3 => IndicatorConfig::AtrWilder { period },
            4 => IndicatorConfig::StdDev { period, price },
            5 => IndicatorConfig::Bollinger { period, k, price },
            6 => IndicatorConfig::Roc { period, price },
            7 => IndicatorConfig::LogReturn { period, price },
            _ => IndicatorConfig::Donchian { period },
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn gate_golden_vector_math_streaming_matches_independent_reference(
            bars in arbitrary_bars(),
            config in arbitrary_config(),
        ) {
            let streamed = run(config, &bars);
            let expected = reference_values(config, &bars);
            prop_assert_eq!(streamed.len(), expected.len());
            for (index, (actual, wanted)) in streamed.iter().zip(&expected).enumerate() {
                match (actual, wanted) {
                    (None, None) => {}
                    (Some(actual), Some(wanted)) => {
                        for (a, w) in actual.iter().zip(wanted) {
                            let tolerance = 1e-9 * w.abs().max(1.0);
                            prop_assert!((a - w).abs() <= tolerance, "{config:?} row {index}: {a} vs {w}");
                        }
                    }
                    _ => prop_assert!(false, "{config:?} readiness differs at row {index}"),
                }
            }
        }

        #[test]
        fn gate_golden_vector_math_repeated_runs_are_bit_identical(
            bars in arbitrary_bars(),
            config in arbitrary_config(),
        ) {
            let first = run(config, &bars);
            let second = run(config, &bars);
            let bits = |rows: &[Option<Vec<f64>>]| -> Vec<Option<Vec<u64>>> {
                rows.iter().map(|row| row.as_ref().map(|values| values.iter().map(|v| v.to_bits()).collect())).collect()
            };
            prop_assert_eq!(bits(&first), bits(&second));
        }

        #[test]
        fn gate_adversarial_no_lookahead_future_bars_never_change_past_outputs(
            bars in arbitrary_bars(),
            config in arbitrary_config(),
            cut in 0usize..120,
            shock in 0.5f64..1.5,
        ) {
            let cut = cut.min(bars.len());
            let mut perturbed = bars.clone();
            for bar in &mut perturbed[cut..] {
                bar.open *= shock;
                bar.high *= shock;
                bar.low *= shock;
                bar.close *= shock;
            }
            let original = run(config, &bars);
            let changed = run(config, &perturbed);
            prop_assert_eq!(&original[..cut], &changed[..cut]);
        }
    }
}
