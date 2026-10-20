//! Synthetic, labeled fixture generation for Feature Studio integration tests.
//!
//! Bars come from closed-form price paths and are imported through the real Data Ground importer,
//! so feature computation always runs over registered, hash-verified datasets.

#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    fs::File,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc},
};

use arrow::array::{Array, Float64Array, StringArray, TimestampMicrosecondArray};
use chrono::{DateTime, Datelike, Duration, Utc, Weekday};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use rg_data_ground::{
    store::DatasetStore, CalendarPolicy, CsvDelimiter, FieldMapping, ImportRequest, MarketRole,
    ResourceQuotas, Timeframe, TimestampSemantics,
};
use rg_feature_studio::{
    store::FeatureStore, FeatureProgress, FeatureQuotas, FeatureSetRequest, FeatureSource,
    FeatureSpec, ParameterValue, SeriesSelector,
};
use tempfile::TempDir;

pub const FIXTURE_SOURCE_PREFIX: &str = "synthetic-fixture:";

#[derive(Clone, Debug)]
pub struct SyntheticBar {
    pub symbol: String,
    pub timeframe: Timeframe,
    pub open_utc: DateTime<Utc>,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

pub fn utc(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .unwrap()
        .with_timezone(&Utc)
}

/// Deterministic bar series. `skip_weekends` omits Saturday and Sunday opens, as a market with a
/// weekend closure would.
pub fn series(
    symbol: &str,
    timeframe: Timeframe,
    start: &str,
    count: usize,
    phase: f64,
    skip_weekends: bool,
) -> Vec<SyntheticBar> {
    let step = Duration::microseconds(timeframe.duration_micros());
    let mut time = utc(start);
    let mut previous_close = 1.1 + 0.01 * phase.sin();
    let mut bars = Vec::with_capacity(count);
    let mut index = 0usize;
    while bars.len() < count {
        if skip_weekends && matches!(time.weekday(), Weekday::Sat | Weekday::Sun) {
            time += step;
            continue;
        }
        let x = index as f64;
        let close = 1.1 + 0.01 * (x * 0.37 + phase).sin() + 0.004 * (x * 0.071).cos();
        let open = previous_close;
        bars.push(SyntheticBar {
            symbol: symbol.into(),
            timeframe,
            open_utc: time,
            open,
            high: open.max(close) + 0.0003 + 0.0001 * (x * 0.9).sin().abs(),
            low: open.min(close) - 0.0003 - 0.0001 * (x * 1.3).cos().abs(),
            close,
            volume: (index % 7) as f64,
        });
        previous_close = close;
        time += step;
        index += 1;
    }
    bars
}

pub fn scale_prices(bars: &mut [SyntheticBar], factor: f64) {
    for bar in bars {
        bar.open *= factor;
        bar.high *= factor;
        bar.low *= factor;
        bar.close *= factor;
    }
}

fn csv(rows: &[SyntheticBar]) -> String {
    let mut text = String::from("timestamp,symbol,timeframe,open,high,low,close,volume\n");
    for bar in rows {
        text.push_str(&format!(
            "{},{},{},{},{},{},{},{}\n",
            bar.open_utc
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            bar.symbol,
            bar.timeframe.as_str(),
            bar.open,
            bar.high,
            bar.low,
            bar.close,
            bar.volume
        ));
    }
    text
}

pub struct Fixture {
    pub directory: TempDir,
    pub root: PathBuf,
    pub datasets: Arc<DatasetStore>,
    pub features: Option<FeatureStore>,
}

impl Fixture {
    pub fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("app-data");
        let datasets = Arc::new(DatasetStore::open(&root).unwrap());
        let features = FeatureStore::open(&root, Arc::clone(&datasets)).unwrap();
        Self {
            directory,
            root,
            datasets,
            features: Some(features),
        }
    }

    pub fn store(&self) -> &FeatureStore {
        self.features.as_ref().unwrap()
    }

    /// Simulates an application restart of the feature registry.
    pub fn reopen(&mut self) -> Result<(), rg_feature_studio::FeatureError> {
        self.features = None;
        self.features = Some(FeatureStore::open(&self.root, Arc::clone(&self.datasets))?);
        Ok(())
    }

    pub fn import(&self, name: &str, rows: &[SyntheticBar], calendar: CalendarPolicy) -> String {
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
            calendar,
            quotas: ResourceQuotas {
                max_source_bytes: Some(512 * 1024 * 1024),
                max_records: Some(2_000_000),
            },
        };
        let prepared = self
            .datasets
            .prepare_import(&source, request, Arc::new(AtomicBool::new(false)))
            .unwrap();
        self.datasets.commit_prepared(prepared).unwrap().dataset_id
    }

    pub fn prepare(
        &self,
        request: &FeatureSetRequest,
    ) -> Result<rg_feature_studio::store::PreparedFeatureSet, rg_feature_studio::FeatureError> {
        self.store().prepare(
            request,
            &AtomicBool::new(false),
            &FeatureProgress::default(),
        )
    }

    pub fn dataset_dir(&self, dataset_id: &str) -> PathBuf {
        self.root.join("datasets").join(dataset_id)
    }

    pub fn staging_entries(&self) -> usize {
        std::fs::read_dir(self.root.join(".feature-staging"))
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

pub fn parameters(values: &[(&str, ParameterValue)]) -> BTreeMap<String, ParameterValue> {
    values
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect()
}

pub fn period(value: i64) -> BTreeMap<String, ParameterValue> {
    parameters(&[("period", ParameterValue::Integer(value))])
}

pub fn target_feature(
    key: &str,
    indicator: &str,
    parameters: BTreeMap<String, ParameterValue>,
) -> FeatureSpec {
    FeatureSpec {
        key: key.into(),
        indicator_id: indicator.into(),
        indicator_version: 1,
        parameters,
        source: FeatureSource::Target,
    }
}

pub fn reference_feature(
    key: &str,
    indicator: &str,
    parameters: BTreeMap<String, ParameterValue>,
    series: SeriesSelector,
    max_staleness_seconds: u64,
    availability_lag_seconds: u64,
) -> FeatureSpec {
    FeatureSpec {
        key: key.into(),
        indicator_id: indicator.into(),
        indicator_version: 1,
        parameters,
        source: FeatureSource::Reference {
            series,
            max_staleness_seconds,
            availability_lag_seconds,
        },
    }
}

pub fn request(target: SeriesSelector, features: Vec<FeatureSpec>) -> FeatureSetRequest {
    FeatureSetRequest {
        target,
        features,
        quotas: FeatureQuotas {
            max_target_rows: 1_000_000,
            max_input_rows: 2_000_000,
            max_output_bytes: 512 * 1024 * 1024,
        },
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ColumnData {
    Times(Vec<Option<i64>>),
    Floats(Vec<Option<f64>>),
    Strings(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutputTable {
    pub rows: usize,
    pub columns: BTreeMap<String, ColumnData>,
    pub column_order: Vec<String>,
    pub row_group_rows: Vec<i64>,
}

impl OutputTable {
    pub fn times(&self, name: &str) -> Vec<Option<i64>> {
        match &self.columns[name] {
            ColumnData::Times(values) => values.clone(),
            other => panic!("{name} is not a timestamp column: {other:?}"),
        }
    }

    pub fn floats(&self, name: &str) -> Vec<Option<f64>> {
        match &self.columns[name] {
            ColumnData::Floats(values) => values.clone(),
            other => panic!("{name} is not a float column: {other:?}"),
        }
    }

    pub fn strings(&self, name: &str) -> Vec<String> {
        match &self.columns[name] {
            ColumnData::Strings(values) => values.clone(),
            other => panic!("{name} is not a text column: {other:?}"),
        }
    }

    /// Values of every column for one row, for whole-row comparisons.
    pub fn row(&self, index: usize) -> Vec<String> {
        self.column_order
            .iter()
            .map(|name| match &self.columns[name] {
                ColumnData::Times(values) => format!("{:?}", values[index]),
                ColumnData::Floats(values) => format!("{:?}", values[index].map(f64::to_bits)),
                ColumnData::Strings(values) => values[index].clone(),
            })
            .collect()
    }
}

pub fn read_output(path: &Path) -> OutputTable {
    let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(path).unwrap()).unwrap();
    let row_group_rows = builder
        .metadata()
        .row_groups()
        .iter()
        .map(|group| group.num_rows())
        .collect();
    let schema = builder.schema().clone();
    let column_order: Vec<String> = schema.fields().iter().map(|f| f.name().clone()).collect();
    let mut columns: BTreeMap<String, ColumnData> = BTreeMap::new();
    let mut rows = 0;
    for batch in builder.build().unwrap() {
        let batch = batch.unwrap();
        rows += batch.num_rows();
        for (index, field) in schema.fields().iter().enumerate() {
            let array = batch.column(index);
            let entry = columns.entry(field.name().clone());
            if let Some(values) = array.as_any().downcast_ref::<TimestampMicrosecondArray>() {
                let data =
                    (0..values.len()).map(|row| (!values.is_null(row)).then(|| values.value(row)));
                match entry.or_insert_with(|| ColumnData::Times(Vec::new())) {
                    ColumnData::Times(existing) => existing.extend(data),
                    _ => unreachable!(),
                }
            } else if let Some(values) = array.as_any().downcast_ref::<Float64Array>() {
                let data =
                    (0..values.len()).map(|row| (!values.is_null(row)).then(|| values.value(row)));
                match entry.or_insert_with(|| ColumnData::Floats(Vec::new())) {
                    ColumnData::Floats(existing) => existing.extend(data),
                    _ => unreachable!(),
                }
            } else if let Some(values) = array.as_any().downcast_ref::<StringArray>() {
                let data = (0..values.len()).map(|row| values.value(row).to_owned());
                match entry.or_insert_with(|| ColumnData::Strings(Vec::new())) {
                    ColumnData::Strings(existing) => existing.extend(data),
                    _ => unreachable!(),
                }
            } else {
                panic!("unexpected output column type for {}", field.name());
            }
        }
    }
    OutputTable {
        rows,
        columns,
        column_order,
        row_group_rows,
    }
}

pub fn file_sha256(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(std::fs::read(path).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn micros(value: &str) -> i64 {
    utc(value).timestamp_micros()
}

pub const HOUR_US: i64 = 3_600_000_000;
