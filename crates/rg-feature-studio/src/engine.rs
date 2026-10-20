//! Streaming feature materialization over canonical RG-R01 bar snapshots.
//!
//! Inputs are read in bounded record batches. Each reference series has its own cursor, and the
//! target loop advances a cursor only while the cursor's next bar is already available at the
//! current decision time. Memory is bounded by the batch size, indicator windows, and a fixed
//! number of preview rows, not by series length.

use std::{
    collections::{BTreeMap, VecDeque},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc,
    },
};

use arrow::{
    array::{Array, ArrayRef, Float64Array, RecordBatch, StringArray, TimestampMicrosecondArray},
    datatypes::{DataType, Field, Schema, TimeUnit},
};
use chrono::DateTime;
use parquet::{
    arrow::{
        arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder},
        ArrowWriter,
    },
    file::properties::WriterProperties,
};
use rg_data_ground::{DatasetManifest, Timeframe};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    indicators::{IndicatorState, PriceBar},
    spec::{
        hex, ColumnKind, FeatureSource, FeatureSummary, OutputColumn, RowStatus, ValidatedRequest,
    },
    FeatureError,
};

const READ_BATCH_ROWS: usize = 8192;
const WRITE_BATCH_ROWS: usize = 8192;
const MAX_ROW_GROUP_ROWS: usize = 65_536;
pub const PREVIEW_ROWS: usize = 20;
const HASH_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProgressPhase {
    Queued,
    VerifyingInputs,
    Computing,
    Finalizing,
}

/// Lock-free progress counters shared between the worker and status queries.
#[derive(Debug, Default)]
pub struct FeatureProgress {
    phase: AtomicU8,
    processed_rows: AtomicU64,
    total_rows: AtomicU64,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub struct ProgressSnapshot {
    pub phase: ProgressPhase,
    pub processed_rows: u64,
    pub total_rows: u64,
}

impl FeatureProgress {
    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            phase: match self.phase.load(Ordering::Relaxed) {
                1 => ProgressPhase::VerifyingInputs,
                2 => ProgressPhase::Computing,
                3 => ProgressPhase::Finalizing,
                _ => ProgressPhase::Queued,
            },
            processed_rows: self.processed_rows.load(Ordering::Relaxed),
            total_rows: self.total_rows.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn set_phase(&self, phase: ProgressPhase, total_rows: u64) {
        let value = match phase {
            ProgressPhase::Queued => 0,
            ProgressPhase::VerifyingInputs => 1,
            ProgressPhase::Computing => 2,
            ProgressPhase::Finalizing => 3,
        };
        self.processed_rows.store(0, Ordering::Relaxed);
        self.total_rows.store(total_rows, Ordering::Relaxed);
        self.phase.store(value, Ordering::Relaxed);
    }

    fn add(&self, rows: u64) {
        self.processed_rows.fetch_add(rows, Ordering::Relaxed);
    }
}

pub(crate) struct ResolvedInput {
    pub manifest: DatasetManifest,
    pub bars_path: PathBuf,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct PreviewFeatureValue {
    pub key: String,
    pub status: RowStatus,
    pub values: Vec<Option<f64>>,
    pub source_available_at_utc: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct PreviewRow {
    pub open_time_utc: String,
    pub decision_time_utc: String,
    pub features: Vec<PreviewFeatureValue>,
}

pub(crate) struct Materialized {
    pub row_count: u64,
    pub output_sha256: String,
    pub output_fingerprint: String,
    pub columns: Vec<OutputColumn>,
    pub summaries: Vec<FeatureSummary>,
    pub preview_rows: Vec<PreviewRow>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CanonicalBar {
    pub open_us: i64,
    pub close_us: i64,
    pub available_us: i64,
    pub prices: PriceBar,
}

struct Columns {
    symbol: StringArray,
    timeframe: StringArray,
    open_time: TimestampMicrosecondArray,
    close_time: TimestampMicrosecondArray,
    available_at: TimestampMicrosecondArray,
    open: Float64Array,
    high: Float64Array,
    low: Float64Array,
    close: Float64Array,
    volume: Float64Array,
}

fn canonical_schema() -> Schema {
    let timestamp = || DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()));
    Schema::new(vec![
        Field::new("symbol", DataType::Utf8, false),
        Field::new("timeframe", DataType::Utf8, false),
        Field::new("open_time_utc", timestamp(), false),
        Field::new("close_time_utc", timestamp(), false),
        Field::new("available_at_utc", timestamp(), false),
        Field::new("open", DataType::Float64, false),
        Field::new("high", DataType::Float64, false),
        Field::new("low", DataType::Float64, false),
        Field::new("close", DataType::Float64, false),
        Field::new("volume", DataType::Float64, false),
    ])
}

/// Sequential reader over an RG-R01 canonical `bars.parquet` that rejects any other schema.
struct CanonicalReader {
    reader: ParquetRecordBatchReader,
    columns: Option<Columns>,
    row: usize,
    rows: usize,
}

impl CanonicalReader {
    fn open(path: &Path) -> Result<Self, String> {
        let file =
            File::open(path).map_err(|error| format!("cannot open canonical bars: {error}"))?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)
            .map_err(|error| format!("canonical bars are not readable Parquet: {error}"))?;
        let expected = canonical_schema();
        let actual = builder.schema();
        let matches = actual.fields().len() == expected.fields().len()
            && actual.fields().iter().zip(expected.fields()).all(|(a, e)| {
                a.name() == e.name()
                    && a.data_type() == e.data_type()
                    && a.is_nullable() == e.is_nullable()
            });
        if !matches {
            return Err("canonical bars schema does not match the RG-R01 contract".into());
        }
        let reader = builder
            .with_batch_size(READ_BATCH_ROWS)
            .build()
            .map_err(|error| format!("canonical bars reader failed: {error}"))?;
        Ok(Self {
            reader,
            columns: None,
            row: 0,
            rows: 0,
        })
    }

    /// Moves to the next row. Returns `false` at the end of the file.
    fn advance(&mut self) -> Result<bool, String> {
        if self.columns.is_some() && self.row + 1 < self.rows {
            self.row += 1;
            return Ok(true);
        }
        loop {
            let Some(batch) = self.reader.next() else {
                self.columns = None;
                return Ok(false);
            };
            let batch = batch.map_err(|error| format!("canonical bars batch failed: {error}"))?;
            if batch.num_rows() == 0 {
                continue;
            }
            if batch.columns().iter().any(|column| column.null_count() > 0) {
                return Err("canonical bars contain null values".into());
            }
            self.rows = batch.num_rows();
            self.row = 0;
            self.columns = Some(Columns {
                symbol: typed(&batch, 0)?,
                timeframe: typed(&batch, 1)?,
                open_time: typed(&batch, 2)?,
                close_time: typed(&batch, 3)?,
                available_at: typed(&batch, 4)?,
                open: typed(&batch, 5)?,
                high: typed(&batch, 6)?,
                low: typed(&batch, 7)?,
                close: typed(&batch, 8)?,
                volume: typed(&batch, 9)?,
            });
            return Ok(true);
        }
    }

    fn current(&self) -> &Columns {
        self.columns
            .as_ref()
            .expect("current() is only called after advance() returned true")
    }

    fn symbol(&self) -> &str {
        self.current().symbol.value(self.row)
    }

    fn timeframe(&self) -> &str {
        self.current().timeframe.value(self.row)
    }

    fn bar(&self) -> CanonicalBar {
        let columns = self.current();
        let row = self.row;
        CanonicalBar {
            open_us: columns.open_time.value(row),
            close_us: columns.close_time.value(row),
            available_us: columns.available_at.value(row),
            prices: PriceBar {
                open: columns.open.value(row),
                high: columns.high.value(row),
                low: columns.low.value(row),
                close: columns.close.value(row),
            },
        }
    }

    fn volume(&self) -> f64 {
        self.current().volume.value(self.row)
    }
}

fn typed<T: Array + Clone + 'static>(batch: &RecordBatch, index: usize) -> Result<T, String> {
    batch
        .column(index)
        .as_any()
        .downcast_ref::<T>()
        .cloned()
        .ok_or_else(|| format!("canonical bars column {index} has an unexpected type"))
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn validate_bar(timeframe: Timeframe, bar: &CanonicalBar, volume: f64) -> Result<(), String> {
    let prices = bar.prices;
    let values = [prices.open, prices.high, prices.low, prices.close];
    if bar.close_us.checked_sub(bar.open_us) != Some(timeframe.duration_micros())
        || bar.available_us < bar.close_us
        || values
            .iter()
            .any(|value| !value.is_finite() || *value <= 0.0)
        || !volume.is_finite()
        || volume < 0.0
        || prices.low > prices.open.min(prices.close)
        || prices.high < prices.open.max(prices.close)
    {
        return Err(format!(
            "bar at {} violates the canonical time or price contract",
            format_utc(bar.open_us)
        ));
    }
    Ok(())
}

/// Recomputes the RG-R01 canonical row fingerprint from the exact rows in the file and checks it,
/// the row count, the per-series summaries, ordering, and bar contracts against the manifest.
pub(crate) fn verify_canonical_input(
    input: &ResolvedInput,
    cancel: &AtomicBool,
    progress: &FeatureProgress,
) -> Result<(), FeatureError> {
    let manifest = &input.manifest;
    let integrity = |reason: String| FeatureError::InputIntegrity {
        dataset_id: manifest.dataset_id.clone(),
        reason,
    };
    let mut reader = CanonicalReader::open(&input.bars_path).map_err(integrity)?;
    let mut hasher = Sha256::new();
    let mut rows = 0u64;
    let mut series: BTreeMap<(String, String), (u64, i64, i64)> = BTreeMap::new();
    let mut previous: Option<(String, String, i64)> = None;
    while reader.advance().map_err(integrity)? {
        if rows.is_multiple_of(1024) && cancel.load(Ordering::Relaxed) {
            return Err(FeatureError::Cancelled);
        }
        let symbol = reader.symbol();
        let timeframe_text = reader.timeframe();
        let bar = reader.bar();
        let volume = reader.volume();
        let timeframe = Timeframe::parse(timeframe_text)
            .ok()
            .filter(|value| value.as_str() == timeframe_text)
            .ok_or_else(|| integrity(format!("non-canonical timeframe {timeframe_text:?}")))?;
        validate_bar(timeframe, &bar, volume).map_err(integrity)?;
        if let Some((last_symbol, last_timeframe, last_open)) = &previous {
            let order = (last_symbol.as_str(), last_timeframe.as_str(), *last_open).cmp(&(
                symbol,
                timeframe_text,
                bar.open_us,
            ));
            if order != std::cmp::Ordering::Less {
                return Err(integrity(
                    "canonical bars contain a duplicate or out-of-order bar".into(),
                ));
            }
        }
        hash_field(&mut hasher, symbol.as_bytes());
        hash_field(&mut hasher, timeframe_text.as_bytes());
        hasher.update(bar.open_us.to_be_bytes());
        hasher.update(bar.close_us.to_be_bytes());
        hasher.update(bar.available_us.to_be_bytes());
        for value in [
            bar.prices.open,
            bar.prices.high,
            bar.prices.low,
            bar.prices.close,
            volume,
        ] {
            hasher.update(value.to_bits().to_be_bytes());
        }
        let entry = series
            .entry((symbol.to_owned(), timeframe_text.to_owned()))
            .or_insert((0, bar.open_us, bar.open_us));
        entry.0 += 1;
        entry.2 = bar.open_us;
        match &mut previous {
            Some((last_symbol, last_timeframe, last_open))
                if last_symbol == symbol && last_timeframe == timeframe_text =>
            {
                *last_open = bar.open_us;
            }
            _ => previous = Some((symbol.to_owned(), timeframe_text.to_owned(), bar.open_us)),
        }
        rows += 1;
        progress.add(1);
    }
    if hex(hasher.finalize()) != manifest.canonical_bars_fingerprint {
        return Err(integrity(
            "canonical row fingerprint does not match the dataset manifest".into(),
        ));
    }
    if rows != manifest.bar_count || series.len() != manifest.series.len() {
        return Err(integrity(
            "row or series count does not match the manifest".into(),
        ));
    }
    for summary in &manifest.series {
        let observed = series.get(&(summary.symbol.clone(), summary.timeframe.clone()));
        let matches = observed.is_some_and(|(count, first, last)| {
            *count == summary.bar_count
                && format_utc(*first) == summary.first_open_utc
                && format_utc(*last) == summary.last_open_utc
        });
        if !matches {
            return Err(integrity(format!(
                "series {} {} does not match its manifest summary",
                summary.symbol, summary.timeframe
            )));
        }
    }
    Ok(())
}

pub(crate) fn sha256_file(path: &Path, cancel: &AtomicBool) -> Result<String, FeatureError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; HASH_BUFFER_BYTES];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(FeatureError::Cancelled);
        }
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex(hasher.finalize()))
}

/// Ordered bars of one series, read lazily with a one-bar lookahead buffer. The lookahead is used
/// only to decide whether the next bar is already available; it is never fed to an indicator early.
struct SeriesCursor {
    dataset_id: String,
    reader: CanonicalReader,
    symbol: String,
    timeframe: String,
    peeked: Option<CanonicalBar>,
    last_open: Option<i64>,
    finished: bool,
}

impl SeriesCursor {
    fn open(input: &ResolvedInput, symbol: &str, timeframe: &str) -> Result<Self, FeatureError> {
        let reader = CanonicalReader::open(&input.bars_path).map_err(|reason| {
            FeatureError::InputIntegrity {
                dataset_id: input.manifest.dataset_id.clone(),
                reason,
            }
        })?;
        Ok(Self {
            dataset_id: input.manifest.dataset_id.clone(),
            reader,
            symbol: symbol.to_owned(),
            timeframe: timeframe.to_owned(),
            peeked: None,
            last_open: None,
            finished: false,
        })
    }

    fn integrity(&self, reason: impl Into<String>) -> FeatureError {
        FeatureError::InputIntegrity {
            dataset_id: self.dataset_id.clone(),
            reason: reason.into(),
        }
    }

    fn peek(&mut self) -> Result<Option<CanonicalBar>, FeatureError> {
        if self.peeked.is_some() || self.finished {
            return Ok(self.peeked);
        }
        while self
            .reader
            .advance()
            .map_err(|reason| self.integrity(reason))?
        {
            let key = (self.reader.symbol(), self.reader.timeframe());
            let wanted = (self.symbol.as_str(), self.timeframe.as_str());
            if key < wanted {
                continue;
            }
            if key > wanted {
                break;
            }
            let bar = self.reader.bar();
            if self.last_open.is_some_and(|last| bar.open_us <= last) {
                return Err(self.integrity("series contains a duplicate or out-of-order bar"));
            }
            self.last_open = Some(bar.open_us);
            self.peeked = Some(bar);
            return Ok(self.peeked);
        }
        self.finished = true;
        Ok(None)
    }

    fn take(&mut self) -> Result<Option<CanonicalBar>, FeatureError> {
        self.peek()?;
        Ok(self.peeked.take())
    }
}

struct ReferenceStream {
    cursor: SeriesCursor,
    lag_us: i64,
    max_staleness_us: i64,
    latest: Option<(i64, bool)>,
}

struct FeatureEvaluator {
    key: String,
    state: IndicatorState,
    values: Vec<f64>,
    reference: Option<ReferenceStream>,
    status: RowStatus,
    source_available_us: Option<i64>,
    summary: FeatureSummary,
}

impl FeatureEvaluator {
    fn evaluate(&mut self, target: &CanonicalBar) -> Result<(), FeatureError> {
        let decision_us = target.available_us;
        match &mut self.reference {
            None => {
                let ready = self
                    .state
                    .update(&target.prices, &mut self.values)
                    .map_err(|error| FeatureEvaluator::error_for(&self.key, decision_us, error))?;
                self.status = if ready {
                    RowStatus::Ready
                } else {
                    RowStatus::Warmup
                };
            }
            Some(stream) => {
                while let Some(next) = stream.cursor.peek()? {
                    let effective =
                        next.available_us
                            .checked_add(stream.lag_us)
                            .ok_or_else(|| {
                                stream.cursor.integrity("reference availability overflowed")
                            })?;
                    if effective > decision_us {
                        break;
                    }
                    let bar = stream.cursor.take()?.expect("peeked bar is present");
                    let ready =
                        self.state
                            .update(&bar.prices, &mut self.values)
                            .map_err(|error| {
                                FeatureEvaluator::error_for(&self.key, effective, error)
                            })?;
                    stream.latest = Some((effective, ready));
                }
                (self.status, self.source_available_us) = match stream.latest {
                    None => (RowStatus::NoObservation, None),
                    Some((effective, _)) if decision_us - effective > stream.max_staleness_us => {
                        (RowStatus::Stale, Some(effective))
                    }
                    Some((effective, false)) => (RowStatus::Warmup, Some(effective)),
                    Some((effective, true)) => (RowStatus::Ready, Some(effective)),
                };
            }
        }
        let summary = &mut self.summary;
        match self.status {
            RowStatus::Ready => {
                summary.ready_rows += 1;
                summary
                    .first_ready_decision_utc
                    .get_or_insert_with(|| format_utc(decision_us));
            }
            RowStatus::Warmup => summary.warmup_rows += 1,
            RowStatus::Stale => summary.stale_rows += 1,
            RowStatus::NoObservation => summary.no_observation_rows += 1,
        }
        Ok(())
    }

    fn error_for(key: &str, at_us: i64, source: crate::indicators::IndicatorError) -> FeatureError {
        FeatureError::Indicator {
            feature_key: key.to_owned(),
            decision_time_utc: format_utc(at_us),
            source,
        }
    }

    fn ready_values(&self) -> Option<&[f64]> {
        (self.status == RowStatus::Ready).then_some(self.values.as_slice())
    }
}

enum ColumnBuffer {
    Times(Vec<i64>),
    OptionalTimes(Vec<Option<i64>>),
    Floats(Vec<Option<f64>>),
    Statuses(Vec<&'static str>),
}

impl ColumnBuffer {
    fn new(kind: ColumnKind) -> Self {
        match kind {
            ColumnKind::TimestampUtc => Self::Times(Vec::with_capacity(WRITE_BATCH_ROWS)),
            ColumnKind::NullableTimestampUtc => {
                Self::OptionalTimes(Vec::with_capacity(WRITE_BATCH_ROWS))
            }
            ColumnKind::NullableFloat64 => Self::Floats(Vec::with_capacity(WRITE_BATCH_ROWS)),
            ColumnKind::Status => Self::Statuses(Vec::with_capacity(WRITE_BATCH_ROWS)),
        }
    }

    fn take_array(&mut self) -> ArrayRef {
        match self {
            Self::Times(values) => Arc::new(
                TimestampMicrosecondArray::from(std::mem::take(values)).with_timezone("UTC"),
            ),
            Self::OptionalTimes(values) => Arc::new(
                TimestampMicrosecondArray::from(std::mem::take(values)).with_timezone("UTC"),
            ),
            Self::Floats(values) => Arc::new(Float64Array::from(std::mem::take(values))),
            Self::Statuses(values) => Arc::new(StringArray::from(std::mem::take(values))),
        }
    }
}

fn output_schema(columns: &[OutputColumn]) -> Schema {
    Schema::new(
        columns
            .iter()
            .map(|column| {
                let timestamp = DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()));
                match column.kind {
                    ColumnKind::TimestampUtc => Field::new(&column.name, timestamp, false),
                    ColumnKind::NullableTimestampUtc => Field::new(&column.name, timestamp, true),
                    ColumnKind::NullableFloat64 => {
                        Field::new(&column.name, DataType::Float64, true)
                    }
                    ColumnKind::Status => Field::new(&column.name, DataType::Utf8, false),
                }
            })
            .collect::<Vec<_>>(),
    )
}

fn output_columns(request: &ValidatedRequest) -> Vec<OutputColumn> {
    let mut columns: Vec<_> = ["open_time_utc", "close_time_utc", "decision_time_utc"]
        .iter()
        .map(|name| OutputColumn {
            name: (*name).into(),
            kind: ColumnKind::TimestampUtc,
            feature_key: None,
        })
        .collect();
    for (feature, _) in &request.features {
        let column = |suffix: &str, kind| OutputColumn {
            name: format!("{}__{suffix}", feature.key),
            kind,
            feature_key: Some(feature.key.clone()),
        };
        for output in &feature.outputs {
            columns.push(column(output, ColumnKind::NullableFloat64));
        }
        columns.push(column("status", ColumnKind::Status));
        if matches!(feature.source, FeatureSource::Reference { .. }) {
            columns.push(column(
                "source_available_at_utc",
                ColumnKind::NullableTimestampUtc,
            ));
        }
    }
    columns
}

struct OutputWriter {
    writer: ArrowWriter<File>,
    schema: Arc<Schema>,
    buffers: Vec<ColumnBuffer>,
    buffered_rows: usize,
    hasher: Sha256,
    max_output_bytes: u64,
}

impl OutputWriter {
    fn create(
        path: &Path,
        columns: &[OutputColumn],
        max_output_bytes: u64,
    ) -> Result<Self, FeatureError> {
        let schema = Arc::new(output_schema(columns));
        let properties = WriterProperties::builder()
            .set_max_row_group_row_count(Some(MAX_ROW_GROUP_ROWS))
            .build();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(Self {
            writer: ArrowWriter::try_new(file, Arc::clone(&schema), Some(properties))?,
            schema,
            buffers: columns
                .iter()
                .map(|column| ColumnBuffer::new(column.kind))
                .collect(),
            buffered_rows: 0,
            hasher: Sha256::new(),
            max_output_bytes,
        })
    }

    fn push_row(
        &mut self,
        bar: &CanonicalBar,
        evaluators: &[FeatureEvaluator],
    ) -> Result<(), FeatureError> {
        let mut buffers = self.buffers.iter_mut();
        let mut next = || {
            buffers
                .next()
                .expect("column buffers match the output schema")
        };
        for time in [bar.open_us, bar.close_us, bar.available_us] {
            if let ColumnBuffer::Times(values) = next() {
                values.push(time);
            }
            self.hasher.update(time.to_be_bytes());
        }
        for evaluator in evaluators {
            let ready = evaluator.ready_values();
            for index in 0..evaluator.values.len() {
                let value = ready.map(|values| values[index]);
                if let ColumnBuffer::Floats(values) = next() {
                    values.push(value);
                }
                match value {
                    Some(value) => {
                        self.hasher.update([1]);
                        self.hasher.update(value.to_bits().to_be_bytes());
                    }
                    None => self.hasher.update([0]),
                }
            }
            let status = evaluator.status.as_str();
            if let ColumnBuffer::Statuses(values) = next() {
                values.push(status);
            }
            hash_field(&mut self.hasher, status.as_bytes());
            if evaluator.reference.is_some() {
                if let ColumnBuffer::OptionalTimes(values) = next() {
                    values.push(evaluator.source_available_us);
                }
                match evaluator.source_available_us {
                    Some(time) => {
                        self.hasher.update([1]);
                        self.hasher.update(time.to_be_bytes());
                    }
                    None => self.hasher.update([0]),
                }
            }
        }
        self.buffered_rows += 1;
        if self.buffered_rows >= WRITE_BATCH_ROWS {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), FeatureError> {
        if self.buffered_rows == 0 {
            return Ok(());
        }
        let arrays = self
            .buffers
            .iter_mut()
            .map(ColumnBuffer::take_array)
            .collect();
        let batch = RecordBatch::try_new(Arc::clone(&self.schema), arrays)?;
        self.writer.write(&batch)?;
        self.buffered_rows = 0;
        let pending = self.writer.bytes_written() as u64 + self.writer.in_progress_size() as u64;
        if pending > self.max_output_bytes {
            return Err(FeatureError::ResourceQuota(format!(
                "feature output exceeded the configured {} byte quota",
                self.max_output_bytes
            )));
        }
        Ok(())
    }

    fn finish(mut self) -> Result<String, FeatureError> {
        self.flush()?;
        self.writer.close()?;
        Ok(hex(self.hasher.finalize()))
    }
}

pub(crate) fn materialize(
    request: &ValidatedRequest,
    inputs: &BTreeMap<String, ResolvedInput>,
    output_path: &Path,
    cancel: &AtomicBool,
    progress: &FeatureProgress,
) -> Result<Materialized, FeatureError> {
    let total_input_rows = inputs.values().map(|input| input.manifest.bar_count).sum();
    progress.set_phase(ProgressPhase::VerifyingInputs, total_input_rows);
    for input in inputs.values() {
        verify_canonical_input(input, cancel, progress)?;
    }
    let target_input = &inputs[&request.target.dataset_id];
    let target_rows = target_input
        .manifest
        .series
        .iter()
        .find(|series| {
            series.symbol == request.target.symbol && series.timeframe == request.target.timeframe
        })
        .map(|series| series.bar_count)
        .unwrap_or_default();
    progress.set_phase(ProgressPhase::Computing, target_rows);

    let mut target = SeriesCursor::open(
        target_input,
        &request.target.symbol,
        &request.target.timeframe,
    )?;
    let mut evaluators = Vec::with_capacity(request.features.len());
    for (feature, config) in &request.features {
        let state = IndicatorState::new(*config);
        let reference = match &feature.source {
            FeatureSource::Target => None,
            FeatureSource::Reference {
                series,
                max_staleness_seconds,
                availability_lag_seconds,
            } => Some(ReferenceStream {
                cursor: SeriesCursor::open(
                    &inputs[&series.dataset_id],
                    &series.symbol,
                    &series.timeframe,
                )?,
                lag_us: seconds_to_micros(*availability_lag_seconds)?,
                max_staleness_us: seconds_to_micros(*max_staleness_seconds)?,
                latest: None,
            }),
        };
        evaluators.push(FeatureEvaluator {
            key: feature.key.clone(),
            values: vec![0.0; state.output_count()],
            state,
            reference,
            status: RowStatus::Warmup,
            source_available_us: None,
            summary: FeatureSummary {
                key: feature.key.clone(),
                ..FeatureSummary::default()
            },
        });
    }

    let columns = output_columns(request);
    let mut writer = OutputWriter::create(output_path, &columns, request.quotas.max_output_bytes)?;
    let mut preview = VecDeque::with_capacity(PREVIEW_ROWS);
    let mut row_count = 0u64;
    while let Some(bar) = target.take()? {
        if row_count.is_multiple_of(1024) && cancel.load(Ordering::Relaxed) {
            return Err(FeatureError::Cancelled);
        }
        if row_count >= request.quotas.max_target_rows {
            return Err(FeatureError::ResourceQuota(format!(
                "target series exceeded the configured {} row quota",
                request.quotas.max_target_rows
            )));
        }
        for evaluator in &mut evaluators {
            evaluator.evaluate(&bar)?;
        }
        writer.push_row(&bar, &evaluators)?;
        if preview.len() == PREVIEW_ROWS {
            preview.pop_front();
        }
        preview.push_back(preview_row(&bar, &evaluators));
        row_count += 1;
        progress.add(1);
    }
    progress.set_phase(ProgressPhase::Finalizing, 0);
    let output_fingerprint = writer.finish()?;
    let output_sha256 = sha256_file(output_path, cancel)?;
    Ok(Materialized {
        row_count,
        output_sha256,
        output_fingerprint,
        columns,
        summaries: evaluators
            .into_iter()
            .map(|evaluator| evaluator.summary)
            .collect(),
        preview_rows: preview.into_iter().collect(),
    })
}

fn preview_row(bar: &CanonicalBar, evaluators: &[FeatureEvaluator]) -> PreviewRow {
    PreviewRow {
        open_time_utc: format_utc(bar.open_us),
        decision_time_utc: format_utc(bar.available_us),
        features: evaluators
            .iter()
            .map(|evaluator| PreviewFeatureValue {
                key: evaluator.key.clone(),
                status: evaluator.status,
                values: match evaluator.ready_values() {
                    Some(values) => values.iter().copied().map(Some).collect(),
                    None => vec![None; evaluator.values.len()],
                },
                source_available_at_utc: evaluator.source_available_us.map(format_utc),
            })
            .collect(),
    }
}

fn seconds_to_micros(seconds: u64) -> Result<i64, FeatureError> {
    i64::try_from(seconds)
        .ok()
        .and_then(|value| value.checked_mul(1_000_000))
        .ok_or_else(|| FeatureError::InvalidRequest("duration is out of range".into()))
}

pub(crate) fn format_utc(microseconds: i64) -> String {
    DateTime::from_timestamp_micros(microseconds)
        .unwrap_or(DateTime::UNIX_EPOCH)
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rg_data_ground::{
        prepare_dataset, CalendarPolicy, CsvDelimiter, FieldMapping, ImportRequest, MarketRole,
        ResourceQuotas, TimestampSemantics,
    };

    fn staged_input(directory: &tempfile::TempDir) -> ResolvedInput {
        let source = directory.path().join("synthetic-fixture.csv");
        std::fs::write(
            &source,
            "timestamp,symbol,timeframe,open,high,low,close,volume\n\
             2024-01-01T00:00:00Z,EURUSD.m,H1,1.1,1.2,1.0,1.15,3\n\
             2024-01-01T01:00:00Z,EURUSD.m,H1,1.15,1.25,1.1,1.2,0\n\
             2024-01-01T00:00:00Z,GBPUSD.m,H4,1.3,1.35,1.25,1.31,2\n",
        )
        .unwrap();
        let prepared = prepare_dataset(
            &source,
            directory.path().join("staging"),
            ImportRequest {
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
                default_symbol: "EURUSD.m".into(),
                default_timeframe: "H1".into(),
                default_market_role: MarketRole::Target,
                market_roles: Default::default(),
                source_id: "synthetic-fixture:engine-unit".into(),
                broker: "SYNTHETIC-FIXTURE".into(),
                server: "SYNTHETIC-FIXTURE".into(),
                delimiter: CsvDelimiter::Comma,
                calendar: CalendarPolicy::Continuous,
                quotas: ResourceQuotas {
                    max_source_bytes: Some(1_000_000),
                    max_records: Some(100),
                },
            },
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        ResolvedInput {
            bars_path: prepared.staging_directory.join("bars.parquet"),
            manifest: prepared.manifest,
        }
    }

    #[test]
    fn gate_input_authority_engine_recomputes_the_canonical_row_fingerprint() {
        let directory = tempfile::TempDir::new().unwrap();
        let input = staged_input(&directory);
        let never = AtomicBool::new(false);
        let progress = FeatureProgress::default();
        verify_canonical_input(&input, &never, &progress).unwrap();
        assert_eq!(progress.snapshot().processed_rows, 3);

        let mut wrong_fingerprint = ResolvedInput {
            manifest: input.manifest.clone(),
            bars_path: input.bars_path.clone(),
        };
        wrong_fingerprint.manifest.canonical_bars_fingerprint = "0".repeat(64);
        let mut wrong_count = ResolvedInput {
            manifest: input.manifest.clone(),
            bars_path: input.bars_path.clone(),
        };
        wrong_count.manifest.series[0].bar_count += 1;
        let mut wrong_schema = ResolvedInput {
            manifest: input.manifest.clone(),
            bars_path: directory.path().join("not-bars.parquet"),
        };
        std::fs::write(&wrong_schema.bars_path, b"PAR1 not parquet").unwrap();
        wrong_schema.manifest.dataset_id = input.manifest.dataset_id.clone();
        for candidate in [&wrong_fingerprint, &wrong_count, &wrong_schema] {
            assert!(matches!(
                verify_canonical_input(candidate, &never, &progress),
                Err(FeatureError::InputIntegrity { .. })
            ));
        }
        assert!(matches!(
            verify_canonical_input(&input, &AtomicBool::new(true), &progress),
            Err(FeatureError::Cancelled)
        ));
    }

    #[test]
    fn gate_multi_symbol_timeframe_join_cursor_reads_only_its_exact_series() {
        let directory = tempfile::TempDir::new().unwrap();
        let input = staged_input(&directory);
        let mut cursor = SeriesCursor::open(&input, "EURUSD.m", "H1").unwrap();
        let first = cursor.peek().unwrap().unwrap();
        assert_eq!(cursor.peek().unwrap().unwrap().open_us, first.open_us);
        assert_eq!(cursor.take().unwrap().unwrap().prices.close, 1.15);
        assert_eq!(cursor.take().unwrap().unwrap().prices.close, 1.2);
        assert!(cursor.take().unwrap().is_none());
        let mut suffixless = SeriesCursor::open(&input, "EURUSD", "H1").unwrap();
        assert!(suffixless.take().unwrap().is_none());
        let mut other_timeframe = SeriesCursor::open(&input, "GBPUSD.m", "H1").unwrap();
        assert!(other_timeframe.take().unwrap().is_none());
    }

    #[test]
    fn gate_resource_controls_progress_phases_are_observable() {
        let progress = FeatureProgress::default();
        assert_eq!(progress.snapshot().phase, ProgressPhase::Queued);
        progress.set_phase(ProgressPhase::Computing, 10);
        progress.add(4);
        assert_eq!(
            progress.snapshot(),
            ProgressSnapshot {
                phase: ProgressPhase::Computing,
                processed_rows: 4,
                total_rows: 10
            }
        );
        progress.set_phase(ProgressPhase::Finalizing, 0);
        assert_eq!(progress.snapshot().processed_rows, 0);
    }

    #[test]
    fn gate_causal_availability_bar_contract_rejects_early_availability() {
        let bar = CanonicalBar {
            open_us: 0,
            close_us: Timeframe::H1.duration_micros(),
            available_us: Timeframe::H1.duration_micros() - 1,
            prices: PriceBar {
                open: 1.0,
                high: 1.0,
                low: 1.0,
                close: 1.0,
            },
        };
        assert!(validate_bar(Timeframe::H1, &bar, 0.0).is_err());
        let wrong_length = CanonicalBar {
            close_us: Timeframe::M15.duration_micros(),
            available_us: Timeframe::M15.duration_micros(),
            ..bar
        };
        assert!(validate_bar(Timeframe::H1, &wrong_length, 0.0).is_err());
        let valid = CanonicalBar {
            available_us: Timeframe::H1.duration_micros(),
            ..bar
        };
        assert!(validate_bar(Timeframe::H1, &valid, 0.0).is_ok());
        assert!(validate_bar(Timeframe::H1, &valid, f64::NAN).is_err());
    }
}
