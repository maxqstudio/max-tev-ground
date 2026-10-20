//! Synthetic, labeled fixtures for Strategy Engine integration tests.
//!
//! Bars come from closed-form price paths, are imported through the real Data Ground importer,
//! and become feature sets through the real Feature Studio store, so every decision in these
//! tests is evaluated over registered, hash-verified research inputs. None of it is market data.

#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    fs::File,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc},
};

use arrow::array::{Array, Float64Array, StringArray, TimestampMicrosecondArray};
use chrono::{DateTime, Duration, Utc};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use rg_data_ground::{
    store::DatasetStore, CalendarPolicy, CsvDelimiter, FieldMapping, ImportRequest, MarketRole,
    ResourceQuotas, Timeframe, TimestampSemantics,
};
use rg_feature_studio::{
    store::FeatureStore, FeatureProgress, FeatureQuotas, FeatureSetRequest, FeatureSource,
    FeatureSpec, ParameterValue, SeriesSelector,
};
use rg_strategy_engine::{
    store::{DecisionCommitResult, PreparedDecisionSet, StrategyStore},
    DecisionProgress, DecisionQuotas, DecisionSetRequest, FeatureBinding, StrategyError,
};
use tempfile::TempDir;

pub const FIXTURE_SOURCE_PREFIX: &str = "synthetic-fixture:";

#[derive(Clone, Debug)]
pub struct Bar {
    pub symbol: String,
    pub timeframe: Timeframe,
    pub open_utc: DateTime<Utc>,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

pub fn utc(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .unwrap()
        .with_timezone(&Utc)
}

/// Deterministic, continuous bar series with a slow oscillation, so threshold rules see both
/// directions and quiet stretches.
pub fn bars(symbol: &str, timeframe: Timeframe, start: &str, count: usize, phase: f64) -> Vec<Bar> {
    let step = Duration::microseconds(timeframe.duration_micros());
    let mut time = utc(start);
    let mut previous = 1.1 + 0.01 * phase.sin();
    (0..count)
        .map(|index| {
            let x = index as f64;
            let close = 1.1 + 0.012 * (x * 0.31 + phase).sin() + 0.003 * (x * 0.083).cos();
            let bar = Bar {
                symbol: symbol.into(),
                timeframe,
                open_utc: time,
                open: previous,
                high: previous.max(close) + 0.0004,
                low: previous.min(close) - 0.0004,
                close,
            };
            previous = close;
            time += step;
            bar
        })
        .collect()
}

/// Multiplies prices from `from_index` onward, a change visible only in the future of that bar.
pub fn perturb_from(rows: &mut [Bar], from_index: usize, factor: f64) {
    for bar in &mut rows[from_index..] {
        bar.open *= factor;
        bar.high *= factor;
        bar.low *= factor;
        bar.close *= factor;
    }
}

fn csv(rows: &[Bar]) -> String {
    let mut text = String::from("timestamp,symbol,timeframe,open,high,low,close,volume\n");
    for bar in rows {
        text.push_str(&format!(
            "{},{},{},{},{},{},{},1\n",
            bar.open_utc
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            bar.symbol,
            bar.timeframe.as_str(),
            bar.open,
            bar.high,
            bar.low,
            bar.close
        ));
    }
    text
}

pub struct Fixture {
    pub directory: TempDir,
    pub root: PathBuf,
    pub datasets: Arc<DatasetStore>,
    pub features: Arc<FeatureStore>,
    pub strategies: Option<StrategyStore>,
}

impl Fixture {
    pub fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("app-data");
        let datasets = Arc::new(DatasetStore::open(&root).unwrap());
        let features = Arc::new(FeatureStore::open(&root, Arc::clone(&datasets)).unwrap());
        let strategies = StrategyStore::open(&root, Arc::clone(&features)).unwrap();
        Self {
            directory,
            root,
            datasets,
            features,
            strategies: Some(strategies),
        }
    }

    pub fn store(&self) -> &StrategyStore {
        self.strategies.as_ref().unwrap()
    }

    /// Simulates an application restart of the decision registry.
    pub fn reopen(&mut self) -> Result<(), StrategyError> {
        self.strategies = None;
        self.strategies = Some(StrategyStore::open(&self.root, Arc::clone(&self.features))?);
        Ok(())
    }

    pub fn import(&self, name: &str, rows: &[Bar]) -> String {
        let source = self.directory.path().join(format!("{name}.csv"));
        std::fs::write(&source, csv(rows)).unwrap();
        let request = ImportRequest {
            mapping: FieldMapping {
                timestamp: "timestamp".into(),
                open: "open".into(),
                high: "high".into(),
                low: "low".into(),
                close: "close".into(),
                volume: "volume".into(),
                symbol: Some("symbol".into()),
                timeframe: Some("timeframe".into()),
            },
            timestamp_semantics: TimestampSemantics::BarOpensAt,
            naive_timestamp_offset_minutes: 0,
            default_symbol: rows[0].symbol.clone(),
            default_timeframe: rows[0].timeframe.as_str().into(),
            default_market_role: MarketRole::Target,
            market_roles: BTreeMap::new(),
            source_id: format!("{FIXTURE_SOURCE_PREFIX}{name}"),
            broker: "SYNTHETIC-FIXTURE".into(),
            server: "SYNTHETIC-FIXTURE".into(),
            delimiter: CsvDelimiter::Comma,
            calendar: CalendarPolicy::Continuous,
            quotas: ResourceQuotas {
                max_source_bytes: Some(64 * 1024 * 1024),
                max_records: Some(1_000_000),
            },
        };
        let prepared = self
            .datasets
            .prepare_import(&source, request, Arc::new(AtomicBool::new(false)))
            .unwrap();
        self.datasets.commit_prepared(prepared).unwrap().dataset_id
    }

    /// Materializes and registers a feature set; returns its ID.
    pub fn feature_set(&self, request: &FeatureSetRequest) -> String {
        let prepared = self
            .features
            .prepare(
                request,
                &AtomicBool::new(false),
                &FeatureProgress::default(),
            )
            .unwrap();
        self.features
            .commit_prepared(prepared)
            .unwrap()
            .feature_set_id
    }

    pub fn prepare(
        &self,
        request: &DecisionSetRequest,
    ) -> Result<PreparedDecisionSet, StrategyError> {
        self.store().prepare(
            request,
            &AtomicBool::new(false),
            &DecisionProgress::default(),
        )
    }

    pub fn decide(&self, request: &DecisionSetRequest) -> DecisionCommitResult {
        let prepared = self.prepare(request).unwrap();
        self.store().commit_prepared(prepared).unwrap()
    }

    pub fn dataset_bars(&self, dataset_id: &str) -> PathBuf {
        self.root
            .join("datasets")
            .join(dataset_id)
            .join("bars.parquet")
    }

    pub fn feature_output(&self, feature_set_id: &str) -> PathBuf {
        self.root
            .join("feature-sets")
            .join(feature_set_id)
            .join("features.parquet")
    }

    pub fn decision_dir(&self, decision_set_id: &str) -> PathBuf {
        self.root.join("decision-sets").join(decision_set_id)
    }

    pub fn staging_entries(&self) -> usize {
        std::fs::read_dir(self.root.join(".decision-staging"))
            .unwrap()
            .count()
    }
}

pub fn selector(dataset_id: &str, symbol: &str, timeframe: &str) -> SeriesSelector {
    SeriesSelector {
        dataset_id: dataset_id.into(),
        symbol: symbol.into(),
        timeframe: timeframe.into(),
    }
}

pub fn period(value: i64) -> BTreeMap<String, ParameterValue> {
    BTreeMap::from([("period".to_owned(), ParameterValue::Integer(value))])
}

pub fn target_feature(key: &str, indicator: &str, period_value: i64) -> FeatureSpec {
    FeatureSpec {
        key: key.into(),
        indicator_id: indicator.into(),
        indicator_version: 1,
        parameters: period(period_value),
        source: FeatureSource::Target,
    }
}

pub fn reference_feature(
    key: &str,
    indicator: &str,
    period_value: i64,
    series: SeriesSelector,
    max_staleness_seconds: u64,
) -> FeatureSpec {
    FeatureSpec {
        key: key.into(),
        indicator_id: indicator.into(),
        indicator_version: 1,
        parameters: period(period_value),
        source: FeatureSource::Reference {
            series,
            max_staleness_seconds,
            availability_lag_seconds: 0,
        },
    }
}

pub fn feature_request(target: SeriesSelector, features: Vec<FeatureSpec>) -> FeatureSetRequest {
    FeatureSetRequest {
        target,
        features,
        quotas: FeatureQuotas {
            max_target_rows: 1_000_000,
            max_input_rows: 2_000_000,
            max_output_bytes: 256 * 1024 * 1024,
        },
    }
}

pub fn binding(feature_key: &str) -> FeatureBinding {
    FeatureBinding {
        feature_key: feature_key.into(),
        output: "value".into(),
    }
}

pub fn decision_request(
    feature_set_id: &str,
    strategy_id: &str,
    parameters: &[(&str, f64)],
    bindings: &[(&str, &str)],
) -> DecisionSetRequest {
    DecisionSetRequest {
        feature_set_id: feature_set_id.into(),
        strategy_id: strategy_id.into(),
        strategy_version: 1,
        parameters: parameters
            .iter()
            .map(|(name, value)| ((*name).to_owned(), *value))
            .collect(),
        bindings: bindings
            .iter()
            .map(|(role, key)| ((*role).to_owned(), binding(key)))
            .collect(),
        quotas: DecisionQuotas {
            max_rows: 1_000_000,
            max_output_bytes: 256 * 1024 * 1024,
        },
    }
}

/// A column read back from a Parquet file, for value-level assertions.
#[derive(Clone, Debug, PartialEq)]
pub enum Column {
    Times(Vec<Option<i64>>),
    Floats(Vec<Option<f64>>),
    Texts(Vec<Option<String>>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    pub rows: usize,
    pub order: Vec<String>,
    pub columns: BTreeMap<String, Column>,
}

impl Table {
    pub fn times(&self, name: &str) -> Vec<i64> {
        match &self.columns[name] {
            Column::Times(values) => values.iter().map(|value| value.unwrap()).collect(),
            other => panic!("{name} is not a time column: {other:?}"),
        }
    }

    pub fn floats(&self, name: &str) -> Vec<Option<f64>> {
        match &self.columns[name] {
            Column::Floats(values) => values.clone(),
            other => panic!("{name} is not a float column: {other:?}"),
        }
    }

    pub fn texts(&self, name: &str) -> Vec<Option<String>> {
        match &self.columns[name] {
            Column::Texts(values) => values.clone(),
            other => panic!("{name} is not a text column: {other:?}"),
        }
    }

    pub fn text(&self, name: &str) -> Vec<String> {
        self.texts(name)
            .into_iter()
            .map(|value| value.unwrap())
            .collect()
    }

    /// Every column of one row, for whole-row comparisons.
    pub fn row(&self, index: usize) -> Vec<String> {
        self.order
            .iter()
            .map(|name| match &self.columns[name] {
                Column::Times(values) => format!("{:?}", values[index]),
                Column::Floats(values) => format!("{:?}", values[index].map(f64::to_bits)),
                Column::Texts(values) => format!("{:?}", values[index]),
            })
            .collect()
    }
}

pub fn read_table(path: &Path) -> Table {
    let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(path).unwrap()).unwrap();
    let schema = builder.schema().clone();
    let order: Vec<String> = schema.fields().iter().map(|f| f.name().clone()).collect();
    let mut columns: BTreeMap<String, Column> = BTreeMap::new();
    let mut rows = 0;
    for batch in builder.build().unwrap() {
        let batch = batch.unwrap();
        rows += batch.num_rows();
        for (index, field) in schema.fields().iter().enumerate() {
            let array = batch.column(index);
            let entry = columns.entry(field.name().clone());
            if let Some(values) = array.as_any().downcast_ref::<TimestampMicrosecondArray>() {
                let data = (0..values.len()).map(|r| (!values.is_null(r)).then(|| values.value(r)));
                match entry.or_insert_with(|| Column::Times(Vec::new())) {
                    Column::Times(existing) => existing.extend(data),
                    _ => unreachable!(),
                }
            } else if let Some(values) = array.as_any().downcast_ref::<Float64Array>() {
                let data = (0..values.len()).map(|r| (!values.is_null(r)).then(|| values.value(r)));
                match entry.or_insert_with(|| Column::Floats(Vec::new())) {
                    Column::Floats(existing) => existing.extend(data),
                    _ => unreachable!(),
                }
            } else if let Some(values) = array.as_any().downcast_ref::<StringArray>() {
                let data = (0..values.len())
                    .map(|r| (!values.is_null(r)).then(|| values.value(r).to_owned()));
                match entry.or_insert_with(|| Column::Texts(Vec::new())) {
                    Column::Texts(existing) => existing.extend(data),
                    _ => unreachable!(),
                }
            } else {
                panic!("unexpected column type for {}", field.name());
            }
        }
    }
    Table {
        rows,
        order,
        columns,
    }
}

pub fn flip_middle_byte(path: &Path) {
    let mut bytes = std::fs::read(path).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x5a;
    std::fs::write(path, bytes).unwrap();
}

pub fn micros(value: &str) -> i64 {
    utc(value).timestamp_micros()
}

pub const HOUR_US: i64 = 3_600_000_000;
