//! Offline, immutable OHLCV ingestion and dataset snapshot contracts.

pub mod store;

use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use arrow::{
    array::{
        Array, Decimal128Array, Decimal256Array, Float32Array, Float64Array, Int16Array,
        Int32Array, Int64Array, Int8Array, RecordBatch, StringArray, TimestampMicrosecondArray,
        UInt16Array, UInt32Array, UInt64Array, UInt8Array,
    },
    datatypes::{DataType, Field, Schema, TimeUnit},
};
use chrono::{DateTime, Datelike, FixedOffset, NaiveDateTime, TimeZone, Utc, Weekday};
use parquet::arrow::{arrow_reader::ParquetRecordBatchReaderBuilder, ArrowWriter};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

pub const DATASET_MANIFEST_SCHEMA_VERSION: u16 = 1;
pub const IMPORT_BATCH_ROWS: usize = 8192;
pub const MAX_GAP_SAMPLES: usize = 100;
pub const SOURCE_PREVIEW_MAX_BYTES: u64 = 64 * 1024 * 1024;
const COPY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum DataGroundError {
    #[error("source file is not a selected regular local file")]
    UnsafeSourcePath,
    #[error("only CSV and Parquet source files are supported")]
    UnsupportedFormat,
    #[error("import cancelled")]
    Cancelled,
    #[error("resource quota exceeded: {0}")]
    ResourceQuota(String),
    #[error("invalid import request: {0}")]
    InvalidRequest(String),
    #[error("source schema error: {0}")]
    Schema(String),
    #[error("invalid source row {row}: {reason}")]
    InvalidRow { row: u64, reason: String },
    #[error("duplicate bar: {symbol} {timeframe} at {open_time_utc}")]
    DuplicateBar {
        symbol: String,
        timeframe: String,
        open_time_utc: String,
    },
    #[error("out-of-order bar: {symbol} {timeframe} at {open_time_utc}")]
    OutOfOrderBar {
        symbol: String,
        timeframe: String,
        open_time_utc: String,
    },
    #[error("internal staging database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("CSV parser error: {0}")]
    Csv(#[from] csv::Error),
    #[error("Arrow/Parquet error: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),
    #[error("Arrow error: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("time parsing error: {0}")]
    Time(String),
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Timeframe {
    M1,
    M5,
    M15,
    M30,
    H1,
    H4,
    D1,
    W1,
}

impl Timeframe {
    pub fn parse(value: &str) -> Result<Self, DataGroundError> {
        match value.trim().to_ascii_uppercase().as_str() {
            "M1" => Ok(Self::M1),
            "M5" => Ok(Self::M5),
            "M15" => Ok(Self::M15),
            "M30" => Ok(Self::M30),
            "H1" => Ok(Self::H1),
            "H4" => Ok(Self::H4),
            "D1" => Ok(Self::D1),
            "W1" => Ok(Self::W1),
            _ => Err(DataGroundError::InvalidRequest(format!(
                "unsupported timeframe: {value}"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::M1 => "M1",
            Self::M5 => "M5",
            Self::M15 => "M15",
            Self::M30 => "M30",
            Self::H1 => "H1",
            Self::H4 => "H4",
            Self::D1 => "D1",
            Self::W1 => "W1",
        }
    }

    pub fn duration_micros(self) -> i64 {
        match self {
            Self::M1 => 60_000_000,
            Self::M5 => 300_000_000,
            Self::M15 => 900_000_000,
            Self::M30 => 1_800_000_000,
            Self::H1 => 3_600_000_000,
            Self::H4 => 14_400_000_000,
            Self::D1 => 86_400_000_000,
            Self::W1 => 604_800_000_000,
        }
    }

    fn alignment_anchor_micros(self) -> i64 {
        match self {
            // 1970-01-05 00:00:00 UTC was the first Monday after the Unix epoch.
            Self::W1 => 345_600_000_000,
            _ => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TimestampSemantics {
    BarOpensAt,
    BarClosesAt,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MarketRole {
    Target,
    Reference,
}

impl MarketRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::Target => "TARGET",
            Self::Reference => "REFERENCE",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CsvDelimiter {
    Comma,
    Semicolon,
    Tab,
}

impl CsvDelimiter {
    fn byte(self) -> u8 {
        match self {
            Self::Comma => b',',
            Self::Semicolon => b';',
            Self::Tab => b'\t',
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FieldMapping {
    pub timestamp: String,
    pub open: String,
    pub high: String,
    pub low: String,
    pub close: String,
    pub volume: String,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub timeframe: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ClosedInterval {
    pub start_utc: String,
    pub end_utc: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CalendarPolicy {
    Unknown,
    Continuous,
    WeekendsUtc,
    CustomClosedIntervals { intervals: Vec<ClosedInterval> },
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResourceQuotas {
    #[serde(default)]
    pub max_source_bytes: Option<u64>,
    #[serde(default)]
    pub max_records: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ImportRequest {
    pub mapping: FieldMapping,
    pub timestamp_semantics: TimestampSemantics,
    /// Fixed offset, in minutes east of UTC, for source timestamps with no embedded offset.
    pub naive_timestamp_offset_minutes: i16,
    pub default_symbol: String,
    pub default_timeframe: String,
    pub default_market_role: MarketRole,
    #[serde(default)]
    pub market_roles: BTreeMap<String, MarketRole>,
    pub source_id: String,
    pub broker: String,
    pub server: String,
    pub delimiter: CsvDelimiter,
    pub calendar: CalendarPolicy,
    #[serde(default)]
    pub quotas: ResourceQuotas,
}

impl ImportRequest {
    pub fn validate(&self) -> Result<(), DataGroundError> {
        validate_symbol(&self.default_symbol)?;
        Timeframe::parse(&self.default_timeframe)?;
        for symbol in self.market_roles.keys() {
            validate_symbol(symbol)?;
        }
        for (label, value) in [
            ("source_id", self.source_id.as_str()),
            ("broker", self.broker.as_str()),
            ("server", self.server.as_str()),
        ] {
            if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
                return Err(DataGroundError::InvalidRequest(format!(
                    "{label} must be a non-empty printable value of at most 256 bytes"
                )));
            }
        }
        if !(-14 * 60..=14 * 60).contains(&self.naive_timestamp_offset_minutes) {
            return Err(DataGroundError::InvalidRequest(
                "timestamp UTC offset must be within -14:00 and +14:00".into(),
            ));
        }
        if self.quotas.max_source_bytes.is_none_or(|limit| limit == 0)
            || self.quotas.max_records.is_none_or(|limit| limit == 0)
        {
            return Err(DataGroundError::InvalidRequest(
                "positive source-byte and record quotas are required for bounded imports".into(),
            ));
        }
        let mut fields = vec![
            self.mapping.timestamp.as_str(),
            self.mapping.open.as_str(),
            self.mapping.high.as_str(),
            self.mapping.low.as_str(),
            self.mapping.close.as_str(),
            self.mapping.volume.as_str(),
        ];
        fields.extend(self.mapping.symbol.as_deref());
        fields.extend(self.mapping.timeframe.as_deref());
        if fields.iter().any(|field| field.trim().is_empty()) {
            return Err(DataGroundError::InvalidRequest(
                "mapped source field names cannot be empty".into(),
            ));
        }
        let mut mapped = std::collections::BTreeSet::new();
        for field in &fields {
            if !mapped.insert(*field) {
                return Err(DataGroundError::InvalidRequest(
                    "each mapped source field must be distinct".into(),
                ));
            }
        }
        if let CalendarPolicy::CustomClosedIntervals { intervals } = &self.calendar {
            let mut previous_end = None;
            for interval in intervals {
                let start = parse_rfc3339_utc(&interval.start_utc)?;
                let end = parse_rfc3339_utc(&interval.end_utc)?;
                if start >= end || previous_end.is_some_and(|old_end| start < old_end) {
                    return Err(DataGroundError::InvalidRequest(
                        "custom closed intervals must be sorted, non-empty, and non-overlapping"
                            .into(),
                    ));
                }
                previous_end = Some(end);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OhlcvBar {
    pub symbol: String,
    pub timeframe: Timeframe,
    pub role: MarketRole,
    pub open_time_utc: DateTime<Utc>,
    pub close_time_utc: DateTime<Utc>,
    pub available_at_utc: DateTime<Utc>,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GapClass {
    MissingBars,
    ClosedSession,
    Unclassified,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct GapSample {
    pub symbol: String,
    pub timeframe: String,
    pub first_missing_open_utc: String,
    pub next_observed_open_utc: String,
    pub expected_missing_bars: u64,
    pub closed_session_bars: u64,
    pub missing_bars: u64,
    pub unclassified_bars: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct QualityAudit {
    pub duplicate_bars: u64,
    pub descending_bars: u64,
    pub missing_bars: u64,
    pub closed_session_bars: u64,
    pub unclassified_gap_bars: u64,
    pub gaps: Vec<GapSample>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SeriesSummary {
    pub symbol: String,
    pub timeframe: String,
    pub market_role: String,
    pub bar_count: u64,
    pub first_open_utc: String,
    pub last_open_utc: String,
    pub missing_bars: u64,
    pub closed_session_bars: u64,
    pub unclassified_gap_bars: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DatasetStatus {
    Ready,
    Blocked,
    Quarantined,
}

impl DatasetStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ready => "READY",
            Self::Blocked => "BLOCKED",
            Self::Quarantined => "QUARANTINED",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SourceProvenance {
    pub source_id: String,
    pub broker: String,
    pub server: String,
    pub source_file_name: String,
    pub source_format: String,
    pub source_size_bytes: u64,
    pub raw_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DatasetManifest {
    pub schema_version: u16,
    pub dataset_id: String,
    pub fingerprint: String,
    pub created_at_utc: String,
    pub provenance: SourceProvenance,
    pub import_request: ImportRequest,
    pub canonical_bars_file: String,
    pub canonical_bars_sha256: String,
    pub canonical_bars_fingerprint: String,
    pub bar_count: u64,
    pub status: DatasetStatus,
    pub series: Vec<SeriesSummary>,
    pub quality_audit: QualityAudit,
}

impl DatasetManifest {
    pub fn canonical_json(&self) -> Result<String, DataGroundError> {
        Ok(serde_json::to_string(self)?)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SourceInspection {
    pub file_name: String,
    pub extension: String,
    pub size_bytes: u64,
    pub headers: Vec<String>,
    pub sample_rows: Vec<BTreeMap<String, String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ImportPreview {
    pub dataset_id: String,
    pub fingerprint: String,
    pub status: DatasetStatus,
    pub source_file_name: String,
    pub source_format: String,
    pub source_size_bytes: u64,
    pub source_sha256: String,
    pub raw_sha256: String,
    pub canonical_bars_sha256: String,
    pub canonical_bars_fingerprint: String,
    pub parquet_sha256: String,
    pub manifest_sha256: String,
    pub bar_count: u64,
    pub series_count: u64,
    pub series: Vec<SeriesSummary>,
    pub quality_audit: QualityAudit,
}

#[derive(Debug)]
pub struct PreparedDataset {
    pub manifest: DatasetManifest,
    pub manifest_json: String,
    pub manifest_sha256: String,
    pub preview: ImportPreview,
    pub staging_directory: PathBuf,
}

#[derive(Default)]
struct SeriesAccumulator {
    role: Option<MarketRole>,
    bar_count: u64,
    first_open: Option<i64>,
    last_open: Option<i64>,
    missing_bars: u64,
    closed_session_bars: u64,
    unclassified_gap_bars: u64,
}

struct StageContext {
    request: ImportRequest,
    stage_directory: PathBuf,
    raw_copy_path: PathBuf,
    source_format: String,
    source_name: String,
    source_size_bytes: u64,
    raw_sha256: String,
    cancel: Arc<AtomicBool>,
    row_count: u64,
    last_by_series: HashMap<(String, Timeframe), i64>,
    series: BTreeMap<(String, Timeframe), SeriesAccumulator>,
    audit: QualityAudit,
}

fn validate_symbol(symbol: &str) -> Result<(), DataGroundError> {
    let trimmed = symbol.trim();
    if trimmed.is_empty()
        || trimmed != symbol
        || symbol.len() > 64
        || symbol
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(DataGroundError::InvalidRequest(format!(
            "invalid market symbol: {symbol}"
        )));
    }
    Ok(())
}

fn parse_rfc3339_utc(value: &str) -> Result<DateTime<Utc>, DataGroundError> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|error| DataGroundError::Time(format!("{value}: {error}")))
}

pub fn inspect_source(path: impl AsRef<Path>) -> Result<SourceInspection, DataGroundError> {
    inspect_source_with_limit(path, SOURCE_PREVIEW_MAX_BYTES, CsvDelimiter::Comma)
}

pub fn inspect_source_with_limit(
    path: impl AsRef<Path>,
    max_source_bytes: u64,
    delimiter: CsvDelimiter,
) -> Result<SourceInspection, DataGroundError> {
    let path = validate_source_path(path.as_ref())?;
    let metadata = fs::metadata(&path)?;
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let file_name = safe_file_name(&path)?;
    if metadata.len() > max_source_bytes {
        return Err(DataGroundError::ResourceQuota(format!(
            "source is {} bytes; inspection quota is {max_source_bytes}",
            metadata.len()
        )));
    }
    let (headers, sample_rows) = match extension.as_str() {
        "csv" => {
            let file = File::open(&path)?;
            let mut reader = csv::ReaderBuilder::new()
                .flexible(true)
                .delimiter(delimiter.byte())
                .from_reader(file.take(SOURCE_PREVIEW_MAX_BYTES));
            let header = reader.headers()?.clone();
            let headers = header.iter().map(str::to_owned).collect::<Vec<_>>();
            if headers.iter().any(String::is_empty)
                || headers
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    != headers.len()
            {
                return Err(DataGroundError::Schema(
                    "CSV headers must be non-empty and unique".into(),
                ));
            }
            let mut sample_rows = Vec::new();
            for record in reader.records().take(10) {
                let record = record?;
                if record.len() != headers.len() {
                    return Err(DataGroundError::Schema(
                        "CSV preview row width does not match its header".into(),
                    ));
                }
                let mut row = BTreeMap::new();
                for (index, value) in record.iter().enumerate() {
                    row.insert(headers[index].clone(), value.to_owned());
                }
                sample_rows.push(row);
            }
            (headers, sample_rows)
        }
        "parquet" => {
            let file = File::open(&path)?;
            let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
            let schema = builder.schema();
            let headers = schema
                .fields()
                .iter()
                .map(|field| field.name().clone())
                .collect();
            let reader = builder.with_batch_size(10).build()?;
            let mut sample_rows = Vec::new();
            for batch in reader {
                let batch = batch?;
                for row_index in 0..batch
                    .num_rows()
                    .min(10usize.saturating_sub(sample_rows.len()))
                {
                    let mut row = BTreeMap::new();
                    for (column_index, field) in batch.schema().fields().iter().enumerate() {
                        row.insert(
                            field.name().clone(),
                            parquet_cell_as_text(batch.column(column_index).as_ref(), row_index),
                        );
                    }
                    sample_rows.push(row);
                }
                if sample_rows.len() >= 10 {
                    break;
                }
            }
            (headers, sample_rows)
        }
        _ => return Err(DataGroundError::UnsupportedFormat),
    };
    Ok(SourceInspection {
        file_name,
        extension,
        size_bytes: metadata.len(),
        headers,
        sample_rows,
    })
}

pub fn prepare_dataset(
    source_path: impl AsRef<Path>,
    staging_root: impl AsRef<Path>,
    request: ImportRequest,
    cancel: Arc<AtomicBool>,
) -> Result<PreparedDataset, DataGroundError> {
    request.validate()?;
    let source_path = validate_source_path(source_path.as_ref())?;
    let max_source_bytes = request
        .quotas
        .max_source_bytes
        .ok_or_else(|| DataGroundError::InvalidRequest("source-byte quota is required".into()))?;
    let inspection = inspect_source_with_limit(&source_path, max_source_bytes, request.delimiter)?;
    let source_format = inspection.extension.clone();
    if let Some(max_bytes) = request.quotas.max_source_bytes {
        if inspection.size_bytes > max_bytes {
            return Err(DataGroundError::ResourceQuota(format!(
                "source is {} bytes; quota is {max_bytes}",
                inspection.size_bytes
            )));
        }
    }
    let staging_root = staging_root.as_ref();
    fs::create_dir_all(staging_root)?;
    let canonical_staging_root = fs::canonicalize(staging_root)?;
    let stage_id = Uuid::now_v7().to_string();
    let stage_directory = canonical_staging_root.join(&stage_id);
    fs::create_dir(&stage_directory)?;
    let result = prepare_dataset_in_stage(
        &source_path,
        &inspection,
        source_format,
        request,
        cancel,
        stage_directory.clone(),
    );
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage_directory);
    }
    result
}

fn prepare_dataset_in_stage(
    source_path: &Path,
    inspection: &SourceInspection,
    source_format: String,
    request: ImportRequest,
    cancel: Arc<AtomicBool>,
    stage_directory: PathBuf,
) -> Result<PreparedDataset, DataGroundError> {
    check_cancelled(&cancel)?;
    let raw_copy_path = stage_directory.join(format!("raw-source.{source_format}"));
    let (raw_sha256, source_size_bytes) = copy_source_snapshot(
        source_path,
        &raw_copy_path,
        request.quotas.max_source_bytes,
        &cancel,
    )?;
    let mut context = StageContext {
        request,
        stage_directory: stage_directory.clone(),
        raw_copy_path,
        source_format,
        source_name: inspection.file_name.clone(),
        source_size_bytes,
        raw_sha256,
        cancel,
        row_count: 0,
        last_by_series: HashMap::new(),
        series: BTreeMap::new(),
        audit: QualityAudit::default(),
    };
    let staging_db_path = stage_directory.join("bars-stage.sqlite3");
    let mut staging_db = Connection::open(&staging_db_path)?;
    staging_db.execute_batch(
        "PRAGMA journal_mode=OFF;
         PRAGMA synchronous=OFF;
         CREATE TABLE bars (
             symbol TEXT NOT NULL,
             timeframe TEXT NOT NULL,
             role TEXT NOT NULL,
             open_time_utc_us INTEGER NOT NULL,
             close_time_utc_us INTEGER NOT NULL,
             available_at_utc_us INTEGER NOT NULL,
             open REAL NOT NULL,
             high REAL NOT NULL,
             low REAL NOT NULL,
             close REAL NOT NULL,
             volume REAL NOT NULL,
             UNIQUE(symbol, timeframe, open_time_utc_us)
         );",
    )?;
    match context.source_format.as_str() {
        "csv" => parse_csv_into_stage(&mut staging_db, &mut context)?,
        "parquet" => parse_parquet_into_stage(&mut staging_db, &mut context)?,
        _ => return Err(DataGroundError::UnsupportedFormat),
    }
    check_cancelled(&context.cancel)?;
    if context.row_count == 0 {
        return Err(DataGroundError::Schema(
            "source contains no market-data rows".into(),
        ));
    }
    let canonical_bars_fingerprint = write_canonical_parquet(
        &staging_db,
        &stage_directory.join("bars.parquet"),
        &context.cancel,
    )?;
    drop(staging_db);
    let _ = fs::remove_file(&staging_db_path);
    let parquet_sha256 = hash_file(&stage_directory.join("bars.parquet"), &context.cancel)?;
    let canonical_bars_sha256 = parquet_sha256.clone();
    let series = series_summaries(&context.series)?;
    context.audit.gaps.shrink_to_fit();
    let status = if context.audit.missing_bars == 0 && context.audit.unclassified_gap_bars == 0 {
        DatasetStatus::Ready
    } else {
        DatasetStatus::Blocked
    };
    let provenance = SourceProvenance {
        source_id: context.request.source_id.clone(),
        broker: context.request.broker.clone(),
        server: context.request.server.clone(),
        source_file_name: context.source_name.clone(),
        source_format: context.source_format.clone(),
        source_size_bytes: context.source_size_bytes,
        raw_sha256: context.raw_sha256.clone(),
    };
    let fingerprint_input = FingerprintInput {
        schema_version: DATASET_MANIFEST_SCHEMA_VERSION,
        provenance: &provenance,
        import_request: &context.request,
        canonical_bars_fingerprint: &canonical_bars_fingerprint,
        bar_count: context.row_count,
        series: &series,
        quality_audit: &context.audit,
    };
    let fingerprint_bytes = serde_json::to_vec(&fingerprint_input)?;
    let fingerprint = sha256_hex(&fingerprint_bytes);
    let dataset_id = format!("ds_{fingerprint}");
    let manifest = DatasetManifest {
        schema_version: DATASET_MANIFEST_SCHEMA_VERSION,
        dataset_id: dataset_id.clone(),
        fingerprint: fingerprint.clone(),
        created_at_utc: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        provenance,
        import_request: context.request,
        canonical_bars_file: "bars.parquet".into(),
        canonical_bars_sha256: canonical_bars_sha256.clone(),
        canonical_bars_fingerprint: canonical_bars_fingerprint.clone(),
        bar_count: context.row_count,
        status: status.clone(),
        series: series.clone(),
        quality_audit: context.audit.clone(),
    };
    let manifest_json = manifest.canonical_json()?;
    let manifest_sha256 = sha256_hex(manifest_json.as_bytes());
    fs::write(
        stage_directory.join("manifest.json"),
        manifest_json.as_bytes(),
    )?;
    let preview = ImportPreview {
        dataset_id,
        fingerprint,
        status,
        source_file_name: context.source_name,
        source_format: context.source_format,
        source_size_bytes: context.source_size_bytes,
        source_sha256: context.raw_sha256.clone(),
        raw_sha256: context.raw_sha256,
        canonical_bars_sha256,
        canonical_bars_fingerprint,
        parquet_sha256,
        manifest_sha256: manifest_sha256.clone(),
        bar_count: context.row_count,
        series_count: series.len() as u64,
        series,
        quality_audit: context.audit,
    };
    Ok(PreparedDataset {
        manifest,
        manifest_json,
        manifest_sha256,
        preview,
        staging_directory: context.stage_directory,
    })
}

#[derive(Serialize)]
struct FingerprintInput<'a> {
    schema_version: u16,
    provenance: &'a SourceProvenance,
    import_request: &'a ImportRequest,
    canonical_bars_fingerprint: &'a str,
    bar_count: u64,
    series: &'a [SeriesSummary],
    quality_audit: &'a QualityAudit,
}

fn validate_source_path(path: &Path) -> Result<PathBuf, DataGroundError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(DataGroundError::UnsafeSourcePath);
    }
    for ancestor in path.ancestors() {
        let metadata =
            fs::symlink_metadata(ancestor).map_err(|_| DataGroundError::UnsafeSourcePath)?;
        if metadata.file_type().is_symlink() {
            return Err(DataGroundError::UnsafeSourcePath);
        }
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| DataGroundError::UnsafeSourcePath)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(DataGroundError::UnsafeSourcePath);
    }
    let canonical = fs::canonicalize(path).map_err(|_| DataGroundError::UnsafeSourcePath)?;
    let canonical_metadata =
        fs::symlink_metadata(&canonical).map_err(|_| DataGroundError::UnsafeSourcePath)?;
    if canonical_metadata.file_type().is_symlink() || !canonical_metadata.is_file() {
        return Err(DataGroundError::UnsafeSourcePath);
    }
    Ok(canonical)
}

fn safe_file_name(path: &Path) -> Result<String, DataGroundError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(DataGroundError::UnsafeSourcePath)?;
    if name.is_empty() || name.chars().any(char::is_control) {
        return Err(DataGroundError::UnsafeSourcePath);
    }
    Ok(name.to_owned())
}

fn copy_source_snapshot(
    source: &Path,
    target: &Path,
    max_bytes: Option<u64>,
    cancel: &AtomicBool,
) -> Result<(String, u64), DataGroundError> {
    let mut input = File::open(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    let mut copied = 0u64;
    loop {
        check_cancelled_ref(cancel)?;
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        copied = copied.saturating_add(count as u64);
        if max_bytes.is_some_and(|limit| copied > limit) {
            return Err(DataGroundError::ResourceQuota(format!(
                "source exceeded the configured byte quota of {}",
                max_bytes.unwrap_or_default()
            )));
        }
        hasher.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    output.sync_all()?;
    Ok((hex_digest(hasher.finalize()), copied))
}

fn hash_file(path: &Path, cancel: &AtomicBool) -> Result<String, DataGroundError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    loop {
        check_cancelled_ref(cancel)?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex_digest(hasher.finalize()))
}

fn parse_csv_into_stage(
    database: &mut Connection,
    context: &mut StageContext,
) -> Result<(), DataGroundError> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(context.request.delimiter.byte())
        .has_headers(true)
        .flexible(false)
        .from_path(&context.raw_copy_path)?;
    let headers = reader.headers()?.clone();
    validate_headers(headers.iter())?;
    let indices = resolve_mapping(&headers, &context.request.mapping)?;
    let mut row = csv::StringRecord::new();
    let mut source_row = 1u64;
    while reader.read_record(&mut row)? {
        source_row += 1;
        check_cancelled_ref(&context.cancel)?;
        if context
            .request
            .quotas
            .max_records
            .is_some_and(|limit| context.row_count >= limit)
        {
            return Err(DataGroundError::ResourceQuota(format!(
                "source exceeded the configured record quota of {}",
                context.request.quotas.max_records.unwrap_or_default()
            )));
        }
        let get = |index: usize, field: &str| {
            row.get(index).ok_or_else(|| DataGroundError::InvalidRow {
                row: source_row,
                reason: format!("mapped field {field} is absent"),
            })
        };
        let timestamp = get(indices.timestamp, &context.request.mapping.timestamp)?;
        let symbol = match indices.symbol {
            Some(index) => get(
                index,
                context.request.mapping.symbol.as_deref().unwrap_or(""),
            )?,
            None => context.request.default_symbol.as_str(),
        };
        let timeframe = match indices.timeframe {
            Some(index) => get(
                index,
                context.request.mapping.timeframe.as_deref().unwrap_or(""),
            )?,
            None => context.request.default_timeframe.as_str(),
        };
        let bar = parse_source_bar(
            SourceBarInput {
                row_number: source_row,
                symbol_value: symbol,
                timeframe_value: timeframe,
                timestamp_value: timestamp,
                open_value: get(indices.open, &context.request.mapping.open)?,
                high_value: get(indices.high, &context.request.mapping.high)?,
                low_value: get(indices.low, &context.request.mapping.low)?,
                close_value: get(indices.close, &context.request.mapping.close)?,
                volume_value: get(indices.volume, &context.request.mapping.volume)?,
            },
            &context.request,
        )?;
        insert_bar(database, context, bar, source_row)?;
    }
    Ok(())
}

fn parse_parquet_into_stage(
    database: &mut Connection,
    context: &mut StageContext,
) -> Result<(), DataGroundError> {
    let file = File::open(&context.raw_copy_path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let schema = builder.schema().clone();
    validate_headers(schema.fields().iter().map(|field| field.name().as_str()))?;
    let mapping = &context.request.mapping;
    let indices = resolve_schema_mapping(&schema, mapping)?;
    let reader = builder.with_batch_size(IMPORT_BATCH_ROWS).build()?;
    let mut source_row = 1u64;
    for batch in reader {
        let batch = batch?;
        for row_index in 0..batch.num_rows() {
            source_row += 1;
            check_cancelled_ref(&context.cancel)?;
            if context
                .request
                .quotas
                .max_records
                .is_some_and(|limit| context.row_count >= limit)
            {
                return Err(DataGroundError::ResourceQuota(format!(
                    "source exceeded the configured record quota of {}",
                    context.request.quotas.max_records.unwrap_or_default()
                )));
            }
            let timestamp = arrow_timestamp(
                batch.column(indices.timestamp).as_ref(),
                row_index,
                context.request.naive_timestamp_offset_minutes,
            )
            .map_err(|reason| DataGroundError::InvalidRow {
                row: source_row,
                reason,
            })?;
            let symbol = match indices.symbol {
                Some(index) => {
                    arrow_string(batch.column(index).as_ref(), row_index).map_err(|reason| {
                        DataGroundError::InvalidRow {
                            row: source_row,
                            reason,
                        }
                    })?
                }
                None => context.request.default_symbol.clone(),
            };
            let timeframe = match indices.timeframe {
                Some(index) => {
                    arrow_string(batch.column(index).as_ref(), row_index).map_err(|reason| {
                        DataGroundError::InvalidRow {
                            row: source_row,
                            reason,
                        }
                    })?
                }
                None => context.request.default_timeframe.clone(),
            };
            let bar = parse_numeric_bar(
                NumericBarInput {
                    row_number: source_row,
                    symbol_value: &symbol,
                    timeframe_value: &timeframe,
                    timestamp,
                    open: arrow_number(batch.column(indices.open).as_ref(), row_index, "open")
                        .map_err(|reason| DataGroundError::InvalidRow {
                            row: source_row,
                            reason,
                        })?,
                    high: arrow_number(batch.column(indices.high).as_ref(), row_index, "high")
                        .map_err(|reason| DataGroundError::InvalidRow {
                            row: source_row,
                            reason,
                        })?,
                    low: arrow_number(batch.column(indices.low).as_ref(), row_index, "low")
                        .map_err(|reason| DataGroundError::InvalidRow {
                            row: source_row,
                            reason,
                        })?,
                    close: arrow_number(batch.column(indices.close).as_ref(), row_index, "close")
                        .map_err(|reason| DataGroundError::InvalidRow {
                        row: source_row,
                        reason,
                    })?,
                    volume: arrow_number(
                        batch.column(indices.volume).as_ref(),
                        row_index,
                        "volume",
                    )
                    .map_err(|reason| DataGroundError::InvalidRow {
                        row: source_row,
                        reason,
                    })?,
                },
                &context.request,
            )?;
            insert_bar(database, context, bar, source_row)?;
        }
    }
    Ok(())
}

fn resolve_schema_mapping(
    schema: &Schema,
    mapping: &FieldMapping,
) -> Result<MappingIndices, DataGroundError> {
    let index = |name: &str| {
        schema
            .index_of(name)
            .map_err(|_| DataGroundError::Schema(format!("mapped field not found: {name}")))
    };
    Ok(MappingIndices {
        timestamp: index(&mapping.timestamp)?,
        open: index(&mapping.open)?,
        high: index(&mapping.high)?,
        low: index(&mapping.low)?,
        close: index(&mapping.close)?,
        volume: index(&mapping.volume)?,
        symbol: mapping.symbol.as_deref().map(index).transpose()?,
        timeframe: mapping.timeframe.as_deref().map(index).transpose()?,
    })
}

fn parse_numeric_bar(
    values: NumericBarInput<'_>,
    request: &ImportRequest,
) -> Result<OhlcvBar, DataGroundError> {
    let NumericBarInput {
        row_number,
        symbol_value,
        timeframe_value,
        timestamp,
        open,
        high,
        low,
        close,
        volume,
    } = values;
    let invalid = |reason: String| DataGroundError::InvalidRow {
        row: row_number,
        reason,
    };
    validate_symbol(symbol_value).map_err(|error| invalid(error.to_string()))?;
    let symbol = symbol_value.to_owned();
    let timeframe =
        Timeframe::parse(timeframe_value).map_err(|error| invalid(error.to_string()))?;
    let step = timeframe.duration_micros();
    let timestamp_us = timestamp.timestamp_micros();
    let (open_us, close_us) = match request.timestamp_semantics {
        TimestampSemantics::BarOpensAt => (timestamp_us, timestamp_us.saturating_add(step)),
        TimestampSemantics::BarClosesAt => (timestamp_us.saturating_sub(step), timestamp_us),
    };
    if (open_us - timeframe.alignment_anchor_micros()).rem_euclid(step) != 0 {
        return Err(invalid(format!(
            "{} timestamp is not aligned to its timeframe in UTC",
            timeframe.as_str()
        )));
    }
    if ![open, high, low, close, volume]
        .iter()
        .all(|value| value.is_finite())
    {
        return Err(invalid("OHLCV values must be finite".into()));
    }
    if [open, high, low, close].iter().any(|value| *value <= 0.0) {
        return Err(invalid("OHLC values must be positive".into()));
    }
    if volume < 0.0 {
        return Err(invalid("volume must be non-negative".into()));
    }
    if low > high || low > open.min(close) || high < open.max(close) {
        return Err(invalid("OHLC values are inconsistent".into()));
    }
    let open_time_utc = DateTime::from_timestamp_micros(open_us)
        .ok_or_else(|| invalid("bar open timestamp is outside the supported range".into()))?;
    let close_time_utc = DateTime::from_timestamp_micros(close_us)
        .ok_or_else(|| invalid("bar close timestamp is outside the supported range".into()))?;
    let role = request
        .market_roles
        .get(&symbol)
        .copied()
        .unwrap_or(request.default_market_role);
    Ok(OhlcvBar {
        symbol,
        timeframe,
        role,
        open_time_utc,
        close_time_utc,
        available_at_utc: close_time_utc,
        open,
        high,
        low,
        close,
        volume,
    })
}

fn arrow_timestamp(
    array: &dyn Array,
    row_index: usize,
    naive_offset_minutes: i16,
) -> Result<DateTime<Utc>, String> {
    if array.is_null(row_index) {
        return Err("timestamp is null".into());
    }
    match array.data_type() {
        DataType::Utf8 | DataType::LargeUtf8 => {
            let value = arrow_string(array, row_index)?;
            parse_source_timestamp(&value, naive_offset_minutes).map_err(|error| error.to_string())
        }
        DataType::Timestamp(unit, timezone) => {
            let raw = match unit {
                TimeUnit::Second => {
                    i128::from(
                        array
                            .as_any()
                            .downcast_ref::<arrow::array::TimestampSecondArray>()
                            .ok_or_else(|| "timestamp array type mismatch".to_owned())?
                            .value(row_index),
                    ) * 1_000_000
                }
                TimeUnit::Millisecond => {
                    i128::from(
                        array
                            .as_any()
                            .downcast_ref::<arrow::array::TimestampMillisecondArray>()
                            .ok_or_else(|| "timestamp array type mismatch".to_owned())?
                            .value(row_index),
                    ) * 1_000
                }
                TimeUnit::Microsecond => i128::from(
                    array
                        .as_any()
                        .downcast_ref::<TimestampMicrosecondArray>()
                        .ok_or_else(|| "timestamp array type mismatch".to_owned())?
                        .value(row_index),
                ),
                TimeUnit::Nanosecond => {
                    let nanos = i128::from(
                        array
                            .as_any()
                            .downcast_ref::<arrow::array::TimestampNanosecondArray>()
                            .ok_or_else(|| "timestamp array type mismatch".to_owned())?
                            .value(row_index),
                    );
                    if nanos.rem_euclid(1000) != 0 {
                        return Err(
                            "timestamp precision finer than one microsecond is unsupported".into(),
                        );
                    }
                    nanos / 1000
                }
            };
            let offset_micros = if timezone.is_none() {
                i128::from(naive_offset_minutes) * 60_000_000
            } else {
                0
            };
            let utc_micros = raw - offset_micros;
            let utc_micros = i64::try_from(utc_micros)
                .map_err(|_| "timestamp is outside the supported range".to_owned())?;
            DateTime::from_timestamp_micros(utc_micros)
                .ok_or_else(|| "timestamp is outside the supported range".into())
        }
        other => Err(format!(
            "timestamp column must be a timestamp or string, found {other:?}"
        )),
    }
}

fn arrow_string(array: &dyn Array, row_index: usize) -> Result<String, String> {
    if array.is_null(row_index) {
        return Err("mapped text field is null".into());
    }
    match array.data_type() {
        DataType::Utf8 => Ok(array
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| "UTF-8 array type mismatch".to_owned())?
            .value(row_index)
            .to_owned()),
        DataType::LargeUtf8 => Ok(array
            .as_any()
            .downcast_ref::<arrow::array::LargeStringArray>()
            .ok_or_else(|| "large UTF-8 array type mismatch".to_owned())?
            .value(row_index)
            .to_owned()),
        other => Err(format!("mapped text field has unsupported type {other:?}")),
    }
}

fn arrow_number(array: &dyn Array, row_index: usize, label: &str) -> Result<f64, String> {
    if array.is_null(row_index) {
        return Err(format!("{label} is null"));
    }
    let value = match array.data_type() {
        DataType::Float64 => array
            .as_any()
            .downcast_ref::<Float64Array>()
            .ok_or_else(|| "float64 array type mismatch".to_owned())?
            .value(row_index),
        DataType::Float32 => f64::from(
            array
                .as_any()
                .downcast_ref::<Float32Array>()
                .ok_or_else(|| "float32 array type mismatch".to_owned())?
                .value(row_index),
        ),
        DataType::Int8 => f64::from(
            array
                .as_any()
                .downcast_ref::<Int8Array>()
                .ok_or_else(|| "int8 array type mismatch".to_owned())?
                .value(row_index),
        ),
        DataType::Int16 => f64::from(
            array
                .as_any()
                .downcast_ref::<Int16Array>()
                .ok_or_else(|| "int16 array type mismatch".to_owned())?
                .value(row_index),
        ),
        DataType::Int32 => f64::from(
            array
                .as_any()
                .downcast_ref::<Int32Array>()
                .ok_or_else(|| "int32 array type mismatch".to_owned())?
                .value(row_index),
        ),
        DataType::Int64 => array
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| "int64 array type mismatch".to_owned())?
            .value(row_index) as f64,
        DataType::UInt8 => f64::from(
            array
                .as_any()
                .downcast_ref::<UInt8Array>()
                .ok_or_else(|| "uint8 array type mismatch".to_owned())?
                .value(row_index),
        ),
        DataType::UInt16 => f64::from(
            array
                .as_any()
                .downcast_ref::<UInt16Array>()
                .ok_or_else(|| "uint16 array type mismatch".to_owned())?
                .value(row_index),
        ),
        DataType::UInt32 => f64::from(
            array
                .as_any()
                .downcast_ref::<UInt32Array>()
                .ok_or_else(|| "uint32 array type mismatch".to_owned())?
                .value(row_index),
        ),
        DataType::UInt64 => array
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or_else(|| "uint64 array type mismatch".to_owned())?
            .value(row_index) as f64,
        DataType::Decimal128(_, _) => array
            .as_any()
            .downcast_ref::<Decimal128Array>()
            .ok_or_else(|| "decimal128 array type mismatch".to_owned())?
            .value_as_string(row_index)
            .parse::<f64>()
            .map_err(|_| format!("{label} decimal128 value cannot be represented as a number"))?,
        DataType::Decimal256(_, _) => array
            .as_any()
            .downcast_ref::<Decimal256Array>()
            .ok_or_else(|| "decimal256 array type mismatch".to_owned())?
            .value_as_string(row_index)
            .parse::<f64>()
            .map_err(|_| format!("{label} decimal256 value cannot be represented as a number"))?,
        other => return Err(format!("{label} has unsupported numeric type {other:?}")),
    };
    if !value.is_finite() {
        return Err(format!("{label} must be finite"));
    }
    Ok(value)
}

fn parquet_cell_as_text(array: &dyn Array, row_index: usize) -> String {
    if array.is_null(row_index) {
        return String::new();
    }
    if let Ok(value) = arrow_string(array, row_index) {
        return value;
    }
    if let Ok(value) = arrow_number(array, row_index, "value") {
        return value.to_string();
    }
    match array.data_type() {
        DataType::Timestamp(_, _) => arrow_timestamp(array, row_index, 0)
            .map(|value| value.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
            .unwrap_or_default(),
        other => format!("<unsupported {other:?}>"),
    }
}

fn write_canonical_parquet(
    database: &Connection,
    path: &Path,
    cancel: &AtomicBool,
) -> Result<String, DataGroundError> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("symbol", DataType::Utf8, false),
        Field::new("timeframe", DataType::Utf8, false),
        Field::new(
            "open_time_utc",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            false,
        ),
        Field::new(
            "close_time_utc",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            false,
        ),
        Field::new(
            "available_at_utc",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            false,
        ),
        Field::new("open", DataType::Float64, false),
        Field::new("high", DataType::Float64, false),
        Field::new("low", DataType::Float64, false),
        Field::new("close", DataType::Float64, false),
        Field::new("volume", DataType::Float64, false),
    ]));
    let file = File::create(path)?;
    let mut writer = ArrowWriter::try_new(file, Arc::clone(&schema), None)?;
    let mut statement = database.prepare(
        "SELECT symbol, timeframe, open_time_utc_us, close_time_utc_us,
                available_at_utc_us, open, high, low, close, volume
         FROM bars ORDER BY symbol, timeframe, open_time_utc_us",
    )?;
    let mut rows = statement.query([])?;
    let mut hasher = Sha256::new();
    let mut symbols = Vec::with_capacity(IMPORT_BATCH_ROWS);
    let mut timeframes = Vec::with_capacity(IMPORT_BATCH_ROWS);
    let mut open_times = Vec::with_capacity(IMPORT_BATCH_ROWS);
    let mut close_times = Vec::with_capacity(IMPORT_BATCH_ROWS);
    let mut available_times = Vec::with_capacity(IMPORT_BATCH_ROWS);
    let mut opens = Vec::with_capacity(IMPORT_BATCH_ROWS);
    let mut highs = Vec::with_capacity(IMPORT_BATCH_ROWS);
    let mut lows = Vec::with_capacity(IMPORT_BATCH_ROWS);
    let mut closes = Vec::with_capacity(IMPORT_BATCH_ROWS);
    let mut volumes = Vec::with_capacity(IMPORT_BATCH_ROWS);
    while let Some(row) = rows.next()? {
        check_cancelled_ref(cancel)?;
        let symbol: String = row.get(0)?;
        let timeframe: String = row.get(1)?;
        let open_time: i64 = row.get(2)?;
        let close_time: i64 = row.get(3)?;
        let available_at: i64 = row.get(4)?;
        let open: f64 = row.get(5)?;
        let high: f64 = row.get(6)?;
        let low: f64 = row.get(7)?;
        let close: f64 = row.get(8)?;
        let volume: f64 = row.get(9)?;
        hash_canonical_field(&mut hasher, symbol.as_bytes());
        hash_canonical_field(&mut hasher, timeframe.as_bytes());
        hasher.update(open_time.to_be_bytes());
        hasher.update(close_time.to_be_bytes());
        hasher.update(available_at.to_be_bytes());
        hasher.update(open.to_bits().to_be_bytes());
        hasher.update(high.to_bits().to_be_bytes());
        hasher.update(low.to_bits().to_be_bytes());
        hasher.update(close.to_bits().to_be_bytes());
        hasher.update(volume.to_bits().to_be_bytes());
        symbols.push(symbol);
        timeframes.push(timeframe);
        open_times.push(open_time);
        close_times.push(close_time);
        available_times.push(available_at);
        opens.push(open);
        highs.push(high);
        lows.push(low);
        closes.push(close);
        volumes.push(volume);
        if symbols.len() >= IMPORT_BATCH_ROWS {
            flush_arrow_batch(
                &mut writer,
                Arc::clone(&schema),
                &mut symbols,
                &mut timeframes,
                &mut open_times,
                &mut close_times,
                &mut available_times,
                &mut opens,
                &mut highs,
                &mut lows,
                &mut closes,
                &mut volumes,
            )?;
        }
    }
    if !symbols.is_empty() {
        flush_arrow_batch(
            &mut writer,
            schema,
            &mut symbols,
            &mut timeframes,
            &mut open_times,
            &mut close_times,
            &mut available_times,
            &mut opens,
            &mut highs,
            &mut lows,
            &mut closes,
            &mut volumes,
        )?;
    }
    writer.close()?;
    Ok(hex_digest(hasher.finalize()))
}

#[allow(clippy::too_many_arguments)]
fn flush_arrow_batch(
    writer: &mut ArrowWriter<File>,
    schema: Arc<Schema>,
    symbols: &mut Vec<String>,
    timeframes: &mut Vec<String>,
    open_times: &mut Vec<i64>,
    close_times: &mut Vec<i64>,
    available_times: &mut Vec<i64>,
    opens: &mut Vec<f64>,
    highs: &mut Vec<f64>,
    lows: &mut Vec<f64>,
    closes: &mut Vec<f64>,
    volumes: &mut Vec<f64>,
) -> Result<(), DataGroundError> {
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from(std::mem::take(symbols))),
            Arc::new(StringArray::from(std::mem::take(timeframes))),
            Arc::new(
                TimestampMicrosecondArray::from(std::mem::take(open_times)).with_timezone("UTC"),
            ),
            Arc::new(
                TimestampMicrosecondArray::from(std::mem::take(close_times)).with_timezone("UTC"),
            ),
            Arc::new(
                TimestampMicrosecondArray::from(std::mem::take(available_times))
                    .with_timezone("UTC"),
            ),
            Arc::new(Float64Array::from(std::mem::take(opens))),
            Arc::new(Float64Array::from(std::mem::take(highs))),
            Arc::new(Float64Array::from(std::mem::take(lows))),
            Arc::new(Float64Array::from(std::mem::take(closes))),
            Arc::new(Float64Array::from(std::mem::take(volumes))),
        ],
    )
    .map_err(|error| DataGroundError::Schema(error.to_string()))?;
    writer.write(&batch)?;
    *symbols = Vec::with_capacity(IMPORT_BATCH_ROWS);
    *timeframes = Vec::with_capacity(IMPORT_BATCH_ROWS);
    *open_times = Vec::with_capacity(IMPORT_BATCH_ROWS);
    *close_times = Vec::with_capacity(IMPORT_BATCH_ROWS);
    *available_times = Vec::with_capacity(IMPORT_BATCH_ROWS);
    *opens = Vec::with_capacity(IMPORT_BATCH_ROWS);
    *highs = Vec::with_capacity(IMPORT_BATCH_ROWS);
    *lows = Vec::with_capacity(IMPORT_BATCH_ROWS);
    *closes = Vec::with_capacity(IMPORT_BATCH_ROWS);
    *volumes = Vec::with_capacity(IMPORT_BATCH_ROWS);
    Ok(())
}

fn hash_canonical_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

struct MappingIndices {
    timestamp: usize,
    open: usize,
    high: usize,
    low: usize,
    close: usize,
    volume: usize,
    symbol: Option<usize>,
    timeframe: Option<usize>,
}

struct NumericBarInput<'a> {
    row_number: u64,
    symbol_value: &'a str,
    timeframe_value: &'a str,
    timestamp: DateTime<Utc>,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
}

struct SourceBarInput<'a> {
    row_number: u64,
    symbol_value: &'a str,
    timeframe_value: &'a str,
    timestamp_value: &'a str,
    open_value: &'a str,
    high_value: &'a str,
    low_value: &'a str,
    close_value: &'a str,
    volume_value: &'a str,
}

fn validate_headers<'a>(headers: impl Iterator<Item = &'a str>) -> Result<(), DataGroundError> {
    let values: Vec<_> = headers.collect();
    if values.is_empty()
        || values.iter().any(|value| value.is_empty())
        || values
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != values.len()
    {
        return Err(DataGroundError::Schema(
            "source fields must be non-empty and unique".into(),
        ));
    }
    Ok(())
}

fn resolve_mapping(
    headers: &csv::StringRecord,
    mapping: &FieldMapping,
) -> Result<MappingIndices, DataGroundError> {
    let index = |name: &str| {
        headers
            .iter()
            .position(|header| header == name)
            .ok_or_else(|| DataGroundError::Schema(format!("mapped field not found: {name}")))
    };
    Ok(MappingIndices {
        timestamp: index(&mapping.timestamp)?,
        open: index(&mapping.open)?,
        high: index(&mapping.high)?,
        low: index(&mapping.low)?,
        close: index(&mapping.close)?,
        volume: index(&mapping.volume)?,
        symbol: mapping.symbol.as_deref().map(index).transpose()?,
        timeframe: mapping.timeframe.as_deref().map(index).transpose()?,
    })
}

fn parse_source_bar(
    values: SourceBarInput<'_>,
    request: &ImportRequest,
) -> Result<OhlcvBar, DataGroundError> {
    let SourceBarInput {
        row_number,
        symbol_value,
        timeframe_value,
        timestamp_value,
        open_value,
        high_value,
        low_value,
        close_value,
        volume_value,
    } = values;
    let invalid = |reason: String| DataGroundError::InvalidRow {
        row: row_number,
        reason,
    };
    let timestamp = parse_source_timestamp(timestamp_value, request.naive_timestamp_offset_minutes)
        .map_err(|error| invalid(error.to_string()))?;
    let open = parse_number(open_value, "open").map_err(invalid)?;
    let high = parse_number(high_value, "high").map_err(invalid)?;
    let low = parse_number(low_value, "low").map_err(invalid)?;
    let close = parse_number(close_value, "close").map_err(invalid)?;
    let volume = parse_number(volume_value, "volume").map_err(invalid)?;
    parse_numeric_bar(
        NumericBarInput {
            row_number,
            symbol_value,
            timeframe_value,
            timestamp,
            open,
            high,
            low,
            close,
            volume,
        },
        request,
    )
}

fn parse_source_timestamp(
    value: &str,
    offset_minutes: i16,
) -> Result<DateTime<Utc>, DataGroundError> {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Ok(parsed.with_timezone(&Utc));
    }
    let naive = [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ]
    .iter()
    .find_map(|format| NaiveDateTime::parse_from_str(value, format).ok())
    .ok_or_else(|| {
        DataGroundError::Time(format!(
            "timestamp needs an explicit offset or supported local format: {value}"
        ))
    })?;
    let offset = FixedOffset::east_opt(i32::from(offset_minutes) * 60)
        .ok_or_else(|| DataGroundError::Time("invalid fixed UTC offset".into()))?;
    offset
        .from_local_datetime(&naive)
        .single()
        .map(|time| time.with_timezone(&Utc))
        .ok_or_else(|| {
            DataGroundError::Time("timestamp is ambiguous in the selected fixed offset".into())
        })
}

fn parse_number(value: &str, field: &str) -> Result<f64, String> {
    let parsed = value
        .trim()
        .parse::<f64>()
        .map_err(|_| format!("{field} is not numeric"))?;
    if !parsed.is_finite() {
        return Err(format!("{field} must be finite"));
    }
    Ok(parsed)
}

fn insert_bar(
    database: &Connection,
    context: &mut StageContext,
    bar: OhlcvBar,
    row_number: u64,
) -> Result<(), DataGroundError> {
    let key = (bar.symbol.clone(), bar.timeframe);
    let open_us = bar.open_time_utc.timestamp_micros();
    if let Some(previous_open_us) = context.last_by_series.get(&key).copied() {
        if open_us == previous_open_us {
            context.audit.duplicate_bars += 1;
            return Err(DataGroundError::DuplicateBar {
                symbol: bar.symbol,
                timeframe: bar.timeframe.as_str().into(),
                open_time_utc: format_utc(open_us),
            });
        }
        if open_us < previous_open_us {
            context.audit.descending_bars += 1;
            return Err(DataGroundError::OutOfOrderBar {
                symbol: bar.symbol,
                timeframe: bar.timeframe.as_str().into(),
                open_time_utc: format_utc(open_us),
            });
        }
        record_gap(
            context,
            &bar.symbol,
            bar.timeframe,
            previous_open_us,
            open_us,
        )?;
    }
    if let Some(limit) = context.request.quotas.max_records {
        if context.row_count >= limit {
            return Err(DataGroundError::ResourceQuota(format!(
                "source exceeded the configured record quota of {limit}"
            )));
        }
    }
    let series = context.series.entry(key.clone()).or_default();
    if let Some(existing) = series.role {
        if existing != bar.role {
            return Err(DataGroundError::InvalidRow {
                row: row_number,
                reason: "a symbol cannot change market role within a dataset".into(),
            });
        }
    } else {
        series.role = Some(bar.role);
    }
    series.bar_count += 1;
    series.first_open.get_or_insert(open_us);
    series.last_open = Some(open_us);
    let result = database.execute(
        "INSERT INTO bars(
            symbol, timeframe, role, open_time_utc_us, close_time_utc_us,
            available_at_utc_us, open, high, low, close, volume
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            bar.symbol,
            bar.timeframe.as_str(),
            bar.role.as_str(),
            open_us,
            bar.close_time_utc.timestamp_micros(),
            bar.available_at_utc.timestamp_micros(),
            bar.open,
            bar.high,
            bar.low,
            bar.close,
            bar.volume
        ],
    );
    if let Err(error) = result {
        if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
            context.audit.duplicate_bars += 1;
            return Err(DataGroundError::DuplicateBar {
                symbol: bar.symbol,
                timeframe: bar.timeframe.as_str().into(),
                open_time_utc: format_utc(open_us),
            });
        }
        return Err(error.into());
    }
    context.last_by_series.insert(key, open_us);
    context.row_count += 1;
    Ok(())
}

fn record_gap(
    context: &mut StageContext,
    symbol: &str,
    timeframe: Timeframe,
    previous_open_us: i64,
    next_open_us: i64,
) -> Result<(), DataGroundError> {
    let step = timeframe.duration_micros();
    let delta = next_open_us - previous_open_us;
    if delta % step != 0 {
        return Err(DataGroundError::InvalidRequest(format!(
            "non-integral interval between {} {} bars",
            symbol,
            timeframe.as_str()
        )));
    }
    let expected = (delta / step - 1).max(0) as u64;
    if expected == 0 {
        return Ok(());
    }
    let (closed, missing, unclassified) = match &context.request.calendar {
        CalendarPolicy::Unknown => (0, 0, expected),
        CalendarPolicy::Continuous => (0, expected, 0),
        CalendarPolicy::WeekendsUtc => {
            let closed = count_weekend_grid_points(previous_open_us + step, expected, step);
            (closed, expected - closed, 0)
        }
        CalendarPolicy::CustomClosedIntervals { intervals } => {
            let mut closed = 0u64;
            for interval in intervals {
                let start = parse_rfc3339_utc(&interval.start_utc)?.timestamp_micros();
                let end = parse_rfc3339_utc(&interval.end_utc)?.timestamp_micros();
                closed = closed.saturating_add(count_grid_points_in_interval(
                    previous_open_us + step,
                    expected,
                    step,
                    start,
                    end,
                ));
            }
            let closed = closed.min(expected);
            (closed, expected - closed, 0)
        }
    };
    context.audit.closed_session_bars += closed;
    context.audit.missing_bars += missing;
    context.audit.unclassified_gap_bars += unclassified;
    if context.audit.gaps.len() < MAX_GAP_SAMPLES {
        context.audit.gaps.push(GapSample {
            symbol: symbol.to_owned(),
            timeframe: timeframe.as_str().into(),
            first_missing_open_utc: format_utc(previous_open_us + step),
            next_observed_open_utc: format_utc(next_open_us),
            expected_missing_bars: expected,
            closed_session_bars: closed,
            missing_bars: missing,
            unclassified_bars: unclassified,
        });
    }
    let series = context
        .series
        .entry((symbol.to_owned(), timeframe))
        .or_default();
    series.closed_session_bars += closed;
    series.missing_bars += missing;
    series.unclassified_gap_bars += unclassified;
    Ok(())
}

fn count_weekend_grid_points(first_us: i64, count: u64, step_us: i64) -> u64 {
    let weekly_us = 7 * 86_400_000_000i64;
    if count == 0 || step_us <= 0 || weekly_us % step_us != 0 {
        return 0;
    }
    let per_week = (weekly_us / step_us) as u64;
    let whole_weeks = count / per_week;
    let remaining = count % per_week;
    let mut weekends_per_week = 0u64;
    for index in 0..per_week {
        let timestamp = first_us.saturating_add((index as i64).saturating_mul(step_us));
        if DateTime::from_timestamp_micros(timestamp)
            .is_some_and(|time| matches!(time.weekday(), Weekday::Sat | Weekday::Sun))
        {
            weekends_per_week += 1;
        }
    }
    let weekend_tail = (0..remaining)
        .filter(|index| {
            let timestamp = first_us.saturating_add((*index as i64).saturating_mul(step_us));
            DateTime::from_timestamp_micros(timestamp)
                .is_some_and(|time| matches!(time.weekday(), Weekday::Sat | Weekday::Sun))
        })
        .count() as u64;
    whole_weeks * weekends_per_week + weekend_tail
}

fn count_grid_points_in_interval(
    first_us: i64,
    count: u64,
    step_us: i64,
    interval_start_us: i64,
    interval_end_us: i64,
) -> u64 {
    if count == 0 || step_us <= 0 || interval_start_us >= interval_end_us {
        return 0;
    }
    let first_index = if interval_start_us <= first_us {
        0
    } else {
        ((interval_start_us - first_us) + step_us - 1) / step_us
    };
    let last_exclusive_index = if interval_end_us <= first_us {
        0
    } else {
        ((interval_end_us - first_us) + step_us - 1) / step_us
    };
    let start = first_index.max(0) as u64;
    let end = (last_exclusive_index.max(0) as u64).min(count);
    end.saturating_sub(start.min(end))
}

fn series_summaries(
    series: &BTreeMap<(String, Timeframe), SeriesAccumulator>,
) -> Result<Vec<SeriesSummary>, DataGroundError> {
    series
        .iter()
        .map(|((symbol, timeframe), summary)| {
            Ok(SeriesSummary {
                symbol: symbol.clone(),
                timeframe: timeframe.as_str().into(),
                market_role: summary
                    .role
                    .ok_or_else(|| {
                        DataGroundError::Schema(format!(
                            "market role missing for {symbol} {}",
                            timeframe.as_str()
                        ))
                    })?
                    .as_str()
                    .into(),
                bar_count: summary.bar_count,
                first_open_utc: format_utc(
                    summary
                        .first_open
                        .ok_or_else(|| DataGroundError::Schema("series has no first bar".into()))?,
                ),
                last_open_utc: format_utc(
                    summary
                        .last_open
                        .ok_or_else(|| DataGroundError::Schema("series has no last bar".into()))?,
                ),
                missing_bars: summary.missing_bars,
                closed_session_bars: summary.closed_session_bars,
                unclassified_gap_bars: summary.unclassified_gap_bars,
            })
        })
        .collect()
}

fn format_utc(microseconds: i64) -> String {
    DateTime::from_timestamp_micros(microseconds)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

fn check_cancelled(cancel: &Arc<AtomicBool>) -> Result<(), DataGroundError> {
    check_cancelled_ref(cancel)
}

fn check_cancelled_ref(cancel: &AtomicBool) -> Result<(), DataGroundError> {
    if cancel.load(Ordering::Relaxed) {
        Err(DataGroundError::Cancelled)
    } else {
        Ok(())
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex_digest(Sha256::digest(bytes))
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::ArrayRef;
    use arrow::array::{Float64Array, StringArray, TimestampMicrosecondArray};
    use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
    use parquet::arrow::ArrowWriter;
    use proptest::prelude::*;
    use std::{fs, sync::atomic::AtomicBool};
    use tempfile::TempDir;

    const FIXTURE: &str = include_str!("../fixtures/eurusd-gbpusd-h1-h4.csv");
    const STANDARD_HEADER: &str = "timestamp,symbol,timeframe,open,high,low,close,volume\n";

    fn request(calendar: CalendarPolicy) -> ImportRequest {
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
            market_roles: BTreeMap::from([
                ("EURUSD.m".into(), MarketRole::Target),
                ("GBPUSD.m".into(), MarketRole::Reference),
            ]),
            source_id: "fixture-source-v1".into(),
            broker: "FIXTURE".into(),
            server: "FIXTURE-SERVER".into(),
            delimiter: CsvDelimiter::Comma,
            calendar,
            quotas: ResourceQuotas {
                max_source_bytes: Some(10_000_000),
                max_records: Some(10_000),
            },
        }
    }

    fn write_source(directory: &TempDir, name: &str, contents: &str) -> PathBuf {
        let path = directory.path().join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn source_inspection_respects_configured_csv_delimiter() {
        let directory = TempDir::new().unwrap();
        let source = write_source(
            &directory,
            "semicolon.csv",
            "timestamp;open;high;low;close;volume\n2024-01-08T00:00:00Z;1.1;1.2;1.0;1.15;4\n",
        );

        let inspection =
            inspect_source_with_limit(&source, 1_000, CsvDelimiter::Semicolon).unwrap();

        assert_eq!(
            inspection.headers,
            ["timestamp", "open", "high", "low", "close", "volume"]
        );
        assert_eq!(inspection.sample_rows[0]["open"], "1.1");
        assert_eq!(inspection.sample_rows[0]["volume"], "4");
    }

    fn prepare_csv(
        directory: &TempDir,
        contents: &str,
        request: ImportRequest,
    ) -> Result<PreparedDataset, DataGroundError> {
        let path = write_source(directory, "input.csv", contents);
        prepare_dataset(
            path,
            directory.path().join("staging"),
            request,
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn write_parquet_fixture(path: &Path, decimal_numeric: bool) {
        let numeric_type = if decimal_numeric {
            DataType::Decimal128(12, 5)
        } else {
            DataType::Float64
        };
        let schema = Arc::new(Schema::new(vec![
            Field::new(
                "timestamp",
                DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
                false,
            ),
            Field::new("symbol", DataType::Utf8, false),
            Field::new("timeframe", DataType::Utf8, false),
            Field::new("open", numeric_type.clone(), false),
            Field::new("high", numeric_type.clone(), false),
            Field::new("low", numeric_type.clone(), false),
            Field::new("close", numeric_type.clone(), false),
            Field::new("volume", numeric_type, false),
        ]));
        let mut reader = csv::Reader::from_reader(FIXTURE.as_bytes());
        let mut timestamps = Vec::new();
        let mut symbols = Vec::new();
        let mut timeframes = Vec::new();
        let mut numeric: [Vec<f64>; 5] = std::array::from_fn(|_| Vec::new());
        for row in reader.deserialize::<BTreeMap<String, String>>() {
            let row = row.unwrap();
            timestamps.push(
                DateTime::parse_from_rfc3339(&row["timestamp"])
                    .unwrap()
                    .timestamp_micros(),
            );
            symbols.push(row["symbol"].clone());
            timeframes.push(row["timeframe"].clone());
            for (index, field) in ["open", "high", "low", "close", "volume"]
                .iter()
                .enumerate()
            {
                numeric[index].push(row[*field].parse().unwrap());
            }
        }
        let numeric_columns = numeric
            .into_iter()
            .map(|values| {
                if decimal_numeric {
                    let scaled = values
                        .into_iter()
                        .map(|value| (value * 100_000.0).round() as i128);
                    Arc::new(
                        Decimal128Array::from_iter_values(scaled)
                            .with_precision_and_scale(12, 5)
                            .unwrap(),
                    ) as ArrayRef
                } else {
                    Arc::new(Float64Array::from(values)) as ArrayRef
                }
            })
            .collect::<Vec<_>>();
        let mut columns = vec![
            Arc::new(TimestampMicrosecondArray::from(timestamps).with_timezone("UTC")) as ArrayRef,
            Arc::new(StringArray::from(symbols)) as ArrayRef,
            Arc::new(StringArray::from(timeframes)) as ArrayRef,
        ];
        columns.extend(numeric_columns);
        let batch = RecordBatch::try_new(Arc::clone(&schema), columns).unwrap();
        let file = File::create(path).unwrap();
        let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    }

    type CanonicalRow = (String, String, i64, i64, i64, f64, f64, f64, f64, f64);

    fn canonical_rows(path: &Path) -> Vec<CanonicalRow> {
        let file = File::open(path).unwrap();
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)
            .unwrap()
            .with_batch_size(IMPORT_BATCH_ROWS)
            .build()
            .unwrap();
        let mut rows = Vec::new();
        for batch in reader {
            let batch = batch.unwrap();
            let symbols = batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            let timeframes = batch
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            let opens = batch
                .column(2)
                .as_any()
                .downcast_ref::<TimestampMicrosecondArray>()
                .unwrap();
            let closes = batch
                .column(3)
                .as_any()
                .downcast_ref::<TimestampMicrosecondArray>()
                .unwrap();
            let available = batch
                .column(4)
                .as_any()
                .downcast_ref::<TimestampMicrosecondArray>()
                .unwrap();
            let o = batch
                .column(5)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap();
            let h = batch
                .column(6)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap();
            let l = batch
                .column(7)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap();
            let c = batch
                .column(8)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap();
            let v = batch
                .column(9)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap();
            for index in 0..batch.num_rows() {
                rows.push((
                    symbols.value(index).to_owned(),
                    timeframes.value(index).to_owned(),
                    opens.value(index),
                    closes.value(index),
                    available.value(index),
                    o.value(index),
                    h.value(index),
                    l.value(index),
                    c.value(index),
                    v.value(index),
                ));
            }
        }
        rows
    }

    #[test]
    fn csv_ingestion_audits_arbitrary_markets_and_timeframes_without_filling_gaps() {
        let directory = TempDir::new().unwrap();
        let prepared =
            prepare_csv(&directory, FIXTURE, request(CalendarPolicy::WeekendsUtc)).unwrap();
        let manifest = &prepared.manifest;
        assert_eq!(manifest.bar_count, 5);
        assert_eq!(manifest.series.len(), 3);
        assert_eq!(manifest.status, DatasetStatus::Blocked);
        assert_eq!(manifest.quality_audit.duplicate_bars, 0);
        assert_eq!(manifest.quality_audit.missing_bars, 2);
        assert_eq!(manifest.quality_audit.closed_session_bars, 46);
        assert_eq!(manifest.quality_audit.unclassified_gap_bars, 0);
        assert_eq!(manifest.quality_audit.gaps[0].expected_missing_bars, 48);
        assert_eq!(manifest.series[0].market_role, "TARGET");
        assert_eq!(manifest.series[2].market_role, "REFERENCE");
        assert_eq!(prepared.preview.canonical_bars_fingerprint.len(), 64);
        let canonical = ParquetRecordBatchReaderBuilder::try_new(
            File::open(prepared.staging_directory.join("bars.parquet")).unwrap(),
        )
        .unwrap();
        assert!(canonical
            .schema()
            .fields()
            .iter()
            .any(|field| field.name() == "available_at_utc"));
        assert_eq!(
            fs::read(prepared.staging_directory.join("raw-source.csv")).unwrap(),
            FIXTURE.as_bytes()
        );
    }

    #[test]
    fn parquet_parser_produces_the_same_canonical_rows_as_the_csv_parser() {
        let directory = TempDir::new().unwrap();
        let csv = prepare_csv(&directory, FIXTURE, request(CalendarPolicy::WeekendsUtc)).unwrap();
        let parquet_source = directory.path().join("equivalent.parquet");
        write_parquet_fixture(&parquet_source, false);
        let parquet = prepare_dataset(
            &parquet_source,
            directory.path().join("parquet-staging"),
            request(CalendarPolicy::WeekendsUtc),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let csv_rows = canonical_rows(&csv.staging_directory.join("bars.parquet"));
        let parquet_rows = canonical_rows(&parquet.staging_directory.join("bars.parquet"));
        assert_eq!(parquet_rows, csv_rows);
        assert_eq!(
            parquet.manifest.canonical_bars_fingerprint,
            csv.manifest.canonical_bars_fingerprint
        );
        assert_ne!(parquet.preview.raw_sha256, csv.preview.raw_sha256);
        assert_ne!(parquet.preview.dataset_id, csv.preview.dataset_id);

        let decimal_source = directory.path().join("decimal.parquet");
        write_parquet_fixture(&decimal_source, true);
        let decimal = prepare_dataset(
            &decimal_source,
            directory.path().join("decimal-staging"),
            request(CalendarPolicy::WeekendsUtc),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert_eq!(
            canonical_rows(&decimal.staging_directory.join("bars.parquet")),
            csv_rows
        );
        assert_eq!(
            decimal.manifest.canonical_bars_fingerprint,
            csv.manifest.canonical_bars_fingerprint
        );
    }

    #[test]
    fn duplicate_descending_corrupt_and_missing_rows_fail_closed() {
        let duplicate = format!("{STANDARD_HEADER}2024-01-08T00:00:00Z,EURUSD.m,H1,1,2,0.5,1.5,1\n2024-01-08T00:00:00Z,EURUSD.m,H1,1,2,0.5,1.5,1\n");
        let directory = TempDir::new().unwrap();
        assert!(matches!(
            prepare_csv(&directory, &duplicate, request(CalendarPolicy::Unknown)),
            Err(DataGroundError::DuplicateBar { .. })
        ));

        let descending = format!("{STANDARD_HEADER}2024-01-08T02:00:00Z,EURUSD.m,H1,1,2,0.5,1.5,1\n2024-01-08T01:00:00Z,EURUSD.m,H1,1,2,0.5,1.5,1\n");
        assert!(matches!(
            prepare_csv(&directory, &descending, request(CalendarPolicy::Unknown)),
            Err(DataGroundError::OutOfOrderBar { .. })
        ));

        for row in [
            "2024-01-08T00:00:00Z,EURUSD.m,H1,1,0.9,0.5,0.8,1\n",
            "2024-01-08T00:00:00Z,EURUSD.m,H1,1,2,0.5,1.5,-1\n",
            "2024-01-08T00:00:00Z,EURUSD.m,H1,1,2,0.5,NaN,1\n",
            "2024-01-08T00:00:00Z,EURUSD.m,H1,1,2,0.5,1.5,Infinity\n",
            "2024-01-08T00:00:00Z, EURUSD.m,H1,1,2,0.5,1.5,1\n",
        ] {
            assert!(
                matches!(
                    prepare_csv(
                        &directory,
                        &format!("{STANDARD_HEADER}{row}"),
                        request(CalendarPolicy::Unknown)
                    ),
                    Err(DataGroundError::InvalidRow { .. })
                ),
                "row should be rejected: {row}"
            );
        }

        let gap = format!("{STANDARD_HEADER}2024-01-08T00:00:00Z,EURUSD.m,H1,1,2,0.5,1.5,1\n2024-01-08T05:00:00Z,EURUSD.m,H1,1,2,0.5,1.5,1\n");
        let blocked = prepare_csv(&directory, &gap, request(CalendarPolicy::Unknown)).unwrap();
        assert_eq!(blocked.manifest.bar_count, 2);
        assert_eq!(blocked.manifest.quality_audit.unclassified_gap_bars, 4);
        assert_eq!(blocked.manifest.status, DatasetStatus::Blocked);
    }

    #[test]
    fn custom_market_closures_are_separated_from_missing_bars() {
        let directory = TempDir::new().unwrap();
        let gap = format!("{STANDARD_HEADER}2024-01-08T00:00:00Z,EURUSD.m,H1,1,2,0.5,1.5,1\n2024-01-08T05:00:00Z,EURUSD.m,H1,1,2,0.5,1.5,1\n");
        let mut config = request(CalendarPolicy::CustomClosedIntervals {
            intervals: vec![ClosedInterval {
                start_utc: "2024-01-08T01:00:00Z".into(),
                end_utc: "2024-01-08T04:00:00Z".into(),
            }],
        });
        let prepared = prepare_csv(&directory, &gap, config.clone()).unwrap();
        assert_eq!(prepared.manifest.quality_audit.closed_session_bars, 3);
        assert_eq!(prepared.manifest.quality_audit.missing_bars, 1);
        assert_eq!(prepared.manifest.quality_audit.unclassified_gap_bars, 0);
        assert_eq!(prepared.manifest.status, DatasetStatus::Blocked);

        config.calendar = CalendarPolicy::CustomClosedIntervals {
            intervals: vec![
                ClosedInterval {
                    start_utc: "2024-01-08T01:00:00Z".into(),
                    end_utc: "2024-01-08T03:00:00Z".into(),
                },
                ClosedInterval {
                    start_utc: "2024-01-08T02:00:00Z".into(),
                    end_utc: "2024-01-08T04:00:00Z".into(),
                },
            ],
        };
        assert!(matches!(
            config.validate(),
            Err(DataGroundError::InvalidRequest(_))
        ));
    }

    #[test]
    fn explicit_timezone_and_close_timestamp_semantics_produce_no_lookahead() {
        let directory = TempDir::new().unwrap();
        let mut config = request(CalendarPolicy::Continuous);
        config.naive_timestamp_offset_minutes = 480;
        let local = format!("{STANDARD_HEADER}2024-01-08 10:00:00,EURUSD.m,H1,1,2,0.5,1.5,1\n");
        let prepared = prepare_csv(&directory, &local, config).unwrap();
        let open =
            DateTime::parse_from_rfc3339(&prepared.manifest.series[0].first_open_utc).unwrap();
        assert_eq!(
            open.with_timezone(&Utc).to_rfc3339(),
            "2024-01-08T02:00:00+00:00"
        );

        let mut close_time = request(CalendarPolicy::Continuous);
        close_time.timestamp_semantics = TimestampSemantics::BarClosesAt;
        let input = format!("{STANDARD_HEADER}2024-01-08T01:00:00Z,EURUSD.m,H1,1,2,0.5,1.5,1\n");
        let closed = prepare_csv(&directory, &input, close_time).unwrap();
        let rows = canonical_rows(&closed.staging_directory.join("bars.parquet"));
        let row = &rows[0];
        assert_eq!(
            row.2,
            DateTime::parse_from_rfc3339("2024-01-08T00:00:00Z")
                .unwrap()
                .timestamp_micros()
        );
        assert_eq!(row.3, row.4);
        assert!(row.4 >= row.3);
    }

    #[test]
    fn schemas_paths_limits_and_cancellation_are_rejected_without_registry_state() {
        let directory = TempDir::new().unwrap();
        let source = write_source(&directory, "bad-schema.csv", "date,open\n2024-01-08,1\n");
        let mut bad_mapping = request(CalendarPolicy::Unknown);
        bad_mapping.mapping.open = "missing_open".into();
        assert!(matches!(
            prepare_dataset(
                source,
                directory.path().join("bad-schema-stage"),
                bad_mapping,
                Arc::new(AtomicBool::new(false))
            ),
            Err(DataGroundError::Schema(_))
        ));

        let unsafe_path = directory.path().join("..").join("not-allowed.csv");
        assert!(matches!(
            inspect_source(unsafe_path),
            Err(DataGroundError::UnsafeSourcePath)
        ));

        let valid_source = write_source(&directory, "symlink-target.csv", FIXTURE);
        let leaf_link = directory.path().join("linked-source.csv");
        #[cfg(windows)]
        let link_result = std::os::windows::fs::symlink_file(&valid_source, &leaf_link);
        #[cfg(unix)]
        let link_result = std::os::unix::fs::symlink(&valid_source, &leaf_link);
        link_result.expect("the test environment must allow local symlink fixtures");
        assert!(matches!(
            inspect_source(&leaf_link),
            Err(DataGroundError::UnsafeSourcePath)
        ));

        let linked_directory = directory.path().join("linked-directory");
        #[cfg(windows)]
        let directory_link_result =
            std::os::windows::fs::symlink_dir(directory.path(), &linked_directory);
        #[cfg(unix)]
        let directory_link_result = std::os::unix::fs::symlink(directory.path(), &linked_directory);
        directory_link_result.expect("the test environment must allow directory symlink fixtures");
        let linked_source = linked_directory.join("symlink-target.csv");
        assert!(matches!(
            inspect_source(&linked_source),
            Err(DataGroundError::UnsafeSourcePath)
        ));

        let mut too_small = request(CalendarPolicy::Unknown);
        too_small.quotas.max_source_bytes = Some(8);
        assert!(matches!(
            prepare_csv(&directory, FIXTURE, too_small),
            Err(DataGroundError::ResourceQuota(_))
        ));

        let cancel = Arc::new(AtomicBool::new(true));
        let source = write_source(&directory, "cancel.csv", FIXTURE);
        assert!(matches!(
            prepare_dataset(
                source,
                directory.path().join("cancel-staging"),
                request(CalendarPolicy::Unknown),
                cancel
            ),
            Err(DataGroundError::Cancelled)
        ));
        let stage = directory.path().join("cancel-staging");
        assert_eq!(fs::read_dir(stage).unwrap().count(), 0);

        let mut unbounded = request(CalendarPolicy::Unknown);
        unbounded.quotas.max_records = None;
        assert!(matches!(
            unbounded.validate(),
            Err(DataGroundError::InvalidRequest(_))
        ));

        let mut repeated_mapping = request(CalendarPolicy::Unknown);
        repeated_mapping.mapping.symbol = Some(repeated_mapping.mapping.open.clone());
        assert!(matches!(
            repeated_mapping.validate(),
            Err(DataGroundError::InvalidRequest(_))
        ));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn weekend_gap_count_is_bounded_and_periodic(
            start_hour in -500_000i64..500_000i64,
            count in 0u64..100_000u64
        ) {
            let step = Timeframe::H1.duration_micros();
            let first = start_hour * step;
            let closed = count_weekend_grid_points(first, count, step);
            prop_assert!(closed <= count);
            prop_assert_eq!(
                closed,
                count_weekend_grid_points(first + 7 * 86_400_000_000, count, step)
            );
        }

        #[test]
        fn fixed_offset_timestamp_parsing_round_trips_utc(
            minute in -840i16..=840i16,
            day in -100_000i64..100_000i64
        ) {
            let base = DateTime::from_timestamp(day * 86_400, 0).unwrap();
            let local = base + chrono::Duration::minutes(i64::from(minute));
            let value = local.format("%Y-%m-%d %H:%M:%S").to_string();
            let parsed = parse_source_timestamp(&value, minute).unwrap();
            prop_assert_eq!(parsed, base);
        }
    }
}
